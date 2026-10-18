use crate::config::{AlbumArtworkOptions, AudioVisualizerOptions, Config, ConfigPatch};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Target {
    Global,
    Visualizer(u64),
    Artwork(u64),
}

pub(super) enum PanelApply {
    Visualizer {
        panel_id: u64,
        options: AudioVisualizerOptions,
        session: u64,
    },
    Artwork {
        panel_id: u64,
        options: AlbumArtworkOptions,
        session: u64,
    },
}

/// Consumed on completion; text snapshots and Input entities belong to the view.
pub(super) struct Submission {
    session: u64,
    config: Config,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Completion {
    Stale,
    Failed,
    Committed,
}

/// Owns edit-session transitions, not persistence or control text. Only the draft
/// is directly editable; callers cannot independently change lifecycle fields.
pub(super) struct SettingsModel {
    baseline: Arc<Config>,
    pub(super) draft: Config,
    session: u64,
    target: Target,
    applying: bool,
    feedback: Option<Result<(), String>>,
}

impl SettingsModel {
    pub(super) fn new(config: &Arc<Config>) -> Self {
        Self {
            baseline: Arc::clone(config),
            draft: config.as_ref().clone(),
            session: 0,
            target: Target::Global,
            applying: false,
            feedback: None,
        }
    }

    pub(super) fn session(&self) -> u64 {
        self.session
    }

    pub(super) fn target(&self) -> Target {
        self.target
    }

    pub(super) fn applying(&self) -> bool {
        self.applying
    }

    pub(super) fn feedback(&self) -> &Option<Result<(), String>> {
        &self.feedback
    }

    #[cfg(test)]
    pub(super) fn baseline(&self) -> &Config {
        &self.baseline
    }

    pub(super) fn reset(&mut self, config: &Arc<Config>) {
        self.session = self.session.wrapping_add(1);
        self.baseline = Arc::clone(config);
        self.draft = config.as_ref().clone();
        self.target = Target::Global;
        self.applying = false;
        self.feedback = None;
    }

    pub(super) fn reset_visualizer(
        &mut self,
        config: &Arc<Config>,
        panel_id: u64,
        options: AudioVisualizerOptions,
    ) {
        self.reset(config);
        self.target = Target::Visualizer(panel_id);
        self.draft.audio_visualizer_style = options.style;
        self.draft.audio_visualizer_grid = options.reference_lines;
        self.draft.audio_visualizer_labels = options.labels;
    }

    pub(super) fn reset_artwork(
        &mut self,
        config: &Arc<Config>,
        panel_id: u64,
        options: AlbumArtworkOptions,
    ) {
        self.reset(config);
        self.target = Target::Artwork(panel_id);
        self.draft.album_artwork_style = options.style;
        self.draft.album_artwork_mode = options.mode;
        self.draft.album_artwork_direction = options.direction;
        self.draft.album_artwork_period_seconds = options.period_seconds;
    }

    pub(super) fn reject_input(&mut self, error: String) {
        if !self.applying {
            self.feedback = Some(Err(error));
        }
    }

    pub(super) fn begin_apply(&mut self, config: Config) -> Option<(ConfigPatch, Submission)> {
        if self.applying || self.target != Target::Global {
            return None;
        }
        let patch = ConfigPatch::between(&self.baseline, &config);
        // Parsed controls establish the same origin as toggle/dropdown edits.
        self.draft = config.clone();
        self.applying = true;
        self.feedback = None;
        Some((
            patch,
            Submission {
                session: self.session,
                config,
            },
        ))
    }

    /// The view parses artwork's text; visualizer settings have no text payload.
    pub(super) fn begin_panel_apply(&mut self, artwork_period: Option<f32>) -> Option<PanelApply> {
        if self.applying {
            return None;
        }
        let request = match self.target {
            Target::Global => return None,
            Target::Visualizer(panel_id) => PanelApply::Visualizer {
                panel_id,
                options: AudioVisualizerOptions::from(&self.draft),
                session: self.session,
            },
            Target::Artwork(panel_id) => {
                let period_seconds = artwork_period?;
                if !period_seconds.is_finite() || period_seconds <= 0.0 {
                    return None;
                }
                PanelApply::Artwork {
                    panel_id,
                    options: AlbumArtworkOptions {
                        style: self.draft.album_artwork_style,
                        mode: self.draft.album_artwork_mode,
                        direction: self.draft.album_artwork_direction,
                        period_seconds,
                    },
                    session: self.session,
                }
            }
        };
        self.applying = true;
        self.feedback = None;
        Some(request)
    }

    pub(super) fn complete_apply(
        &mut self,
        submission: Submission,
        committed: Arc<Config>,
        result: Result<(), String>,
    ) -> Completion {
        if self.session != submission.session || self.target != Target::Global || !self.applying {
            return Completion::Stale;
        }
        self.applying = false;
        let completion = if result.is_ok() {
            let later_edits = ConfigPatch::between(&submission.config, &self.draft);
            self.draft = committed.as_ref().clone();
            later_edits.apply_to(&mut self.draft);
            self.baseline = committed;
            Completion::Committed
        } else {
            Completion::Failed
        };
        self.feedback = Some(result);
        completion
    }

