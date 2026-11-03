//! Root-owned preparation and decoding for explicit full-waveform requests.
use super::*;
use futures::future::BoxFuture;
use rivu_protocol::waveform::{PlaybackRange, WaveformFrame, WaveformScanParameters};
use std::sync::atomic::AtomicBool;

pub(crate) type WaveformScan =
    Box<dyn FnOnce(Track) -> BoxFuture<'static, Result<WaveformFrame>> + Send>;

impl CoreHandle {
    pub(crate) fn prepare_waveform_scan(
        &self,
        parameters: WaveformScanParameters,
        selected: bool,
        cancelled: Arc<AtomicBool>,
        workers: Arc<BlockingExecutor>,
    ) -> Result<WaveformScan> {
        // Register before any queued query/snapshot or future dispatch. A brief
        // disable/re-enable must still revoke an already admitted scan.
        self.waveform_control
            .cancel_scan_on_suspend(&self.waveform.read(), &cancelled);
        let snapshot = selected.then(|| self.waveform_snapshot()).transpose()?;
        Ok(Box::new(move |track| {
            Box::pin(async move {
                let track_range = track.cue.as_ref().map(|cue| PlaybackRange {
                    start_seconds: cue.start_seconds(),
                    end_seconds: cue.end_seconds(),
                });
                let snapshot = match snapshot {
                    Some(snapshot) => snapshot
                        .await
                        .context("Waveform source snapshot disconnected")?
                        .map_err(anyhow::Error::msg)?,
                    None => None,
                };
                let mut snapshot = snapshot.unwrap_or_else(|| {
                    audio::WaveformScanSnapshot::original(
                        track.path.clone(),
                        track_range,
                        track.decoder_hint,
                        parameters.signal,
                        Arc::clone(&parameters.equalizer),
                    )
                });
                // A processed/cached audition can have its own decoding source. The
                // saved hint belongs only to the selected original path and range.
                if snapshot.path == track.path && snapshot.range == track_range {
                    snapshot.decoder_hint = track.decoder_hint;
                }
                workers
                    .execute(move || {
                        let mut frame = audio::waveform(
                            &snapshot,
                            parameters.buffer_mib,
                            parameters.ffmpeg_enabled,
                            &cancelled,
                        )?;
                        frame.path = Some(track.path);
                        frame.range = track_range;
                        frame.decoder_hint = track.decoder_hint;
                        Ok(frame)
                    })
                    .await?
            })
        }))
    }
}
