use super::*;

impl Core {
    pub(in crate::core) fn save(&mut self, force_config: bool) -> Result<()> {
        let config_changed = self.state.system.config.volume != self.state.playback.volume
            || self.state.system.config.shuffle != self.state.playback.shuffle
            || self.state.system.config.repeat != self.state.playback.repeat
            || self.state.system.config.output_device != self.state.system.selected_device;
        if self.config_dirty
            && config_changed
            && (force_config || self.config_last_saved.elapsed() >= Duration::from_millis(250))
        {
            let mut config = self.state.system.config.as_ref().clone();
            config.volume = self.state.playback.volume;
            config.shuffle = self.state.playback.shuffle;
            config.repeat = self.state.playback.repeat;
            config
                .output_device
                .clone_from(&self.state.system.selected_device);
            config.save(&self.state.system.config_path)?;
            self.state.system.config = Arc::new(config);
            self.config_last_saved = Instant::now();
            self.config_dirty = false;
        } else if !config_changed {
            self.config_dirty = false;
        }
        if self.queue_dirty {
            let saved = Saved {
                queue: self.state.queue.entries.as_ref().clone(),
                current: self.state.queue.current_id,
            };
            self.store
                .set_setting("playback", &serde_json::to_string(&saved)?)?;
            self.queue_dirty = false;
        }
        Ok(())
    }
    pub(in crate::core) fn reload_library(&mut self, structure_changed: bool) -> Result<()> {
        let tracks = self.store.tracks()?;
        self.state.library.tracks = self.library.replace(tracks);
        self.state.library.revision = self.state.library.revision.wrapping_add(1);
        if structure_changed {
            self.state.library.structure_revision =
                self.state.library.structure_revision.wrapping_add(1);
        }
        Ok(())
    }

    pub(in crate::core) fn update_track_stats(
        &mut self,
        track_id: i64,
        played_at: Option<i64>,
        increment_play_count: bool,
    ) -> Result<()> {
        self.library.update(track_id, |track| {
            if let Some(played_at) = played_at {
                track.last_played = Some(played_at);
            }
            if increment_play_count {
                track.play_count = track.play_count.saturating_add(1);
            }
        })
    }
    pub(in crate::core) fn reload(&mut self, structure_changed: bool) -> Result<()> {
        self.reload_library(structure_changed)?;
        self.state.library.playlists = Arc::new(self.store.playlists()?);
        self.state.library.history = Arc::new(self.store.history(200)?);
        Ok(())
    }
}
