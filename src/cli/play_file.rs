use anyhow::{Context, Result, bail};
use futures::{FutureExt, StreamExt, select};
use rivu::config::{Config, ConfigPersistence};
use rivu_audio::PlaybackSpeed;
use rivu_audio::{AudioCommand, AudioEngine, AudioEvent, PlaybackMetadata};
use std::{
    fs::File,
    io::{IsTerminal, Read, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) fn run(
    path: PathBuf,
    device: Option<String>,
    speed: Option<f64>,
    config_path: &Path,
) -> Result<()> {
    let mut speed = PlaybackSpeed::new(speed.unwrap_or(1.0)).with_context(|| {
        format!(
            "Playback speed must be between {} and {}",
            PlaybackSpeed::MIN,
            PlaybackSpeed::MAX
        )
    })?;
    let engine = start(path, device, speed, &Config::load(config_path)?)?;
    let events = engine
        .events
        .as_ref()
        .context("Audio event receiver unavailable")?;
    let mut terminal = Terminal::open()?;
    async_io::block_on(async {
        loop {
            let event = if let Some(input) = &mut terminal {
                select! {
                    event = events.recv().fuse() => event,
                    signal = input.signals.next().fuse() => {
                        return Err(anyhow::anyhow!("Playback interrupted by {:?}", signal));
                    }
                    key = input.file.read_with(|mut file| {
                        let mut key = [0];
                        Ok((file.read(&mut key)?, key[0]))
                    }).fuse() => {
                        let (count, key) = key?;
                        if count == 0 {
                            terminal = None;
                        } else if let Some(delta) = std::str::from_utf8(std::slice::from_ref(&key))
                            .ok()
                            .and_then(|key| rivu_presentation::keyboard::speed_delta(key, Default::default()))
                        {
                            let next = rivu_format::playback_speed::adjust(speed.get(), delta);
                            if let Some(next) = PlaybackSpeed::new(next).filter(|next| *next != speed) {
                                engine.send(AudioCommand::Speed(next))?;
                                speed = next;
                                writeln!(std::io::stderr().lock(), "{:.2}×", speed.get())?;
                            }
                        }
                        continue;
                    }
                }
            } else {
                events.recv().await
            };
            match event.context("Audio worker stopped before playback completed")? {
                AudioEvent::Ended { .. } => return Ok(()),
                AudioEvent::Failed { message, .. }
                | AudioEvent::OutputSettingsFailed { message, .. } => bail!("{message}"),
                _ => {}
            }
        }
    })
}

struct Terminal {
    file: async_io::Async<File>,
    baseline: libc::termios,
    signals: async_signal::Signals,
}

impl Terminal {
    fn open() -> Result<Option<Self>> {
        use async_signal::{Signal, Signals};
        if !std::io::stdin().is_terminal() {
            return Ok(None);
        }
        // Reopen the input so Async can set nonblocking mode on its own descriptor.
        let file = File::open("/proc/self/fd/0")?;
        let fd = file.as_raw_fd();
        // SAFETY: These queries take no pointers and do not change the terminal.
        if unsafe { libc::tcgetpgrp(fd) != libc::getpgrp() } {
            return Ok(None);
        }
        let signals = Signals::new([
            Signal::Int,
            Signal::Term,
            Signal::Hup,
            Signal::Quit,
            Signal::Tstp,
        ])?;
        let file = async_io::Async::new(file)?;
        // SAFETY: tcgetattr initializes baseline on success, and fd is live.
        let baseline = unsafe {
            let mut baseline = std::mem::MaybeUninit::uninit();
            if libc::tcgetattr(fd, baseline.as_mut_ptr()) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            baseline.assume_init()
        };
        let mut mode = baseline;
        mode.c_lflag &= !(libc::ICANON | libc::ECHO);
        mode.c_cc[libc::VMIN] = 1;
        mode.c_cc[libc::VTIME] = 0;
        // SAFETY: mode is initialized and fd is live.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &mode) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Some(Self {
            file,
            baseline,
            signals,
        }))
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: The descriptor is still owned and baseline came from tcgetattr.
        unsafe {
            libc::tcsetattr(
                self.file.get_ref().as_raw_fd(),
                libc::TCSANOW,
                &self.baseline,
            )
        };
    }
}

