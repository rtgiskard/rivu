//! Session-bus MPRIS2 adapter. The application owns one media manager; this
//! module owns enable transitions, cancellable registration and publication.
//! Owned executor tasks cancel publication, drain accepted replies with a
//! deadline, then close the connection before application executor retirement.
//! Playback changes come from the core subscription, never a polling loop.

use crate::{
    app::AppHandle,
    artwork::{Artwork, ArtworkIdentity, ArtworkManager},
    core::{CoreHandle, Navigation},
    execution::BlockingExecutor,
    projection::MprisSnapshot,
};
use anyhow::{Context, Result, anyhow};
use futures::{FutureExt, channel::oneshot};
use rivu_protocol::AppCommand;
use rivu_protocol::model::{Command, PlaybackStatus, RepeatMode};
use std::{
    collections::HashMap,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use zbus::{
    Connection,
    connection::Builder,
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

pub(crate) struct MprisController {
    stop: Option<oneshot::Sender<()>>,
    worker: Option<async_executor::Task<()>>,
}

impl MprisController {
    pub(crate) fn start(
        handle: CoreHandle,
        app: AppHandle,
        artwork: Arc<ArtworkManager>,
        executor: &Arc<async_executor::Executor<'static>>,
        workers: Arc<BlockingExecutor>,
    ) -> Self {
        let changes = handle.subscribe_revisions();
        let service_executor = Arc::clone(executor);
        let (stop, stopped) = oneshot::channel();
        let worker = executor.spawn(async move {
            let mut stopped = stopped.fuse();
            let mut service: Option<Mpris> = None;
            let mut registration: Option<Registration> = None;
            let mut enabled = None;
            loop {
                let state = handle.mpris_service_state();
                if state.shutting_down {
                    break;
                }
                if enabled != Some(state.enabled) {
                    enabled = Some(state.enabled);
                    if let Some(pending) = registration.take() {
                        pending.cancel().await;
                    }
                    if let Some(active) = service.take() {
                        active.retire().await;
                    }
                    if state.enabled {
                        registration = Some(Registration::spawn(
                            &service_executor,
                            register(handle.clone(), app.clone(), Arc::clone(&artwork)),
                        ));
                    } else {
                        let _ = handle.send(Command::MprisStatus {
                            status: "Desktop media controls disabled".to_owned(),
                        });
                    }
                }
                let connection = {
                    let registered = async {
                        match registration.as_mut() {
                            Some(pending) => pending.result().await,
                            None => futures::future::pending().await,
                        }
                    }
                    .fuse();
                    futures::pin_mut!(registered);
                    futures::select_biased! {
                        _ = stopped => break,
                        changed = changes.recv().fuse() => {
                            if changed.is_err() {
                                break;
                            }
                            continue;
                        }
                        connection = registered => connection,
                    }
                };
                registration = None;
                // Reconcile intent again: registration may finish with
                // disable or shutdown already queued by the core.
                let state = handle.mpris_service_state();
                match connection {
                    Ok(connection) if state.enabled && !state.shutting_down => {
                        service = Some(Mpris::start(
                            connection,
                            handle.clone(),
                            app.clone(),
                            Arc::clone(&artwork),
                            &service_executor,
                            Arc::clone(&workers),
                        ));
                    }
                    Ok(connection) => retire_connection(connection).await,
                    Err(error) if state.enabled && !state.shutting_down => {
                        let _ = handle.send(Command::MprisStatus {
                            status: format!("MPRIS unavailable: {error:#}"),
                        });
                    }
                    Err(_) => {}
                }
            }
            if let Some(pending) = registration {
                pending.cancel().await;
            }
            if let Some(active) = service {
                active.retire().await;
            }
        });
        Self {
            stop: Some(stop),
            worker: Some(worker),
        }
    }
}

impl Drop for MprisController {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            // App owns this boundary, outside GPUI and before executor stop.
            if async_io::block_on(AssertUnwindSafe(worker).catch_unwind()).is_err() {
                tracing::warn!("mpris_manager_task_panicked");
            }
        }
    }
}

/// Every registration attempt is owned, including cancellation during connect,
/// authentication or name acquisition. Completed connections still drain replies.
struct Registration {
    worker: async_executor::Task<Result<Connection>>,
}

impl Registration {
    fn spawn(
        executor: &Arc<async_executor::Executor<'static>>,
        attempt: impl Future<Output = Result<Connection>> + Send + 'static,
    ) -> Self {
        Self {
            worker: executor.spawn(attempt),
        }
    }

    async fn result(&mut self) -> Result<Connection> {
        AssertUnwindSafe(&mut self.worker)
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(anyhow!("MPRIS registration task panicked")))
    }

    async fn cancel(self) {
        match AssertUnwindSafe(self.worker.cancel()).catch_unwind().await {
            Ok(Some(Ok(connection))) => retire_connection(connection).await,
            Err(_) => tracing::warn!("mpris_registration_task_panicked"),
            Ok(_) => {}
        }
    }
}

async fn register(
    handle: CoreHandle,
    app: AppHandle,
    artwork: Arc<ArtworkManager>,
) -> Result<Connection> {
    // Builder uses DoNotQueue: another Rivu must fail, not silently wait for
    // the name or replace the running player's controls.
    Builder::session()
        .context("Connecting MPRIS to the session bus")?
        .name(BUS_NAME)?
        .allow_name_replacements(false)
        .replace_existing_names(false)
        .method_timeout(Duration::from_secs(5))
        .serve_at(OBJECT_PATH, Root { app })?
        .serve_at(
            OBJECT_PATH,
            Player {
                handle,
                artwork,
                artwork_result: parking_lot::Mutex::new(None),
            },
        )?
        .build()
        .await
        .context("Registering org.mpris.MediaPlayer2.rivu")
}

struct Mpris {
    connection: Connection,
    cancel: oneshot::Sender<()>,
    publisher: async_executor::Task<()>,
    handle: CoreHandle,
}

