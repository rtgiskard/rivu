use super::*;
use anyhow::bail;

impl Core {
    pub(in crate::core) fn database_outcome_uncertain(&mut self) {
        self.database_unavailable = true;
        self.pending_navigation = None;
        self.store.invalidate_caches();
        self.state.system.last_error = Some(
            "Database commit outcome is uncertain; database operations are disabled until Rivu restarts".into(),
        );
    }
    pub(in crate::core) fn database_maintenance_active(&self) -> bool {
        self.database_unavailable
            || self.active_optimization.is_some()
            || self
                .active_scan
                .as_ref()
                .is_some_and(|task| task.status.progress.phase == ScanPhase::Saving)
            || self
                .active_external_admission
                .as_ref()
                .is_some_and(|task| task.is_saving())
    }

    /// Maintenance never admits a command that can wait on its writer connection.
    /// Returns true when an intent for deferred EOF navigation was handled here.
    pub(in crate::core) fn maintenance_control(&mut self, command: &Command) -> Result<bool> {
        match command {
            Command::Stop => self.pending_navigation = None,
            Command::Pause => {
                if let Some(status) = &mut self.pending_navigation {
                    *status = PlaybackStatus::Paused;
                }
            }
            Command::Resume | Command::Toggle => {
                if let Some(status) = &mut self.pending_navigation {
                    *status = if matches!(command, Command::Toggle)
                        && *status == PlaybackStatus::Playing
                    {
                        PlaybackStatus::Paused
                    } else {
                        PlaybackStatus::Playing
                    };
                    return Ok(true);
                }
                if self.state.playback.status == PlaybackStatus::Stopped {
                    bail!(
                        "Database maintenance is in progress; starting playback is unavailable until it finishes"
                    );
                }
            }
            Command::Volume { .. }
            | Command::AdjustVolume { .. }
            | Command::Seek { .. }
            | Command::SeekQueue { .. }
            | Command::SeekRelative { .. }
            | Command::Shuffle { .. }
            | Command::ToggleShuffle
            | Command::Repeat { .. }
            | Command::CycleRepeat
            | Command::Analysis { .. }
            | Command::DismissError
            | Command::DismissScanResult
            | Command::CancelScan { .. }
            | Command::CancelRelocation { .. }
            | Command::ProcessingBypass
            | Command::CancelProcessing { .. }
            | Command::MprisStatus { .. } => {}
            _ => bail!("Database maintenance is in progress; this operation was not executed"),
        }
        Ok(false)
    }
}