#[cfg(feature = "gui")]
pub(super) fn run_window(path: PathBuf, config_path: &Path) -> Result<()> {
    use rivu_gui::{AuditionCommand, AuditionEvent};
    use std::rc::Rc;

    // Retain the engine until the window loop returns. Its destructor joins
    // the worker and must not block GPUI while handling a window event.
    let mut owner = None;
    let (config, loaded) = match Config::load(config_path) {
        Ok(config) => (config, Ok(())),
        Err(error) => (Config::default(), Err(error)),
    };
    let source = (|| {
        loaded?;
        let engine = Rc::new(start(path.clone(), None, PlaybackSpeed::NORMAL, &config)?);
        let events = engine
            .events
            .clone()
            .context("Audio event receiver unavailable")?;
        owner = Some(Rc::clone(&engine));
        let events = futures::stream::unfold(events, |events| async move {
            loop {
                let event = match events.recv().await.ok()? {
                    AudioEvent::Started { info, .. } => AuditionEvent::Started(info.duration),
                    AudioEvent::Progress {
                        seek_revision,
                        position_seconds,
                        ..
                    } => AuditionEvent::Position {
                        revision: seek_revision,
                        seconds: position_seconds,
                    },
                    AudioEvent::SeekRejected {
                        revision, snapshot, ..
                    } => AuditionEvent::Position {
                        revision,
                        seconds: snapshot.position_seconds,
                    },
                    AudioEvent::Ended { .. } => AuditionEvent::Ended,
                    AudioEvent::Failed { message, .. }
                    | AudioEvent::OutputSettingsFailed { message, .. } => {
                        AuditionEvent::Failed(message)
                    }
                    _ => continue,
                };
                return Some((event, events));
            }
        });
        let control = move |command| {
            let result = engine.send(match command {
                AuditionCommand::Pause(paused) => AudioCommand::Pause(paused),
                AuditionCommand::Seek { seconds, revision } => {
                    AudioCommand::Seek { seconds, revision }
                }
                AuditionCommand::Stop => AudioCommand::Stop,
                AuditionCommand::Speed(speed) => AudioCommand::Speed(
                    PlaybackSpeed::new(speed).context("Invalid audition speed")?,
                ),
            });
            match result {
                Ok(()) => Ok(true),
                Err(error) if error.is::<rivu_audio::SeekAfterEnd>() => Ok(false),
                Err(error) => Err(error),
            }
        };
        Ok((events, control))
    })();
    let result = rivu_gui::run_audition(path, config, source);
    drop(owner);
    result
}

fn start(
    path: PathBuf,
    device: Option<String>,
    speed: PlaybackSpeed,
    config: &Config,
) -> Result<AudioEngine> {
    let engine = AudioEngine::new(config.media_read_buffer_mib)?;
    engine.send(AudioCommand::VisualizationEnabled(false))?;
    engine.send(AudioCommand::Volume(config.volume))?;
    engine.send(AudioCommand::FfmpegEnabled(config.ffmpeg_enabled))?;
    engine.send(AudioCommand::OutputSettings {
        request: 0,
        device,
        auto_mix: config.pipewire_auto_mix,
    })?;
    if speed != PlaybackSpeed::NORMAL {
        engine.send(AudioCommand::Speed(speed))?;
    }
    engine.send(AudioCommand::Load {
        path,
        range: None,
        decoder_hint: Default::default(),
        metadata: Arc::new(PlaybackMetadata::default()),
        generation: 1,
        start_seconds: 0.0,
        paused: false,
    })?;
    Ok(engine)
}
