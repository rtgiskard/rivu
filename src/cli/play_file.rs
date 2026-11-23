use anyhow::{Context, Result, bail};
use rivu::config::{Config, ConfigPersistence};
use rivu_audio::{AudioCommand, AudioEngine, AudioEvent, PlaybackMetadata};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) fn run(path: PathBuf, device: Option<String>, config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    let engine = AudioEngine::new(config.media_read_buffer_mib)?;
    engine.send(AudioCommand::VisualizationEnabled(false))?;
    engine.send(AudioCommand::Volume(config.volume))?;
    engine.send(AudioCommand::FfmpegEnabled(config.ffmpeg_enabled))?;
    engine.send(AudioCommand::OutputSettings {
        request: 0,
        device,
        auto_mix: config.pipewire_auto_mix,
    })?;
    engine.send(AudioCommand::Load {
        path,
        range: None,
        decoder_hint: Default::default(),
        metadata: Arc::new(PlaybackMetadata::default()),
        generation: 1,
        start_seconds: 0.0,
        paused: false,
    })?;

    let events = engine
        .events
        .as_ref()
        .context("Audio event receiver unavailable")?;
    loop {
        match events
            .recv_blocking()
            .context("Audio worker stopped before playback completed")?
        {
            AudioEvent::Ended { .. } => return Ok(()),
            AudioEvent::Failed { message, .. }
            | AudioEvent::OutputSettingsFailed { message, .. } => bail!("{message}"),
            _ => {}
        }
    }
}