    pub(super) fn complete_panel_apply(
        &mut self,
        session: u64,
        result: Result<(), String>,
    ) -> bool {
        if self.session != session || self.target == Target::Global || !self.applying {
            return false;
        }
        self.applying = false;
        self.feedback = Some(result);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AlbumArtworkMode, AudioVisualizerStyle};

    #[test]
    fn switching_targets_rejects_old_completions_and_routes_panel_options() {
        let config = Arc::new(Config::default());
        let mut model = SettingsModel::new(&config);
        let (_, global) = model.begin_apply(model.draft.clone()).unwrap();
        assert!(model.begin_apply(model.draft.clone()).is_none());
        assert!(!model.complete_panel_apply(model.session(), Ok(())));

        let visualizer = AudioVisualizerOptions {
            style: AudioVisualizerStyle::Rings,
            reference_lines: false,
            labels: true,
        };
        model.reset_visualizer(&config, 11, visualizer);
        assert_eq!(model.target(), Target::Visualizer(11));
        assert_eq!(AudioVisualizerOptions::from(&model.draft), visualizer);
        assert!(model.begin_apply(model.draft.clone()).is_none());
        let Some(PanelApply::Visualizer {
            panel_id,
            options,
            session,
        }) = model.begin_panel_apply(None)
        else {
            panic!("expected visualizer apply");
        };
        assert_eq!(panel_id, 11);
        assert_eq!(options, visualizer);
        assert_eq!(
            model.complete_apply(global, Arc::clone(&config), Ok(())),
            Completion::Stale,
        );
        assert!(model.applying());
        assert!(model.feedback().is_none());

        let artwork = AlbumArtworkOptions {
            mode: AlbumArtworkMode::Rotating,
            period_seconds: 13.0,
            ..AlbumArtworkOptions::from(config.as_ref())
        };
        model.reset_artwork(&config, 22, artwork);
        assert_eq!(model.target(), Target::Artwork(22));
        assert_eq!(AlbumArtworkOptions::from(&model.draft), artwork);
        assert_eq!(
            AudioVisualizerOptions::from(&model.draft),
            AudioVisualizerOptions::from(config.as_ref())
        );
        let Some(PanelApply::Artwork {
            panel_id,
            options,
            session: artwork_session,
        }) = model.begin_panel_apply(Some(17.0))
        else {
            panic!("expected artwork apply");
        };
        assert_eq!(panel_id, 22);
        assert_eq!(
            options,
            AlbumArtworkOptions {
                period_seconds: 17.0,
                ..artwork
            }
        );
        assert!(!model.complete_panel_apply(session, Err("old failure".into())));
        assert!(model.applying());
        assert!(model.feedback().is_none());
        assert!(model.complete_panel_apply(artwork_session, Ok(())));
        assert_eq!(model.feedback(), &Some(Ok(())));
        assert!(!model.complete_panel_apply(artwork_session, Err("duplicate".into())));

        model.reset(&config);
        assert_eq!(model.target(), Target::Global);
        assert_eq!(model.draft, *config);
        assert!(!model.applying());
        assert!(model.feedback().is_none());
        assert!(model.begin_panel_apply(Some(17.0)).is_none());
    }

    #[test]
    fn panel_validation_and_failure_preserve_edits_for_retry() {
        let config = Arc::new(Config::default());
        let mut model = SettingsModel::new(&config);
        model.reset_artwork(&config, 1, AlbumArtworkOptions::from(config.as_ref()));
        for period in [
            None,
            Some(0.0),
            Some(-1.0),
            Some(f32::NAN),
            Some(f32::INFINITY),
        ] {
            assert!(model.begin_panel_apply(period).is_none());
            assert!(!model.applying());
        }
        model.reject_input("invalid period".into());
        assert!(matches!(model.feedback(), Some(Err(_))));
        assert!(model.begin_panel_apply(Some(20.0)).is_some());
        assert!(model.feedback().is_none());
        assert!(model.begin_panel_apply(Some(30.0)).is_none());
        model.draft.album_artwork_mode = AlbumArtworkMode::Static;
        model.reject_input("must not replace pending feedback".into());
        assert!(model.feedback().is_none());
        assert!(model.complete_panel_apply(model.session(), Err("save failed".into())));
        assert!(!model.applying());
        assert!(matches!(model.feedback(), Some(Err(_))));
        assert_eq!(model.draft.album_artwork_mode, AlbumArtworkMode::Static);
        let Some(PanelApply::Artwork { options, .. }) = model.begin_panel_apply(Some(30.0)) else {
            panic!("expected retry");
        };
        assert_eq!(options.mode, AlbumArtworkMode::Static);
        assert_eq!(options.period_seconds, 30.0);
        assert!(model.feedback().is_none());
    }
}