impl Mpris {
    fn start(
        connection: Connection,
        handle: CoreHandle,
        app: AppHandle,
        artwork: Arc<ArtworkManager>,
        executor: &Arc<async_executor::Executor<'static>>,
        workers: Arc<BlockingExecutor>,
    ) -> Self {
        let updates = handle.subscribe_revisions();
        let initial = handle.mpris_snapshot();
        let initial_can_raise = app.can_raise();
        let (cancel, mut cancelled) = oneshot::channel();
        let bus = connection.clone();
        let publisher_handle = handle.clone();
        let publisher = executor.spawn(async move {
            // The publisher alone owns success/failure status; initial failure
            // must not be overwritten by a delayed manager success message.
            let _ = handle.send(Command::MprisStatus {
                status: "Connected to desktop media controls".to_owned(),
            });
            let mut previous = initial;
            let mut previous_can_raise = initial_can_raise;
            let mut metadata = TrackMetadata::from_snapshot(&previous, &artwork);
            let mut loaded_artwork = None;
            let mut artwork_load = None;
            let publication = AssertUnwindSafe(async {
                loop {
                    if artwork_load.is_none() {
                        artwork_load = load_artwork(
                            &workers,
                            &handle.mpris_snapshot(),
                            &artwork,
                            &loaded_artwork,
                        );
                    }
                    let event = {
                        let completion = async {
                            match artwork_load.as_mut() {
                                Some(load) => (&mut load.completion).await,
                                None => futures::future::pending().await,
                            }
                        }
                        .fuse();
                        futures::pin_mut!(completion);
                        futures::select_biased! {
                            _ = (&mut cancelled).fuse() => break,
                            update = updates.recv().fuse() => {
                                if update.is_err() { break; }
                                PublisherEvent::Revision
                            },
                            result = completion => PublisherEvent::Artwork(result),
                        }
                    };
                    match event {
                        PublisherEvent::Revision => {
                            let current = handle.mpris_snapshot();
                            // Selection changes release the old metadata lease even
                            // while another physical load still occupies the slot.
                            if !loaded_artwork.as_ref().is_some_and(|identity| {
                                current
                                    .current_track()
                                    .is_some_and(|track| identity.matches(track))
                            }) {
                                loaded_artwork = None;
                            }
                            let current_can_raise = app.can_raise();
                            if !publish_until_cancelled(
                                &mut cancelled,
                                &handle,
                                publish_changes(
                                    &bus,
                                    &previous,
                                    &current,
                                    &mut metadata,
                                    &artwork,
                                    previous_can_raise,
                                    current_can_raise,
                                ),
                            )
                            .await
                            {
                                break;
                            }
                            if current.shutting_down {
                                break;
                            }
                            previous = current;
                            previous_can_raise = current_can_raise;
                        }
                        PublisherEvent::Artwork(result) => {
                            // Only physical completion frees this single-flight slot.
                            artwork_load = None;
                            if !publish_until_cancelled(&mut cancelled, &handle, async {
                                publish_artwork(
                                    &bus,
                                    &handle,
                                    result?,
                                    &mut metadata,
                                    &artwork,
                                    &mut loaded_artwork,
                                )
                                .await
                            })
                            .await
                            {
                                break;
                            }
                        }
                    }
                }
            })
            .catch_unwind()
            .await;
            // Cancellation stops publication, not the blocking closure. Retain
            // its waiter through physical completion before service retirement
            // or replacement, even if publication itself panicked.
            if let Some(load) = artwork_load {
                load.retire().await;
            }
            if let Err(panic) = publication {
                std::panic::resume_unwind(panic);
            }
        });
        Self {
            connection,
            cancel,
            publisher,
            handle: publisher_handle,
        }
    }

    async fn retire(self) {
        let _ = self.cancel.send(());
        if AssertUnwindSafe(self.publisher)
            .catch_unwind()
            .await
            .is_err()
        {
            let _ = self.handle.send(Command::MprisStatus {
                status: "publisher stopped: publisher task panicked".to_owned(),
            });
            tracing::warn!("mpris_publisher_task_panicked");
        }
        retire_connection(self.connection).await;
    }
}

/// Cancellation releases backpressured bus sends without detaching their task.
async fn publish_until_cancelled(
    cancelled: &mut oneshot::Receiver<()>,
    handle: &CoreHandle,
    publication: impl Future<Output = zbus::Result<()>>,
) -> bool {
    let result = {
        let publication = publication.fuse();
        futures::pin_mut!(publication);
        futures::select_biased! {
            _ = cancelled.fuse() => None,
            result = publication => Some(result),
        }
    };
    match result {
        Some(Ok(())) => true,
        Some(Err(error)) => {
            let _ = handle.send(Command::MprisStatus {
                status: format!("publisher stopped: {error}"),
            });
            tracing::warn!(error = %error, "mpris_publisher_stopped");
            false
        }
        None => false,
    }
}

async fn retire_connection(connection: Connection) {
    // zbus and the deadlines use runtime-independent async-io driving.
    // Removing entry points and taking exclusive guards drains accepted Quit
    // acknowledgements: zbus holds read guards through sending method replies.
    if rivu_protocol::transport::timeout(Duration::from_secs(1), drain_interfaces(&connection))
        .await
        .is_err()
    {
        tracing::warn!("mpris_connection_drain_timed_out");
    }
    // Close all clones and release the name even when a peer retains a call.
    match rivu_protocol::transport::timeout(Duration::from_secs(1), connection.close()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(error = %error, "mpris_connection_close_failed"),
        Err(_) => tracing::warn!("mpris_connection_close_timed_out"),
    }
}

async fn drain_interfaces(connection: &Connection) {
    let server = connection.object_server();
    let root = server.interface::<_, Root>(OBJECT_PATH).await.ok();
    let player = server.interface::<_, Player>(OBJECT_PATH).await.ok();
    let _ = server.remove::<Root, _>(OBJECT_PATH).await;
    let _ = server.remove::<Player, _>(OBJECT_PATH).await;
    if let Some(root) = root {
        let _guard = root.get_mut().await;
    }
    if let Some(player) = player {
        let _guard = player.get_mut().await;
    }
}

struct Root {
    app: AppHandle,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) -> fdo::Result<()> {
        if !self.app.can_raise() {
            return Err(fdo::Error::NotSupported(
                "Rivu has no graphical host".into(),
            ));
        }
        self.app.raise().map_err(|error| {
            if !self.app.can_raise() {
                fdo::Error::NotSupported("Rivu has no graphical host".into())
            } else {
                fdo::Error::Failed(error.to_string())
            }
        })
    }

    async fn quit(&self) -> fdo::Result<()> {
        let ack = self.app.request_ack_async(AppCommand::Shutdown).await;
        if ack.is_success() {
            Ok(())
        } else {
            Err(fdo::Error::Failed(
                ack.error.unwrap_or_else(|| "Shutdown failed".into()),
            ))
        }
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        self.app.can_raise()
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
    handle: CoreHandle,
    artwork: Arc<ArtworkManager>,
    artwork_result: parking_lot::Mutex<Option<Artwork>>,
}

