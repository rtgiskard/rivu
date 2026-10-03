//! Session-bus MPRIS2 adapter. Keep one `Mpris` alive per application runtime;
//! dropping it wakes the publisher and finishes outstanding replies before closing.
//! Playback changes come from the core subscription, never a library polling loop.

use crate::{
    core::AppHandle,
    model::{AppState, Command, PlaybackStatus, RepeatMode},
};
use anyhow::{Context, Result};
use crossbeam_channel::{Sender, bounded, select_biased};
use std::{
    collections::HashMap,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};
use zbus::{
    blocking::{Connection, connection::Builder},
    fdo,
    object_server::SignalEmitter,
    zvariant::{ObjectPath, OwnedValue, Str, Value},
};

const BUS_NAME: &str = "org.mpris.MediaPlayer2.rivu";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";
const MICROS_PER_SECOND: f64 = 1_000_000.0;
type Metadata = HashMap<String, OwnedValue>;

pub struct Mpris {
    connection: Option<Connection>,
    stop: Sender<()>,
    publisher: Option<JoinHandle<()>>,
}

impl Mpris {
    pub fn start(handle: AppHandle, can_raise: bool) -> Result<Self> {
        let updates = handle.subscribe();
        let initial = handle.snapshot();
        // Builder uses DoNotQueue: another Rivu must fail, not silently wait for
        // the name or replace the running player's controls.
        let connection = Builder::session()
            .context("Connecting MPRIS to the session bus")?
            .name(BUS_NAME)?
            .allow_name_replacements(false)
            .replace_existing_names(false)
            .method_timeout(Duration::from_secs(5))
            .serve_at(
                OBJECT_PATH,
                Root {
                    handle: handle.clone(),
                    can_raise,
                },
            )?
            .serve_at(
                OBJECT_PATH,
                Player {
                    handle: handle.clone(),
                },
            )?
            .build()
            .context("Registering org.mpris.MediaPlayer2.rivu")?;
        let (stop, stopped) = bounded(1);
        let bus = connection.clone();
        let publisher = thread::Builder::new()
            .name("rivu-mpris".into())
            .spawn(move || {
                let mut previous = initial;
                let mut metadata = TrackMetadata::from_state(&previous);
                loop {
                    select_biased! {
                        recv(stopped) -> _ => break,
                        recv(updates) -> update => {
                            if update.is_err() { break; }
                            let current = handle.snapshot();
                            if let Err(error) = publish_changes(&bus, &previous, &current, &mut metadata) {
                                eprintln!("MPRIS publisher stopped: {error}");
                                break;
                            }
                            if current.shutting_down { break; }
                            previous = current;
                        }
                    }
                }
            })
            .context("Starting MPRIS publisher")?;
        Ok(Self {
            connection: Some(connection),
            stop,
            publisher: Some(publisher),
        })
    }
}

impl Drop for Mpris {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(publisher) = self.publisher.take() {
            let _ = publisher.join();
        }
        if let Some(connection) = self.connection.take() {
            connection.graceful_shutdown();
        }
    }
}

