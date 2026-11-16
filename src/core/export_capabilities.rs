use super::*;

/// Explicit, coalesced discovery; ordinary startup never loads encoding libraries.
pub(super) struct EncoderProbe {
    pub(super) finished: oneshot::Receiver<Result<Vec<AudioExportFormat>>>,
    worker: Option<async_executor::Task<()>>,
}

impl EncoderProbe {
    fn join(&mut self) -> Result<()> {
        retire_optional_worker(&mut self.worker, "Audio encoder discovery worker panicked")
    }
}

impl Drop for EncoderProbe {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

impl Core {
    pub(super) fn probe_audio_export_formats(&mut self) {
        if self.encoder_probe.is_some() || self.state.system.export_formats.is_some() {
            return;
        }
        let (completed, finished) = oneshot::channel();
        let workers = Arc::clone(&self.workers);
        let worker = self.executor.spawn(async move {
            let result = workers
                .execute(|| {
                    [
                        AudioExportFormat::Wav,
                        AudioExportFormat::Flac,
                        AudioExportFormat::Opus,
                        AudioExportFormat::Mp3,
                    ]
                    .into_iter()
                    .filter(|format| audio::export_encoder_available(*format))
                    .collect()
                })
                .await;
            let _ = completed.send(result);
        });
        self.encoder_probe = Some(EncoderProbe {
            finished,
            worker: Some(worker),
        });
    }

    pub(super) fn finish_encoder_probe(&mut self, result: Result<Vec<AudioExportFormat>>) {
        if let Some(mut probe) = self.encoder_probe.take() {
            match probe.join().and(result) {
                Ok(formats) => self.state.system.export_formats = Some(formats),
                Err(error) => {
                    self.state.system.last_error =
                        Some(format!("Discovering audio encoders: {error:#}"));
                }
            }
            self.publish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_completion_reaches_system_subscribers_without_playback_changes() {
        let (_directory, mut core) = super::super::tests::fixture_with_database(Some(1.0));
        core.publish();
        let before = core.shared.read().clone();
        let (_sender, finished) = oneshot::channel();
        core.encoder_probe = Some(EncoderProbe {
            finished,
            worker: None,
        });
        core.finish_encoder_probe(Ok(vec![AudioExportFormat::Wav]));
        let after = core.shared.read();
        assert_eq!(
            after.system.export_formats,
            Some(vec![AudioExportFormat::Wav])
        );
        assert!(after.revisions.system > before.revisions.system);
        assert_eq!(after.revisions.playback, before.revisions.playback);
        assert_eq!(after.revisions.processing, before.revisions.processing);
    }
}