impl Player {
    async fn navigate(&self, direction: Navigation) -> fdo::Result<()> {
        acknowledge(self.handle.navigate(direction).await)
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    async fn next(&self) -> fdo::Result<()> {
        if !can_go_next(&self.handle.mpris_snapshot()) {
            return Ok(());
        }
        self.navigate(Navigation::Next).await
    }

    async fn previous(&self) -> fdo::Result<()> {
        if !can_go_previous(&self.handle.mpris_snapshot()) {
            return Ok(());
        }
        self.navigate(Navigation::Previous).await
    }

    async fn pause(&self) -> fdo::Result<()> {
        request(&self.handle, Command::Pause).await
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        request(&self.handle, Command::Toggle).await
    }

    async fn stop(&self) -> fdo::Result<()> {
        request(&self.handle, Command::Stop).await
    }

    async fn play(&self) -> fdo::Result<()> {
        let state = self.handle.mpris_snapshot();
        if state.queue.total == 0 || state.playback.status == PlaybackStatus::Playing {
            return Ok(());
        }
        request(&self.handle, Command::Resume).await
    }

    async fn seek(&self, offset: i64) -> fdo::Result<()> {
        let state = self.handle.mpris_snapshot();
        if !can_seek(&state) {
            return Ok(());
        }
        acknowledge(
            self.handle
                .mpris_seek(
                    state
                        .queue
                        .current_id
                        .expect("seekable track has a queue entry"),
                    offset,
                )
                .await,
        )
    }

    async fn set_position(&self, track_id: ObjectPath<'_>, position: i64) -> fdo::Result<()> {
        let state = self.handle.mpris_snapshot();
        if !can_seek(&state) {
            return Ok(());
        }
        let Some(queue_id) = state.queue.current_id else {
            return Ok(());
        };
        if track_id.as_str() != track_path(queue_id) {
            return Ok(());
        }
        let Some(seconds) = absolute_seek(position, state.playback.duration) else {
            return Ok(());
        };
        request(&self.handle, Command::SeekQueue { queue_id, seconds }).await
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
        playback_status(self.handle.mpris_snapshot().playback.status)
    }

    // The core publisher owns change emission, including changes originating
    // from D-Bus. Disable automatic setter signals to avoid duplicates and
    // notifications for assignments that leave the actual value unchanged.
    #[zbus(property(emits_changed_signal = "false"))]
    fn loop_status(&self) -> &'static str {
        loop_status(self.handle.mpris_snapshot().playback.repeat)
    }

    #[zbus(property)]
    async fn set_loop_status(&self, value: &str) -> fdo::Result<()> {
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
        request(&self.handle, Command::Repeat { mode }).await
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
        self.handle.mpris_snapshot().playback.shuffle
    }

    #[zbus(property)]
    async fn set_shuffle(&self, enabled: bool) -> fdo::Result<()> {
        request(&self.handle, Command::Shuffle { enabled }).await
    }