struct Root {
    handle: AppHandle,
    can_raise: bool,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) -> fdo::Result<()> {
        if !self.can_raise {
            return Err(fdo::Error::NotSupported(
                "Rivu has no graphical window".into(),
            ));
        }
        self.handle
            .raise()
            .map_err(|error| fdo::Error::Failed(error.to_string()))
    }

    fn quit(&self) -> fdo::Result<()> {
        request(&self.handle, Command::Shutdown).map(|_| ())
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_raise(&self) -> bool {
        self.can_raise
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn identity(&self) -> &str {
        "Rivu"
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn supported_uri_schemes(&self) -> Vec<&str> {
        Vec::new()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn supported_mime_types(&self) -> Vec<&str> {
        Vec::new()
    }
}

struct Player {
    handle: AppHandle,
}

impl Player {
    fn navigate(&self, command: Command) -> fdo::Result<()> {
        let status = self.handle.snapshot().status;
        let state = request(&self.handle, command)?;
        if state.status == PlaybackStatus::Playing {
            match status {
                PlaybackStatus::Paused => {
                    request(&self.handle, Command::Pause)?;
                }
                PlaybackStatus::Stopped => {
                    request(&self.handle, Command::Stop)?;
                }
                PlaybackStatus::Playing => {}
            }
        }
        Ok(())
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) -> fdo::Result<()> {
        if !can_go_next(&self.handle.snapshot()) {
            return Ok(());
        }
        self.navigate(Command::Next)
    }

    fn previous(&self) -> fdo::Result<()> {
        if !can_go_previous(&self.handle.snapshot()) {
            return Ok(());
        }
        self.navigate(Command::Previous)
    }

    fn pause(&self) -> fdo::Result<()> {
        request(&self.handle, Command::Pause).map(|_| ())
    }

    fn play_pause(&self) -> fdo::Result<()> {
        request(&self.handle, Command::Toggle).map(|_| ())
    }

    fn stop(&self) -> fdo::Result<()> {
        request(&self.handle, Command::Stop).map(|_| ())
    }

    fn play(&self) -> fdo::Result<()> {
        let state = self.handle.snapshot();
        if state.queue.is_empty() || state.status == PlaybackStatus::Playing {
            return Ok(());
        }
        request(&self.handle, Command::Resume).map(|_| ())
    }

    fn seek(&self, offset: i64) -> fdo::Result<()> {
        let state = self.handle.snapshot();
        if !can_seek(&state) {
            return Ok(());
        }
        match relative_seek(state.position, offset, state.duration) {
            SeekTarget::Next => self.next(),
            SeekTarget::Position(seconds) => request(
                &self.handle,
                Command::SeekQueue {
                    queue_id: state
                        .current_queue_id
                        .expect("seekable track has a queue entry"),
                    seconds,
                },
            )
            .map(|_| ()),
        }
    }

    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) -> fdo::Result<()> {
        let state = self.handle.snapshot();
        if !can_seek(&state) {
            return Ok(());
        }
        let Some(queue_id) = state.current_queue_id else {
            return Ok(());
        };
        if track_id.as_str() != track_path(queue_id) {
            return Ok(());
        }
        let Some(seconds) = absolute_seek(position, state.duration) else {
            return Ok(());
        };
        request(&self.handle, Command::SeekQueue { queue_id, seconds }).map(|_| ())
    }

    fn open_uri(&self, uri: &str) -> fdo::Result<()> {
        let _ = uri;
        Err(fdo::Error::NotSupported(
            "Import music into Rivu before playing it".into(),
        ))
    }

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> &'static str {
        playback_status(self.handle.snapshot().status)
    }

    // The core publisher owns change emission, including changes originating
    // from D-Bus. Disable automatic setter signals to avoid duplicates and
    // notifications for assignments that leave the actual value unchanged.
    #[zbus(property(emits_changed_signal = "false"))]
    fn loop_status(&self) -> &'static str {
        loop_status(self.handle.snapshot().repeat)
    }

    #[zbus(property)]
    fn set_loop_status(&self, value: &str) -> fdo::Result<()> {
        let mode = match value {
            "None" => RepeatMode::Off,
            "Track" => RepeatMode::One,
            "Playlist" => RepeatMode::All,
            _ => {
                return Err(fdo::Error::InvalidArgs(
                    "LoopStatus must be None, Track or Playlist".into(),
                ));
            }
        };
        request(&self.handle, Command::Repeat { mode }).map(|_| ())
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn set_rate(&self, value: f64) -> fdo::Result<()> {
        if value != 1.0 {
            return Err(fdo::Error::NotSupported(
                "Rivu supports only playback Rate 1.0".into(),
            ));
        }
        Ok(())
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn shuffle(&self) -> bool {
        self.handle.snapshot().shuffle
    }

    #[zbus(property)]
    fn set_shuffle(&self, enabled: bool) -> fdo::Result<()> {
        request(&self.handle, Command::Shuffle { enabled }).map(|_| ())
    }

    #[zbus(property)]
    fn metadata(&self) -> Metadata {
        metadata_map(TrackMetadata::from_state(&self.handle.snapshot()).as_ref())
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn volume(&self) -> f64 {
        f64::from(self.handle.snapshot().volume)
    }

    #[zbus(property)]
    fn set_volume(&self, value: f64) -> fdo::Result<()> {
        if !value.is_finite() {
            return Err(fdo::Error::InvalidArgs("Volume must be finite".into()));
        }
        // MPRIS requires negative volume to mean mute; the core has no gain
        // above unity, so values above unity saturate at its actual maximum.
        request(
            &self.handle,
            Command::Volume {
                value: value.clamp(0.0, 1.0) as f32,
            },
        )
        .map(|_| ())
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        microseconds(self.handle.snapshot().position)
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        can_go_next(&self.handle.snapshot())
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        can_go_previous(&self.handle.snapshot())
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        !self.handle.snapshot().queue.is_empty()
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        !self.handle.snapshot().queue.is_empty()
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        can_seek(&self.handle.snapshot())
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        true
    }
}

fn request(handle: &AppHandle, command: Command) -> fdo::Result<AppState> {
    let response = handle.request(command);
    if response.ok {
        Ok(response.state)
    } else {
        Err(fdo::Error::Failed(
            response
                .error
                .unwrap_or_else(|| "Core command failed".into()),
        ))
    }
}

fn playback_status(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Stopped => "Stopped",
        PlaybackStatus::Playing => "Playing",
        PlaybackStatus::Paused => "Paused",
    }
}

fn loop_status(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "None",
        RepeatMode::All => "Playlist",
        RepeatMode::One => "Track",
    }
}

fn can_seek(state: &AppState) -> bool {
    state.status != PlaybackStatus::Stopped && state.current_queue_id.is_some()
}

fn can_go_next(state: &AppState) -> bool {
    if state.queue.is_empty() {
        return false;
    }
    if state.shuffle || state.repeat == RepeatMode::All {
        return true;
    }
    state
        .current_queue_id
        .and_then(|id| state.queue.iter().position(|entry| entry.id == id))
        .is_none_or(|index| index + 1 < state.queue.len())
}

fn can_go_previous(state: &AppState) -> bool {
    if state.queue.is_empty() {
        return false;
    }
    // A loaded track can restart; with stopped playback the first queue
    // occurrence cannot seek and has no predecessor. Shuffle history belongs
    // to the core, so unknown navigation availability is true per MPRIS.
    if can_seek(state) || state.shuffle {
        return true;
    }
    state
        .current_queue_id
        .and_then(|id| state.queue.iter().position(|entry| entry.id == id))
        .is_none_or(|index| index > 0)
}

fn microseconds(seconds: f64) -> i64 {
    if seconds.is_nan() || seconds <= 0.0 {
        return 0;
    }
    (seconds * MICROS_PER_SECOND) as i64
}

#[derive(Debug, PartialEq)]
enum SeekTarget {
    Position(f64),
    Next,
}

fn relative_seek(position: f64, offset: i64, duration: Option<f64>) -> SeekTarget {
    let target = (position + offset as f64 / MICROS_PER_SECOND).max(0.0);
    if duration.is_some_and(|length| target > length) {
        SeekTarget::Next
    } else {
        SeekTarget::Position(target)
    }
}

fn absolute_seek(position: i64, duration: Option<f64>) -> Option<f64> {
    if position < 0 {
        return None;
    }
    let seconds = position as f64 / MICROS_PER_SECOND;
    if duration.is_some_and(|length| seconds > length) {
        None
    } else {
        Some(seconds)
    }
}

fn track_path(queue_id: u64) -> String {
    format!("/org/mpris/MediaPlayer2/track/q{queue_id}")
}

#[derive(PartialEq)]
struct TrackMetadata {
    queue_id: u64,
    title: String,
    artist: String,
    album: String,
    url: String,
    length: Option<i64>,
}

impl TrackMetadata {
    fn from_state(state: &AppState) -> Option<Self> {
        let queue_id = state.current_queue_id?;
        let track = state.current_track()?;
        Some(Self {
            queue_id,
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            url: url::Url::from_file_path(&track.path).ok()?.into(),
            length: state
                .duration
                .or(track.duration)
                .filter(|duration| duration.is_finite() && *duration >= 0.0)
                .map(microseconds),
        })
    }
}

fn metadata_map(track: Option<&TrackMetadata>) -> Metadata {
    let mut metadata = Metadata::new();
    let path = track.map_or_else(|| NO_TRACK.to_owned(), |track| track_path(track.queue_id));
    metadata.insert(
        "mpris:trackid".into(),
        ObjectPath::try_from(path)
            .expect("static prefix and decimal queue ID form an object path")
            .into(),
    );
    if let Some(track) = track {
        if let Some(length) = track.length {
            metadata.insert("mpris:length".into(), length.into());
        }
        metadata.insert("xesam:title".into(), Str::from(track.title.as_str()).into());
        metadata.insert(
            "xesam:artist".into(),
            Value::from(vec![track.artist.as_str()])
                .try_to_owned()
                .expect("string arrays contain no file descriptors"),
        );
        metadata.insert("xesam:album".into(), Str::from(track.album.as_str()).into());
        metadata.insert("xesam:url".into(), Str::from(track.url.as_str()).into());
    }
    metadata
}

fn publish_changes(
    connection: &Connection,
    previous: &AppState,
    current: &AppState,
    metadata: &mut Option<TrackMetadata>,
) -> zbus::Result<()> {
    let mut changed: HashMap<&str, Value<'_>> = HashMap::new();
    if previous.status != current.status {
        changed.insert("PlaybackStatus", playback_status(current.status).into());
    }
    if previous.repeat != current.repeat {
        changed.insert("LoopStatus", loop_status(current.repeat).into());
    }
    if previous.shuffle != current.shuffle {
        changed.insert("Shuffle", current.shuffle.into());
    }
    if previous.volume != current.volume {
        changed.insert("Volume", f64::from(current.volume).into());
    }
    // Arc comparisons avoid rebuilding metadata on ordinary audio clock ticks.
    if previous.current_queue_id != current.current_queue_id
        || previous.duration != current.duration
        || !Arc::ptr_eq(&previous.library, &current.library)
        || !Arc::ptr_eq(&previous.queue, &current.queue)
    {
        let next = TrackMetadata::from_state(current);
        if *metadata != next {
            changed.insert("Metadata", Value::from(metadata_map(next.as_ref())));
            *metadata = next;
        }
    }
    // Queue navigation depends on ordering, selection and repeat/shuffle,
    // not the audio clock; do not walk the queue on each position update.
    if previous.current_queue_id != current.current_queue_id
        || previous.status != current.status
        || previous.shuffle != current.shuffle
        || previous.repeat != current.repeat
        || !Arc::ptr_eq(&previous.queue, &current.queue)
    {
        for (name, before, after) in [
            ("CanGoNext", can_go_next(previous), can_go_next(current)),
            (
                "CanGoPrevious",
                can_go_previous(previous),
                can_go_previous(current),
            ),
        ] {
            if before != after {
                changed.insert(name, after.into());
            }
        }
    }
    for (name, before, after) in [
        (
            "CanPlay",
            !previous.queue.is_empty(),
            !current.queue.is_empty(),
        ),
        (
            "CanPause",
            !previous.queue.is_empty(),
            !current.queue.is_empty(),
        ),
        ("CanSeek", can_seek(previous), can_seek(current)),
    ] {
        if before != after {
            changed.insert(name, after.into());
        }
    }
    if !changed.is_empty() {
        connection.emit_signal(
            None::<&str>,
            OBJECT_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(PLAYER_INTERFACE, changed, Vec::<&str>::new()),
        )?;
    }
    // Position itself never sends PropertiesChanged. Core explicitly marks
    // discontinuities, including seeks from non-D-Bus clients, so small seeks
    // are not lost to a clock-drift threshold and regular progress stays quiet.
    if previous.seek_revision != current.seek_revision {
        connection.emit_signal(
            None::<&str>,
            OBJECT_PATH,
            PLAYER_INTERFACE,
            "Seeked",
            &(microseconds(current.position),),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_boundaries_use_microseconds_and_distinguish_next() {
        assert_eq!(
            relative_seek(5.0, -6_000_000, Some(10.0)),
            SeekTarget::Position(0.0)
        );
        assert_eq!(
            relative_seek(5.0, 5_000_000, Some(10.0)),
            SeekTarget::Position(10.0)
        );
        assert_eq!(relative_seek(5.0, 5_000_001, Some(10.0)), SeekTarget::Next);
        assert_eq!(absolute_seek(-1, Some(10.0)), None);
        assert_eq!(absolute_seek(10_000_000, Some(10.0)), Some(10.0));
        assert_eq!(absolute_seek(10_000_001, Some(10.0)), None);
        assert_eq!(absolute_seek(1_250_000, None), Some(1.25));
        assert_eq!(microseconds(1.25), 1_250_000);
        assert_eq!(microseconds(f64::INFINITY), i64::MAX);
    }

    #[test]
    fn metadata_has_dbus_types_and_queue_occurrence_identity() {
        let track = TrackMetadata {
            queue_id: 42,
            title: "Title".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            url: "file:///music/a%20b.flac".into(),
            length: Some(1_250_000),
        };
        let metadata = metadata_map(Some(&track));
        assert_eq!(metadata["mpris:trackid"].value_signature().to_string(), "o");
        assert_eq!(metadata["mpris:length"].value_signature().to_string(), "x");
        assert_eq!(metadata["xesam:artist"].value_signature().to_string(), "as");
        assert_eq!(<&str>::try_from(&metadata["xesam:title"]).unwrap(), "Title");
        assert_eq!(i64::try_from(&metadata["mpris:length"]).unwrap(), 1_250_000);
        assert_ne!(track_path(42), track_path(43));
        let empty = metadata_map(None);
        assert_eq!(
            <&ObjectPath<'_>>::try_from(&empty["mpris:trackid"])
                .unwrap()
                .as_str(),
            NO_TRACK
        );
    }
}