    #[zbus(property)]
    fn metadata(&self) -> Metadata {
        let track = TrackMetadata::from_snapshot(&self.handle.mpris_snapshot(), &self.artwork);
        *self.artwork_result.lock() = track.as_ref().and_then(|track| track.art_url.clone());
        metadata_map(track.as_ref())
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn volume(&self) -> f64 {
        f64::from(self.handle.mpris_snapshot().playback.volume)
    }

    #[zbus(property)]
    async fn set_volume(&self, value: f64) -> fdo::Result<()> {
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
        .await
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        microseconds(self.handle.mpris_snapshot().playback.position)
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
        can_go_next(&self.handle.mpris_snapshot())
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        can_go_previous(&self.handle.mpris_snapshot())
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.handle.mpris_snapshot().queue.total != 0
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        can_pause(&self.handle.mpris_snapshot())
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        can_seek(&self.handle.mpris_snapshot())
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        true
    }
}

async fn request(handle: &CoreHandle, command: Command) -> fdo::Result<()> {
    acknowledge(handle.request_ack(command).await)
}

fn acknowledge(response: rivu_protocol::response::Ack) -> fdo::Result<()> {
    if response.is_success() {
        Ok(())
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

fn can_pause(state: &MprisSnapshot) -> bool {
    state.current_track().is_some()
}

fn can_seek(state: &MprisSnapshot) -> bool {
    state.playback.status != PlaybackStatus::Stopped && state.queue.current_id.is_some()
}

fn can_go_next(state: &MprisSnapshot) -> bool {
    if state.queue.total == 0 {
        return false;
    }
    if state.playback.shuffle || state.playback.repeat == RepeatMode::All {
        return true;
    }
    state
        .queue
        .current_position
        .is_none_or(|index| index + 1 < state.queue.total)
}

fn can_go_previous(state: &MprisSnapshot) -> bool {
    if state.queue.total == 0 {
        return false;
    }
    // A loaded track can restart; with stopped playback the first queue
    // occurrence cannot seek and has no predecessor. Shuffle history belongs
    // to the core, so unknown navigation availability is true per MPRIS.
    if can_seek(state) || state.playback.shuffle {
        return true;
    }
    state.queue.current_position.is_none_or(|index| index > 0)
}

fn microseconds(seconds: f64) -> i64 {
    if seconds.is_nan() || seconds <= 0.0 {
        return 0;
    }
    (seconds * MICROS_PER_SECOND) as i64
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
    art_url: Option<Artwork>,
}

impl TrackMetadata {
    fn from_snapshot(state: &MprisSnapshot, artwork: &ArtworkManager) -> Option<Self> {
        let queue_id = state.queue.current_id?;
        let track = state.current_track()?;
        Some(Self {
            queue_id,
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            url: url::Url::from_file_path(&track.path).ok()?.into(),
            art_url: artwork.cached(track).or_else(|| artwork.cached_default()),
            length: state
                .playback
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
        if let Some(art_url) = &track.art_url {
            metadata.insert("mpris:artUrl".into(), Str::from(art_url.uri()).into());
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

/// Keep the publisher's metadata cache tied to the selected track snapshot,
/// including path-only scans that do not change the queue's row revision.
fn refresh_metadata(
    previous: &MprisSnapshot,
    current: &MprisSnapshot,
    metadata: &mut Option<TrackMetadata>,
    artwork: &ArtworkManager,
) -> bool {
    let track_changed = match (&previous.current_track, &current.current_track) {
        (Some(previous), Some(current)) => !Arc::ptr_eq(previous, current),
        (None, None) => false,
        _ => true,
    };
    if !track_changed
        && previous.queue.current_id == current.queue.current_id
        && previous.playback.duration == current.playback.duration
        && previous.queue.current_position.is_some() == current.queue.current_position.is_some()
    {
        return false;
    }
    let next = TrackMetadata::from_snapshot(current, artwork);
    if *metadata == next {
        return false;
    }
    *metadata = next;
    true
}

struct ArtworkLoad {
    cancelled: Arc<AtomicBool>,
    completion:
        futures::future::BoxFuture<'static, zbus::Result<(ArtworkIdentity, Option<Artwork>)>>,
}

impl ArtworkLoad {
    async fn retire(self) {
        // Skip cold I/O if its bounded execution slot has not started it. If it has, await the
        // actual closure rather than dropping its logical waiter.
        self.cancelled.store(true, Ordering::Release);
        let _ = self.completion.await;
    }
}

enum PublisherEvent {
    Revision,
    Artwork(zbus::Result<(ArtworkIdentity, Option<Artwork>)>),
}

/// Cold probes use the application's bounded blocking executor, never its reactor.
/// publisher retains this future until physical completion, including shutdown.
fn load_artwork(
    workers: &Arc<BlockingExecutor>,
    requested: &MprisSnapshot,
    artwork: &Arc<ArtworkManager>,
    loaded: &Option<ArtworkIdentity>,
) -> Option<ArtworkLoad> {
    let track = requested
        .current_track()
        .filter(|_| !requested.shutting_down)?;
    if loaded
        .as_ref()
        .is_some_and(|identity| identity.matches(track))
    {
        return None;
    }
    let identity = ArtworkIdentity::for_track(track);
    let manager = Arc::clone(artwork);
    let track = track.clone();
    let workers = Arc::clone(workers);
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation = Arc::clone(&cancelled);
    let completion = async move {
        let result = workers
            .execute(move || {
                if cancellation.load(Ordering::Acquire) {
                    None
                } else {
                    manager.load(&track).or_else(|| manager.export_default())
                }
            })
            .await
            .map_err(|error| zbus::Error::Failure(format!("Loading MPRIS artwork: {error:#}")))?;
        Ok((identity, result))
    }
    .boxed();
    Some(ArtworkLoad {
        cancelled,
        completion,
    })
}

/// Re-read playback after physical completion: stale artwork may only warm the cache.
async fn publish_artwork(
    connection: &Connection,
    handle: &CoreHandle,
    (identity, result): (ArtworkIdentity, Option<Artwork>),
    metadata: &mut Option<TrackMetadata>,
    artwork: &ArtworkManager,
    loaded: &mut Option<ArtworkIdentity>,
) -> zbus::Result<()> {
    let current = handle.mpris_snapshot();
    if current.shutting_down
        || !current
            .current_track()
            .is_some_and(|track| identity.matches(track))
    {
        return Ok(());
    }
    *loaded = Some(identity);
    let mut next = TrackMetadata::from_snapshot(&current, artwork);
    if let Some(next) = &mut next {
        next.art_url = result;
    }
    if *metadata != next {
        let mut changed: HashMap<&str, Value<'_>> = HashMap::new();
        changed.insert("Metadata", Value::from(metadata_map(next.as_ref())));
        connection
            .emit_signal(
                None::<&str>,
                OBJECT_PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(PLAYER_INTERFACE, changed, Vec::<&str>::new()),
            )
            .await?;
        *metadata = next;
    }
    Ok(())
}

async fn publish_changes(
    connection: &Connection,
    previous: &MprisSnapshot,
    current: &MprisSnapshot,
    metadata: &mut Option<TrackMetadata>,
    artwork: &ArtworkManager,
    previous_can_raise: bool,
    current_can_raise: bool,
) -> zbus::Result<()> {
    let mut changed: HashMap<&str, Value<'_>> = HashMap::new();
    if previous.playback.status != current.playback.status {
        changed.insert(
            "PlaybackStatus",
            playback_status(current.playback.status).into(),
        );
    }
    if previous.playback.repeat != current.playback.repeat {
        changed.insert("LoopStatus", loop_status(current.playback.repeat).into());
    }
    if previous.playback.shuffle != current.playback.shuffle {
        changed.insert("Shuffle", current.playback.shuffle.into());
    }
    if previous.playback.volume != current.playback.volume {
        changed.insert("Volume", f64::from(current.playback.volume).into());
    }
    if refresh_metadata(previous, current, metadata, artwork) {
        changed.insert("Metadata", Value::from(metadata_map(metadata.as_ref())));
    }
    // Queue navigation depends on ordering, selection and repeat/shuffle,
    // not the audio clock; do not walk the queue on each position update.
    if previous.queue.current_id != current.queue.current_id
        || previous.playback.status != current.playback.status
        || previous.playback.shuffle != current.playback.shuffle
        || previous.playback.repeat != current.playback.repeat
        || previous.queue != current.queue
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
    if previous.queue.current_id != current.queue.current_id
        || previous.playback.status != current.playback.status
        || previous.queue != current.queue
    {
        for (name, before, after) in [
            (
                "CanPlay",
                previous.queue.total != 0,
                current.queue.total != 0,
            ),
            ("CanPause", can_pause(previous), can_pause(current)),
            ("CanSeek", can_seek(previous), can_seek(current)),
        ] {
            if before != after {
                changed.insert(name, after.into());
            }
        }
    }
    if !changed.is_empty() {
        connection
            .emit_signal(
                None::<&str>,
                OBJECT_PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(PLAYER_INTERFACE, changed, Vec::<&str>::new()),
            )
            .await?;
    }
    if previous_can_raise != current_can_raise {
        let mut changed: HashMap<&str, Value<'_>> = HashMap::new();
        changed.insert("CanRaise", current_can_raise.into());
        connection
            .emit_signal(
                None::<&str>,
                OBJECT_PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &("org.mpris.MediaPlayer2", changed, Vec::<&str>::new()),
            )
            .await?;
    }
    // Position itself never sends PropertiesChanged. Core explicitly marks
    // discontinuities, including seeks from non-D-Bus clients, so small seeks
    // are not lost to a clock-drift threshold and regular progress stays quiet.
    if previous.playback.seek_revision != current.playback.seek_revision {
        connection
            .emit_signal(
                None::<&str>,
                OBJECT_PATH,
                PLAYER_INTERFACE,
                "Seeked",
                &(microseconds(current.playback.position),),
            )
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigPersistence;
    use crossbeam_channel::bounded;
    use rivu_protocol::model::{PlaybackState, QueueState, Track};
    use std::{
        io::{BufRead, BufReader},
        path::PathBuf,
        process::{Child, Stdio},
        sync::atomic::{AtomicBool, Ordering},
    };

    fn runtime_fixture() -> (tempfile::TempDir, crate::core::CoreRuntime) {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        crate::config::Config {
            media_read_buffer_mb: 1,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            ..Default::default()
        }
        .save(&config)
        .unwrap();
        let runtime = crate::core::CoreRuntime::start(directory.path(), &config).unwrap();
        (directory, runtime)
    }

    fn wait_status(handle: &CoreHandle, expected: &str) {
        let changes = handle.subscribe_revisions();
        async_io::block_on(rivu_protocol::transport::timeout(
            Duration::from_secs(5),
            async {
                while handle.gui_snapshot().system.mpris_status != expected {
                    changes.recv().await.unwrap();
                }
            },
        ))
        .unwrap();
    }

    struct Dropped(Arc<AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn initially_disabled_manager_reports_status_without_bus_registration() {
        let (directory, mut runtime) = runtime_fixture();
        let manager = MprisController::start(
            runtime.handle.clone(),
            AppHandle::new(&runtime, false),
            Arc::new(ArtworkManager::new(directory.path())),
            runtime.test_executor(),
            Arc::clone(runtime.test_workers()),
        );
        wait_status(&runtime.handle, "Desktop media controls disabled");
        drop(manager);
        runtime.control.request_shutdown().unwrap();
        runtime.join().unwrap();
    }

    #[test]
    fn registration_panic_becomes_an_error_and_manager_drop_contains_panics() {
        let (_directory, mut runtime) = runtime_fixture();
        let mut attempt = Registration::spawn(runtime.test_executor(), async {
            panic!("registration panic regression");
        });
        let error = async_io::block_on(attempt.result()).unwrap_err();
        assert_eq!(error.to_string(), "MPRIS registration task panicked");

        let (stop, _stopped) = oneshot::channel();
        let worker = runtime.test_executor().spawn(async {
            panic!("manager panic regression");
        });
        drop(MprisController {
            stop: Some(stop),
            worker: Some(worker),
        });
        runtime.control.request_shutdown().unwrap();
        runtime.join().unwrap();
    }

    #[test]
    fn registration_cancellation_drops_pending_build_before_retirement_returns() {
        let (_directory, mut runtime) = runtime_fixture();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(Arc::clone(&dropped));
        let (polled, entered) = bounded(1);
        let attempt = Registration::spawn(runtime.test_executor(), async move {
            let _guard = guard;
            polled.send(()).unwrap();
            futures::future::pending::<Result<Connection>>().await
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        async_io::block_on(attempt.cancel());
        assert!(dropped.load(Ordering::SeqCst));
        runtime.control.request_shutdown().unwrap();
        runtime.join().unwrap();
    }

    #[test]
    fn artwork_retirement_waits_for_physical_completion() {
        let pool = crate::execution::BlockingExecutor::new(1);
        let retired = Arc::new(AtomicBool::new(false));
        let worker_retired = Arc::clone(&retired);
        let (started, entered) = bounded(1);
        let (release, released) = bounded(1);
        let completion = async move {
            pool.execute(move || {
                started.send(()).unwrap();
                released.recv().unwrap();
                worker_retired.store(true, Ordering::Release);
            })
            .await
            .map_err(|error| zbus::Error::Failure(error.to_string()))?;
            Ok((ArtworkIdentity::for_track(&track(1)), None))
        }
        .boxed();
        let mut load = ArtworkLoad {
            cancelled: Arc::new(AtomicBool::new(false)),
            completion,
        };
        assert!((&mut load.completion).now_or_never().is_none());
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut retirement = Box::pin(load.retire());
        assert!(retirement.as_mut().now_or_never().is_none());
        assert!(!retired.load(Ordering::Acquire));
        release.send(()).unwrap();
        async_io::block_on(retirement);
        assert!(retired.load(Ordering::Acquire));
    }

    #[test]
    fn cancelling_unstarted_artwork_preserves_cache_only_behavior() {
        let (directory, mut runtime) = runtime_fixture();
        let artwork = Arc::new(ArtworkManager::new(directory.path()));
        let state = MprisSnapshot {
            queue: QueueState {
                total: 1,
                current_id: Some(1),
                current_position: Some(0),
                revision: 0,
            },
            current_track: Some(Arc::new(track(1))),
            playback: PlaybackState::default(),
            shutting_down: false,
        };
        let load = load_artwork(runtime.test_workers(), &state, &artwork, &None).unwrap();
        async_io::block_on(load.retire());
        assert!(
            artwork
                .cached_result(state.current_track().unwrap())
                .is_none()
        );
        assert!(artwork.cached_default().is_none());
        runtime.control.request_shutdown().unwrap();
        runtime.join().unwrap();
    }

    #[test]
    fn publisher_cancellation_drops_pending_send_before_retirement_returns() {
        let (_directory, mut runtime) = runtime_fixture();
        let handle = runtime.handle.clone();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(Arc::clone(&dropped));
        let (cancel, mut cancelled) = oneshot::channel();
        let (polled, entered) = bounded(1);
        let publisher = runtime.test_executor().spawn(async move {
            publish_until_cancelled(&mut cancelled, &handle, async move {
                let _guard = guard;
                polled.send(()).unwrap();
                futures::future::pending::<zbus::Result<()>>().await
            })
            .await
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        cancel.send(()).unwrap();
        assert!(!async_io::block_on(publisher));
        assert!(dropped.load(Ordering::SeqCst));
        assert!(runtime.handle.gui_snapshot().system.mpris_status.is_empty());
        runtime.control.request_shutdown().unwrap();
        runtime.join().unwrap();
    }

    #[test]
    fn publisher_failure_follows_connected_status_and_cancellation_wins_ready_error() {
        let (_directory, mut runtime) = runtime_fixture();
        let handle = &runtime.handle;
        handle
            .send(Command::MprisStatus {
                status: "Connected to desktop media controls".to_owned(),
            })
            .unwrap();
        let (_cancel, mut cancelled) = oneshot::channel();
        assert!(!async_io::block_on(publish_until_cancelled(
            &mut cancelled,
            handle,
            async { Err(zbus::Error::Failure("test send failure".to_owned())) }
        )));
        let failure = "publisher stopped: test send failure";
        wait_status(handle, failure);
        let (cancel, mut cancelled) = oneshot::channel();
        cancel.send(()).unwrap();
        assert!(!async_io::block_on(publish_until_cancelled(
            &mut cancelled,
            handle,
            async {
                panic!("cancelled publication must not be polled");
            }
        )));
        assert_eq!(handle.gui_snapshot().system.mpris_status, failure);
        runtime.control.request_shutdown().unwrap();
        runtime.join().unwrap();
    }

    // These tests use an isolated session bus, never the user's desktop bus.
    struct TestBus {
        child: Child,
        address: String,
    }

    impl TestBus {
        fn start() -> Self {
            let mut child = std::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .spawn()
                .expect("MPRIS lifecycle tests require dbus-daemon");
            let mut address = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut address)
                .unwrap();
            Self {
                child,
                address: address.trim().to_owned(),
            }
        }

        fn connect(&self) -> Connection {
            futures::executor::block_on(Builder::address(self.address.as_str()).unwrap().build())
                .unwrap()
        }

        fn serve_root(&self, app: AppHandle) -> Connection {
            futures::executor::block_on(
                Builder::address(self.address.as_str())
                    .unwrap()
                    .name(BUS_NAME)
                    .unwrap()
                    .serve_at(OBJECT_PATH, Root { app })
                    .unwrap()
                    .build(),
            )
            .unwrap()
        }
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn retirement_deadline_closes_connection_even_with_live_interface_guard() {
        let bus = TestBus::start();
        let (_directory, mut runtime) = runtime_fixture();
        let connection = bus.serve_root(AppHandle::new(&runtime, false));
        let retained = connection.clone();
        let root = futures::executor::block_on(
            connection.object_server().interface::<_, Root>(OBJECT_PATH),
        )
        .unwrap();
        // Model a peer whose reply has not finished: the exclusive drain cannot
        // complete, but the deadline must still close all connection clones.
        let active_reply = futures::executor::block_on(root.get());
        let retire = runtime.test_executor().spawn(retire_connection(connection));
        async_io::block_on(retire);
        assert!(retained.is_closed());
        assert!(futures::executor::block_on(
            retained.object_server().interface::<_, Root>(OBJECT_PATH),
        ).is_err());
        drop(active_reply);
        // A retained closed connection cannot prevent reacquiring the name.
        let replacement = futures::executor::block_on(
            Builder::address(bus.address.as_str())
                .unwrap()
                .name(BUS_NAME)
                .unwrap()
                .build(),
        )
        .unwrap();
        futures::executor::block_on(replacement.close()).unwrap();
        runtime.control.request_shutdown().unwrap();
        runtime.join().unwrap();
    }

    #[test]
    fn publisher_panic_still_retires_connection_and_releases_bus_name() {
        let bus = TestBus::start();
        let (_directory, mut runtime) = runtime_fixture();
        let connection = bus.serve_root(AppHandle::new(&runtime, false));
        let retained = connection.clone();
        let (cancel, _cancelled) = oneshot::channel();
        let publisher = runtime.test_executor().spawn(async {
            panic!("publisher panic regression");
        });
        let service = Mpris {
            connection,
            cancel,
            publisher,
            handle: runtime.handle.clone(),
        };
        async_io::block_on(service.retire());
        wait_status(
            &runtime.handle,
            "publisher stopped: publisher task panicked",
        );
        assert!(retained.is_closed());
        let replacement = async_io::block_on(
            Builder::address(bus.address.as_str())
                .unwrap()
                .name(BUS_NAME)
                .unwrap()
                .build(),
        )
        .unwrap();
        async_io::block_on(replacement.close()).unwrap();
        runtime.control.request_shutdown().unwrap();
        runtime.join().unwrap();
    }

    #[test]
    fn accepted_quit_reply_survives_service_retirement() {
        let bus = TestBus::start();
        let (_directory, mut runtime) = runtime_fixture();
        let connection = bus.serve_root(AppHandle::new(&runtime, false));
        let changes = runtime.handle.subscribe_revisions();
        let client = bus.connect();
        let quit = runtime.test_executor().spawn(async move {
            client
                .call_method(
                    Some(BUS_NAME),
                    OBJECT_PATH,
                    Some("org.mpris.MediaPlayer2"),
                    "Quit",
                    &(),
                )
                .await
        });
        async_io::block_on(rivu_protocol::transport::timeout(
            Duration::from_secs(5),
            async {
                while !runtime.handle.is_shutting_down() {
                    changes.recv().await.unwrap();
                }
            },
        ))
        .unwrap();
        async_io::block_on(retire_connection(connection));
        async_io::block_on(quit).unwrap();
        runtime.join().unwrap();
    }

    fn track(id: i64) -> Track {
        Track {
            id,
            path: PathBuf::from(format!("/music/{id}.flac")),
            fingerprint: None,
            cue: None,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            duration: None,
            codec: String::new(),
            channels: 0,
            sample_rate: 0,
            bitrate_bps: None,
            bitrate_exact: false,
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
            favorite: false,
            decoder_hint: Default::default(),
            missing: false,
            play_count: 0,
            last_played: None,
        }
    }

    #[test]
    fn reselected_artwork_reloads_after_eviction_with_clear_or_another_load_in_flight() {
        use futures::StreamExt;

        async fn next_property(
            signals: &mut (impl futures::Stream<Item = zbus::Message> + Unpin),
            property: &str,
        ) -> OwnedValue {
            rivu_protocol::transport::timeout(Duration::from_secs(5), async {
                loop {
                    let signal = signals.next().await.expect("properties signal stream");
                    let (interface, mut changed, _): (String, Metadata, Vec<String>) =
                        signal.body().deserialize().unwrap();
                    if interface == PLAYER_INTERFACE
                        && let Some(value) = changed.remove(property)
                    {
                        return value;
                    }
                }
            })
            .await
            .expect("consumer receives property changes without polling")
        }

        async fn next_metadata(
            signals: &mut (impl futures::Stream<Item = zbus::Message> + Unpin),
        ) -> Metadata {
            Metadata::try_from(next_property(signals, "Metadata").await).unwrap()
        }

        let bus = TestBus::start();
        let (directory, mut runtime) = runtime_fixture();
        let path = directory.path().join("selected.wav");
        let mut writer = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 8_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        writer.write_sample(0_i16).unwrap();
        writer.finalize().unwrap();
        let other_path = directory.path().join("other.wav");
        std::fs::copy(&path, &other_path).unwrap();
        // Distinct from the default export, so a fallback cannot satisfy the test.
        let mut cover = crate::artwork::DEFAULT_IMAGE.to_vec();
        cover.push(1);
        std::fs::write(directory.path().join("cover.png"), cover).unwrap();
        let ack = async_io::block_on(runtime.handle.request_ack(Command::QueueExternalPaths {
            paths: vec![path, other_path],
            index: None,
            expected_revision: None,
        }));
        assert!(ack.is_success(), "{:?}", ack.error);
        async_io::block_on(
            runtime
                .handle
                .wait_task(rivu_protocol::model::TaskTarget::Scan {
                    id: ack.scan_id.unwrap(),
                }),
        )
        .unwrap();
        // Navigation while stopped selects metadata without opening audio output.
        assert!(async_io::block_on(runtime.handle.navigate(Navigation::Next)).is_success());
        let selected = runtime.handle.mpris_snapshot();
        assert_eq!(selected.playback.status, PlaybackStatus::Stopped);
        let selected_track = selected.current_track().unwrap();
        assert!(async_io::block_on(runtime.handle.navigate(Navigation::Next)).is_success());
        let other = runtime
            .handle
            .mpris_snapshot()
            .current_track()
            .unwrap()
            .clone();
        assert_ne!(other.id, selected_track.id);
        assert!(async_io::block_on(runtime.handle.navigate(Navigation::Previous)).is_success());
        let artwork = Arc::new(ArtworkManager::new(directory.path()));
        let app = AppHandle::new(&runtime, false);
        let connection = bus.serve_root(app.clone());
        let client = bus.connect();
        async_io::block_on(async {
            let proxy = zbus::Proxy::new(
                &client,
                BUS_NAME,
                OBJECT_PATH,
                "org.freedesktop.DBus.Properties",
            )
            .await
            .unwrap();
            let mut signals = proxy.receive_signal("PropertiesChanged").await.unwrap();
            let publisher = Mpris::start(
                connection,
                runtime.handle.clone(),
                app,
                Arc::clone(&artwork),
                runtime.test_executor(),
                Arc::clone(runtime.test_workers()),
            );
            let first = next_metadata(&mut signals).await;
            let original_uri = <&str>::try_from(&first["mpris:artUrl"]).unwrap().to_owned();
            assert_eq!(original_uri, artwork.cached(selected_track).unwrap().uri());
            assert_ne!(original_uri, artwork.export_default().unwrap().uri());

            for (round, clear) in [false, true].into_iter().enumerate() {
                let blocked = if clear {
                    assert!(
                        runtime
                            .handle
                            .request_ack(Command::ClearQueue)
                            .await
                            .is_success()
                    );
                    assert_eq!(next_metadata(&mut signals).await, metadata_map(None));
                    assert!(runtime.handle.mpris_snapshot().current_track().is_none());
                    None
                } else {
                    let blocked =
                        crate::artwork::tests::block_probe(Arc::clone(&artwork), other.clone());
                    assert!(runtime.handle.navigate(Navigation::Next).await.is_success());
                    let changed = next_metadata(&mut signals).await;
                    assert_eq!(
                        changed["mpris:trackid"],
                        metadata_map(
                            TrackMetadata::from_snapshot(
                                &runtime.handle.mpris_snapshot(),
                                &artwork
                            )
                            .as_ref()
                        )["mpris:trackid"]
                    );
                    // A later transport revision proves the publisher has entered
                    // the next iteration with B's physical load occupying its slot.
                    assert!(
                        runtime
                            .handle
                            .request_ack(Command::Volume { value: 0.25 })
                            .await
                            .is_success()
                    );
                    assert_eq!(
                        f64::try_from(next_property(&mut signals, "Volume").await).unwrap(),
                        0.25
                    );
                    Some(blocked)
                };
                // The consumer has observed selection replacement and released A.
                for id in 1..=65 {
                    let mut pressure = track(-((round * 100 + id) as i64));
                    pressure.path = directory.path().join("pressure").join(format!("{id}.wav"));
                    assert!(artwork.load(&pressure).is_none());
                }
                assert!(artwork.cached_result(selected_track).is_none());

                if clear {
                    assert!(
                        runtime
                            .handle
                            .request_ack(Command::Enqueue {
                                track_ids: vec![selected_track.id],
                            })
                            .await
                            .is_success()
                    );
                    assert!(runtime.handle.navigate(Navigation::Next).await.is_success());
                } else {
                    assert!(
                        runtime
                            .handle
                            .navigate(Navigation::Previous)
                            .await
                            .is_success()
                    );
                    let waiting = next_metadata(&mut signals).await;
                    assert_eq!(
                        <&str>::try_from(&waiting["mpris:artUrl"]).unwrap(),
                        artwork.cached_default().unwrap().uri()
                    );
                    assert!(artwork.cached_result(selected_track).is_none());
                }
                let reselected = runtime.handle.mpris_snapshot();
                assert!(
                    ArtworkIdentity::for_track(selected_track)
                        .matches(reselected.current_track().unwrap())
                );
                assert_eq!(reselected.playback.status, PlaybackStatus::Stopped);
                if let Some((release, worker)) = blocked {
                    drop(release);
                    worker.join().unwrap();
                }
                // Require A's real cover notification after B physically retires,
                // not just populated fallback metadata or an already-warm lookup.
                loop {
                    let metadata = next_metadata(&mut signals).await;
                    if metadata
                        .get("mpris:artUrl")
                        .and_then(|value| <&str>::try_from(value).ok())
                        == Some(original_uri.as_str())
                    {
                        assert_eq!(
                            metadata["mpris:trackid"],
                            metadata_map(
                                TrackMetadata::from_snapshot(&reselected, &artwork).as_ref()
                            )["mpris:trackid"]
                        );
                        break;
                    }
                }
                assert!(artwork.cached_result(selected_track).is_some());
            }
            publisher.retire().await;
            client.close().await.unwrap();
        });
        runtime.control.request_shutdown().unwrap();
        runtime.join().unwrap();
    }

    #[test]
    fn late_artwork_completion_cannot_replace_the_selected_occurrence_metadata() {
        let bus = TestBus::start();
        let (directory, mut runtime) = runtime_fixture();
        let paths: Vec<_> = (1..=2)
            .map(|id| directory.path().join(format!("{id}.wav")))
            .collect();
        for path in &paths {
            let mut writer = hound::WavWriter::create(
                path,
                hound::WavSpec {
                    channels: 1,
                    sample_rate: 8_000,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )
            .unwrap();
            writer.write_sample(0_i16).unwrap();
            writer.finalize().unwrap();
        }
        let ack = async_io::block_on(runtime.handle.request_ack(Command::QueueExternalPaths {
            paths,
            index: None,
            expected_revision: None,
        }));
        assert!(ack.is_success(), "{:?}", ack.error);
        async_io::block_on(
            runtime
                .handle
                .wait_task(rivu_protocol::model::TaskTarget::Scan {
                    id: ack.scan_id.unwrap(),
                }),
        )
        .unwrap();
        assert!(async_io::block_on(runtime.handle.navigate(Navigation::Next)).is_success());
        let old = runtime.handle.mpris_snapshot();
        let identity = ArtworkIdentity::for_track(old.current_track().unwrap());
        assert!(async_io::block_on(runtime.handle.navigate(Navigation::Next)).is_success());
        let current = runtime.handle.mpris_snapshot();
        assert_ne!(old.queue.current_id, current.queue.current_id);
        let artwork = ArtworkManager::new(directory.path());
        let mut metadata = TrackMetadata::from_snapshot(&current, &artwork);
        let mut loaded = None;
        let connection = bus.connect();
        let image = artwork.export_default();
        async_io::block_on(publish_artwork(
            &connection,
            &runtime.handle,
            (identity, image),
            &mut metadata,
            &artwork,
            &mut loaded,
        ))
        .unwrap();
        assert!(loaded.is_none());
        let metadata = metadata.unwrap();
        assert_eq!(metadata.queue_id, current.queue.current_id.unwrap());
        assert!(metadata.art_url.is_none());
        async_io::block_on(connection.close()).unwrap();
        runtime.control.request_shutdown().unwrap();
        runtime.join().unwrap();
    }

    #[test]
    fn metadata_getter_leaves_cold_and_replaced_fingerprints_unprobed() {
        let directory = tempfile::tempdir().unwrap();
        let artwork = ArtworkManager::new(directory.path());
        let mut current = track(7);
        current.path = directory.path().join("missing.flac");
        current.fingerprint = Some("original".into());
        let mut state = MprisSnapshot {
            queue: QueueState {
                total: 1,
                current_id: Some(1),
                current_position: Some(0),
                revision: 0,
            },
            current_track: Some(Arc::new(current)),
            playback: PlaybackState::default(),
            shutting_down: false,
        };
        let current = state.current_track().unwrap();
        assert_eq!(
            TrackMetadata::from_snapshot(&state, &artwork)
                .unwrap()
                .art_url,
            artwork.cached(current)
        );
        assert!(artwork.cached_result(current).is_none());
        assert!(!directory.path().join("artwork").exists());
        assert!(
            !metadata_map(TrackMetadata::from_snapshot(&state, &artwork).as_ref())
                .contains_key("mpris:artUrl")
        );
        let default = artwork.export_default().unwrap();
        assert_eq!(
            TrackMetadata::from_snapshot(&state, &artwork)
                .unwrap()
                .art_url,
            Some(default)
        );
        artwork.load(current);
        assert!(artwork.cached_result(current).is_some());
        let mut replacement = current.clone();
        replacement.fingerprint = Some("replacement".into());
        state.current_track = Some(Arc::new(replacement));
        assert!(TrackMetadata::from_snapshot(&state, &artwork).is_some());
        assert!(
            artwork
                .cached_result(state.current_track().unwrap())
                .is_none()
        );
    }

    #[test]
    fn failed_default_export_omits_art_url_without_retrying_from_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let blocked = directory.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let artwork = ArtworkManager::new(&blocked);
        assert!(artwork.export_default().is_none());
        std::fs::remove_file(&blocked).unwrap();
        std::fs::create_dir(&blocked).unwrap();
        let state = MprisSnapshot {
            queue: QueueState {
                total: 1,
                current_id: Some(1),
                current_position: Some(0),
                revision: 0,
            },
            current_track: Some(Arc::new(track(1))),
            playback: PlaybackState::default(),
            shutting_down: false,
        };
        assert!(
            !metadata_map(TrackMetadata::from_snapshot(&state, &artwork).as_ref())
                .contains_key("mpris:artUrl")
        );
        assert!(!blocked.join("artwork").exists());
        assert!(artwork.cached_default().is_none());
    }

    #[test]
    fn metadata_notification_follows_path_changes_without_queue_revision_changes() {
        let directory = tempfile::tempdir().unwrap();
        let artwork = ArtworkManager::new(directory.path());
        let previous = MprisSnapshot {
            queue: QueueState {
                total: 1,
                current_id: Some(1),
                current_position: Some(0),
                revision: 2,
            },
            current_track: Some(Arc::new(track(7))),
            playback: PlaybackState::default(),
            shutting_down: false,
        };
        let mut metadata = TrackMetadata::from_snapshot(&previous, &artwork);
        let mut current = previous.clone();
        current.playback.position = 5.0;
        assert!(!refresh_metadata(
            &previous,
            &current,
            &mut metadata,
            &artwork
        ));

        Arc::make_mut(current.current_track.as_mut().unwrap()).path =
            PathBuf::from("/music/moved/7.flac");
        assert!(refresh_metadata(
            &previous,
            &current,
            &mut metadata,
            &artwork
        ));
        let notification = metadata_map(metadata.as_ref());
        assert_eq!(
            <&str>::try_from(&notification["xesam:url"]).unwrap(),
            "file:///music/moved/7.flac"
        );

        // Replacing a snapshot without changing its public metadata is quiet.
        let mut unchanged = current.clone();
        Arc::make_mut(unchanged.current_track.as_mut().unwrap()).favorite = true;
        assert!(!refresh_metadata(
            &current,
            &unchanged,
            &mut metadata,
            &artwork
        ));

        let mut reordered = current.clone();
        reordered.queue.revision += 1;
        reordered.queue.total = 2;
        reordered.queue.current_position = Some(1);
        assert!(!refresh_metadata(
            &current,
            &reordered,
            &mut metadata,
            &artwork
        ));

        let mut unavailable = reordered.clone();
        unavailable.queue.current_position = None;
        assert!(refresh_metadata(
            &reordered,
            &unavailable,
            &mut metadata,
            &artwork
        ));
        assert!(metadata.is_none());
        assert!(refresh_metadata(
            &unavailable,
            &reordered,
            &mut metadata,
            &artwork
        ));
        assert_eq!(
            <&str>::try_from(&metadata_map(metadata.as_ref())["xesam:url"]).unwrap(),
            "file:///music/moved/7.flac"
        );
    }

    #[test]
    fn can_pause_requires_a_valid_current_track() {
        let mut state = MprisSnapshot {
            queue: QueueState {
                total: 1,
                ..QueueState::default()
            },
            current_track: None,
            playback: PlaybackState::default(),
            shutting_down: false,
        };
        assert!(!can_pause(&state));

        state.queue.current_id = Some(1);
        assert!(!can_pause(&state));

        state.queue.current_position = Some(0);
        state.current_track = Some(Arc::new(track(7)));
        assert!(can_pause(&state));

        state.queue.current_id = Some(2);
        state.queue.current_position = None;
        assert!(!can_pause(&state));
    }
    #[test]
    fn seek_boundaries_use_microseconds() {
        assert_eq!(absolute_seek(-1, Some(10.0)), None);
        assert_eq!(absolute_seek(10_000_000, Some(10.0)), Some(10.0));
        assert_eq!(absolute_seek(10_000_001, Some(10.0)), None);
        assert_eq!(absolute_seek(1_250_000, None), Some(1.25));
        assert_eq!(microseconds(1.25), 1_250_000);
        assert_eq!(microseconds(f64::INFINITY), i64::MAX);
    }

    #[test]
    fn metadata_has_dbus_types_and_queue_occurrence_identity() {
        let directory = tempfile::tempdir().unwrap();
        let artwork = ArtworkManager::new(directory.path());
        let art = artwork.export_default().unwrap();
        let expected_uri = art.uri().to_owned();
        let track = TrackMetadata {
            queue_id: 42,
            title: "Title".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            url: "file:///music/a%20b.flac".into(),
            length: Some(1_250_000),
            art_url: Some(art),
        };
        let metadata = metadata_map(Some(&track));
        assert_eq!(metadata["mpris:trackid"].value_signature().to_string(), "o");
        assert_eq!(metadata["mpris:length"].value_signature().to_string(), "x");
        assert_eq!(metadata["xesam:artist"].value_signature().to_string(), "as");
        assert_eq!(metadata["mpris:artUrl"].value_signature().to_string(), "s");
        assert_eq!(
            <&str>::try_from(&metadata["mpris:artUrl"]).unwrap(),
            expected_uri,
        );
        assert_eq!(<&str>::try_from(&metadata["xesam:title"]).unwrap(), "Title");
        assert_eq!(i64::try_from(&metadata["mpris:length"]).unwrap(), 1_250_000);
        assert_ne!(track_path(42), track_path(43));
        let empty = metadata_map(None);
        assert!(!empty.contains_key("mpris:artUrl"));
        assert_eq!(
            <&ObjectPath<'_>>::try_from(&empty["mpris:trackid"])
                .unwrap()
                .as_str(),
            NO_TRACK
        );
    }
}
