//! Window-local visualization demand, frame consumption, and panel invalidation.
//! Business state arrives through `sync`; the animation clock never reads or
//! refreshes the application root. Panel entities are the only redraw targets.

use super::{layout, visualization};
use crate::{
    analysis::{AnalysisFrame, DisplayDemand, VisualizerDemand},
    config::{AudioVisualizerOptions, AudioVisualizerStyle, Config},
    core::CoreHandle,
    i18n::Localizer,
    model::{PlaybackStatus, Track},
};
use gpui::{
    AnyElement, Context, Entity, Render, Subscription, Task, WeakEntity, Window, div, prelude::*,
    rgb,
};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelKind {
    Spectrum,
    Spectrogram,
    Audio(AudioVisualizerOptions),
    Waveform,
}

impl PanelKind {
    fn from_panel(panel: &layout::Panel, config: &Config) -> Option<Self> {
        match panel.kind.as_str() {
            "spectrum" => Some(Self::Spectrum),
            "spectrogram" => Some(Self::Spectrogram),
            "audio_visualizer" => Some(Self::Audio(
                panel.audio_visualizer.unwrap_or_else(|| config.into()),
            )),
            "waveform" => Some(Self::Waveform),
            _ => None,
        }
    }

    fn analysis(self) -> bool {
        !matches!(self, Self::Waveform)
    }
}

pub(super) struct PresentationState<'a> {
    pub(super) config: Arc<Config>,
    pub(super) panels: &'a [&'a layout::Panel],
    pub(super) status: PlaybackStatus,
    pub(super) visible: bool,
    pub(super) shutting_down: bool,
    pub(super) track: Option<&'a Track>,
    pub(super) position: f64,
    pub(super) duration: Option<f64>,
}

pub(super) struct Presentation {
    handle: CoreHandle,
    i18n: Arc<Localizer>,
    config: Arc<Config>,
    visuals: visualization::Visuals,
    audio_visualizer: visualization::AudioVisualizer,
    waveform: Entity<visualization::Waveform>,
    panels: HashMap<u64, PanelKind>,
    views: HashMap<u64, Entity<PresentationPanel>>,
    demand: DisplayDemand,
    sequence: u64,
    staging: AnalysisFrame,
    needs_refresh: bool,
    visible: bool,
    shutting_down: bool,
    status: PlaybackStatus,
    position: f64,
    duration: Option<f64>,
    frame_requested: bool,
    frame_pending: bool,
    frame_generation: u64,
    frame_timer: Option<Task<()>>,
    last_request: Option<Instant>,
    _waveform_subscription: Subscription,
}

impl Presentation {
    pub(super) fn new(
        handle: CoreHandle,
        i18n: Arc<Localizer>,
        waveform: Entity<visualization::Waveform>,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = handle.config_snapshot();
        let subscription = cx.observe(&waveform, |this, _, cx| {
            this.notify_panels(false, cx);
        });
        Self {
            handle,
            i18n,
            config,
            visuals: visualization::Visuals::new(),
            audio_visualizer: visualization::AudioVisualizer::new(),
            waveform,
            panels: HashMap::new(),
            views: HashMap::new(),
            demand: DisplayDemand::default(),
            sequence: 0,
            staging: AnalysisFrame::default(),
            needs_refresh: false,
            visible: false,
            shutting_down: false,
            status: PlaybackStatus::Stopped,
            position: 0.0,
            duration: None,
            frame_requested: false,
            frame_pending: false,
            frame_generation: 0,
            frame_timer: None,
            last_request: None,
            _waveform_subscription: subscription,
        }
    }

    pub(super) fn sync(&mut self, state: PresentationState<'_>, cx: &mut Context<Self>) {
        let PresentationState {
            config,
            panels,
            status,
            visible,
            shutting_down,
            track,
            position,
            duration,
        } = state;
        let config_changed = !Arc::ptr_eq(&self.config, &config);
        let still_changed = self.config.still_mode != config.still_mode;
        let visibility_changed = self.visible != visible || self.shutting_down != shutting_down;
        let status_changed = self.status != status;
        let position_changed = self.position != position || self.duration != duration;
        let panels_changed = self.panels.len()
            != panels
                .iter()
                .filter(|panel| PanelKind::from_panel(panel, &config).is_some())
                .count()
            || panels.iter().any(|panel| {
                PanelKind::from_panel(panel, &config)
                    .is_some_and(|kind| self.panels.get(&panel.id) != Some(&kind))
            });
        let was_running = self.frame_interval();
        self.config = config;
        self.visible = visible;
        self.shutting_down = shutting_down;
        self.status = status;
        self.position = position;
        self.duration = duration;

        if panels_changed {
            self.panels.clear();
            self.panels.extend(panels.iter().filter_map(|panel| {
                PanelKind::from_panel(panel, &self.config).map(|kind| (panel.id, kind))
            }));
            self.views.retain(|id, _| self.panels.contains_key(id));
        }
        if still_changed {
            // A mode transition must never redisplay the pre-suspension signal.
            self.staging = AnalysisFrame::default();
            self.sequence = self.handle.analysis.read().sequence;
            self.needs_refresh = false;
        }
        if config_changed || panels_changed || visibility_changed {
            let enabled = self.active();
            let active_panels = if enabled { panels } else { &[] };
            self.visuals.set_enabled(enabled);
            let spectrum_ids = active_panels
                .iter()
                .filter(|panel| panel.kind == "spectrum")
                .map(|panel| panel.id)
                .collect::<Vec<_>>();
            let spectrogram_ids = active_panels
                .iter()
                .filter(|panel| panel.kind == "spectrogram")
                .map(|panel| panel.id)
                .collect::<Vec<_>>();
            self.visuals.retain_spectrum_panels(&spectrum_ids);
            self.visuals.retain_spectrogram_panels(&spectrogram_ids);
            let spectrum_restored = self.visuals.set_spectrum_visible(!spectrum_ids.is_empty());
            let audio_changed = self
                .audio_visualizer
                .sync(&self.config, active_panels, enabled);
            let mut visualizer = VisualizerDemand::default();
            for kind in self.panels.values().filter(|_| enabled) {
                if let PanelKind::Audio(options) = kind {
                    match options.style {
                        AudioVisualizerStyle::StereoDensity => visualizer.stereo_density = true,
                        AudioVisualizerStyle::SpatialSpectrum => visualizer.spatial_spectrum = true,
                        AudioVisualizerStyle::FractalBeams | AudioVisualizerStyle::FractalFlow => {
                            visualizer.band_energy = true;
                        }
                        _ => {}
                    }
                }
            }
            let demand = DisplayDemand {
                enabled: enabled && self.panels.values().any(|kind| kind.analysis()),
                visualizer,
            };
            if demand.enabled {
                let spectrum_changed = self.visuals.configure(&self.config);
                self.needs_refresh |= spectrum_restored || spectrum_changed || audio_changed;
            } else {
                self.needs_refresh = false;
            }
            retain_analysis_staging(&mut self.staging, demand);
            self.handle.set_display_demand(demand);
            if self.demand != demand {
                self.cancel_clock();
            }
            self.demand = demand;
        }
        if was_running != self.frame_interval() || visibility_changed || still_changed {
            self.cancel_clock();
        }
        let waveform_enabled = !self.config.still_mode && !self.shutting_down;
        let waveform_visible = self.active();
        self.waveform.update(cx, |waveform, cx| {
            waveform.retain_panels(if waveform_visible { panels } else { &[] }, cx);
            // Visibility retires rendering resources, not explicit scan results.
            waveform.set_enabled(waveform_enabled);
            waveform.set_seek_enabled(waveform_visible && self.config.waveform_seek_enabled);
            if waveform_enabled {
                waveform.sync(track, cx);
            }
        });
        if config_changed || panels_changed || visibility_changed || status_changed {
            self.frame_requested = self.demand.enabled;
            self.notify_panels(true, cx);
            self.notify_panels(false, cx);
        } else if position_changed {
            self.notify_panels(false, cx);
        }
    }

    fn active(&self) -> bool {
        self.visible && !self.shutting_down && !self.config.still_mode
    }

    fn frame_interval(&self) -> Option<Duration> {
        (self.active() && self.demand.enabled && self.status == PlaybackStatus::Playing)
            .then(|| Duration::from_secs_f64(1.0 / self.config.analysis_fps as f64))
    }

    fn cancel_clock(&mut self) {
        self.frame_generation = self.frame_generation.wrapping_add(1);
        self.frame_timer.take();
        self.last_request = None;
        self.frame_requested = false;
        // Native callbacks cannot be removed. Keep their pending slot occupied
        // until delivery; the generation check retires them without consumption.
    }

    pub(super) fn request_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.active()
            || !self.demand.enabled
            || (!self.frame_requested && self.frame_interval().is_none())
            || self.frame_pending
            || self.frame_timer.is_some()
        {
            return;
        }
        self.frame_pending = true;
        let generation = self.frame_generation;
        let entity = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = entity.update(cx, |this, cx| {
                this.frame_pending = false;
                if generation != this.frame_generation {
                    this.request_frame(window, cx);
                    return;
                }
                if !this.active() || !this.demand.enabled {
                    return;
                }
                this.frame_requested = false;
                this.consume_analysis(cx);
                let Some(interval) = this.frame_interval() else {
                    this.last_request = None;
                    return;
                };
                let now = Instant::now();
                if this.last_request.is_none_or(|last| now - last >= interval) {
                    this.last_request = Some(now);
                    this.handle.request_analysis_frame();
                }
                let delay = interval.saturating_sub(this.last_request.unwrap().elapsed());
                let timer = cx.background_executor().timer(delay);
                this.frame_timer = Some(cx.spawn_in(window, async move |this, cx| {
                    timer.await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        if this.frame_generation == generation {
                            this.frame_timer = None;
                            this.request_frame(window, cx);
                        }
                    });
                }));
            });
        });
        // Dirty a visual view to obtain a real native frame, never the business
        // root. A pending timer and native callback share this single gate.
        self.notify_panels(true, cx);
    }

    fn consume_analysis(&mut self, cx: &mut Context<Self>) {
        let frame = self.handle.analysis.read();
        if !stage_analysis_frame(
            &mut self.staging,
            &mut self.sequence,
            self.needs_refresh,
            &frame,
            self.handle.analysis_frame_is_current(&frame),
        ) {
            return;
        }
        drop(frame);
        self.needs_refresh = false;
        self.visuals.update(&self.staging);
        self.audio_visualizer.update(&self.staging);
        self.notify_panels(true, cx);
    }

    fn notify_panels(&self, analysis: bool, cx: &mut Context<Self>) {
        for (id, view) in &self.views {
            if self
                .panels
                .get(id)
                .is_some_and(|kind| kind.analysis() == analysis)
            {
                view.update(cx, |_, cx| cx.notify());
            }
        }
    }

    pub(super) fn panel(&mut self, id: u64, cx: &mut Context<Self>) -> Entity<PresentationPanel> {
        self.views
            .entry(id)
            .or_insert_with(|| {
                let owner = cx.weak_entity();
                cx.new(|_| PresentationPanel { owner, id })
            })
            .clone()
    }

    fn render_panel(&mut self, id: u64, cx: &mut Context<Self>) -> AnyElement {
        if !self.active() {
            return div()
                .size_full()
                .bg(rgb(self.config.visual_background.rgb()))
                .into_any_element();
        }
        let theme = rivu_ui::theme::palette(cx);
        match self.panels.get(&id).copied() {
            Some(PanelKind::Spectrum) => self.visuals.spectrum(theme, &self.i18n, id),
            Some(PanelKind::Spectrogram) => self.visuals.spectrogram(theme, &self.i18n, id),
            Some(PanelKind::Audio(options)) => self.audio_visualizer.view(
                &self.i18n,
                id,
                self.status == PlaybackStatus::Playing,
                options,
            ),
            Some(PanelKind::Waveform) => self.waveform.update(cx, |waveform, cx| {
                waveform.view(
                    &self.i18n,
                    id,
                    self.position,
                    self.duration,
                    &self.config,
                    cx,
                )
            }),
            None => div().size_full().into_any_element(),
        }
    }

    /// Called by the shell's first paint, after every panel's prepaint. Keeping
    /// this hook outside panel render also retires images after panel removal.
    pub(super) fn finish_paint(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.visuals.release_retired(window);
        self.waveform
            .read(cx)
            .image_releaser()
            .release_retired(window);
        self.audio_visualizer.release_retired_images(window);
        if self.visuals.finish_prepaint() {
            // A resized panel can alter history shared by cached siblings.
            // Repaint them before the next retirement boundary.
            for (id, view) in &self.views {
                if self.panels.get(id) == Some(&PanelKind::Spectrogram) {
                    view.update(cx, |_, cx| cx.notify());
                }
            }
        }
    }

    /// Hidden/destroyed windows cannot rely on another paint for retirement.
    pub(super) fn release_render_images(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = false;
        self.cancel_clock();
        self.demand = DisplayDemand::default();
        self.handle.set_display_demand(self.demand);
        self.staging = AnalysisFrame::default();
        self.needs_refresh = false;
        self.visuals.set_enabled(false);
        self.visuals.retain_spectrogram_panels(&[]);
        self.visuals.retain_spectrum_panels(&[]);
        self.visuals.set_spectrum_visible(false);
        self.audio_visualizer.sync(&self.config, &[], false);
        self.waveform.update(cx, |waveform, cx| {
            waveform.retain_panels(&[], cx);
        });
        self.audio_visualizer.release_all_images(window);
        self.visuals.release_all_images(window);
        self.waveform
            .read(cx)
            .image_releaser()
            .release_all_images(window);
    }
}

impl Drop for Presentation {
    fn drop(&mut self) {
        self.handle.set_display_demand(DisplayDemand::default());
    }
}

pub(super) struct PresentationPanel {
    owner: WeakEntity<Presentation>,
    id: u64,
}

impl Render for PresentationPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.owner
            .update(cx, |owner, cx| owner.render_panel(self.id, cx))
            .unwrap_or_else(|_| div().into_any_element())
    }
}

pub(super) fn retain_analysis_staging(frame: &mut AnalysisFrame, demand: DisplayDemand) {
    if !demand.enabled {
        *frame = AnalysisFrame::default();
        return;
    }
    // Retain allocations still demanded by another panel, but release optional
    // snapshots immediately rather than waiting for the next worker publication.
    let visualizer = &mut frame.visualizer;
    if !demand.visualizer.stereo_density {
        visualizer.demand.stereo_density = false;
        visualizer.density = crate::analysis::DensitySnapshot::default();
        visualizer.correlation = None;
        visualizer.balance = None;
        visualizer.side_ratio = None;
    }
    if !demand.visualizer.spatial_spectrum {
        visualizer.demand.spatial_spectrum = false;
        visualizer.spatial = Vec::new();
    }
    if !demand.visualizer.band_energy {
        visualizer.demand.band_energy = false;
        visualizer.band_rms = [0.0; 3];
    }
}

pub(super) fn stage_analysis_frame(
    staging: &mut AnalysisFrame,
    sequence: &mut u64,
    needs_refresh: bool,
    frame: &AnalysisFrame,
    frame_is_current: bool,
) -> bool {
    if !frame_is_current
        || (frame.sequence == *sequence && !(needs_refresh && staging.settings.is_some()))
    {
        return false;
    }
    staging.frequencies_hz.clone_from(&frame.frequencies_hz);
    staging.spectrum_db.clone_from(&frame.spectrum_db);
    staging.sequence = frame.sequence;
    staging.display_token = frame.display_token;
    staging.sample_rate = frame.sample_rate;
    staging.sample_time = frame.sample_time;
    staging.generation = frame.generation;
    staging.settings = frame.settings;
    staging.rms_left = frame.rms_left;
    staging.rms_right = frame.rms_right;
    staging.visualizer.clone_from(&frame.visualizer);
    *sequence = frame.sequence;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Runtime;

    #[gpui::test]
    fn presentation_bounds_frames_and_cancels_hidden_still_and_shutdown_demand(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            ..Config::default()
        }
        .save(&config_path)
        .unwrap();
        let runtime = Runtime::start(directory.path(), &config_path).unwrap();
        let config = runtime.handle.config_snapshot();
        let window = cx.add_empty_window();
        let owner = window.update(|_, cx| {
            let waveform =
                cx.new(|_| visualization::Waveform::new(Arc::clone(&runtime.handle.waveform)));
            cx.new(|cx| {
                Presentation::new(
                    runtime.handle.clone(),
                    Arc::new(Localizer::new("en-US")),
                    waveform,
                    cx,
                )
            })
        });
        let panel = layout::Panel {
            id: 1,
            kind: "spectrogram".into(),
            show_tab_bar: false,
            audio_visualizer: None,
            album_artwork: None,
        };
        window.update(|window, cx| {
            owner.update(cx, |owner, cx| {
                owner.sync(
                    PresentationState {
                        config: Arc::clone(&config),
                        panels: &[&panel],
                        status: PlaybackStatus::Paused,
                        visible: true,
                        shutting_down: false,
                        track: None,
                        position: 0.0,
                        duration: None,
                    },
                    cx,
                );
                let view = owner.panel(panel.id, cx);
                assert_eq!(view.entity_id(), owner.panel(panel.id, cx).entity_id());
                owner.request_frame(window, cx);
                assert!(owner.frame_pending);
                let generation = owner.frame_generation;
                owner.request_frame(window, cx);
                assert_eq!(owner.frame_generation, generation);
                assert!(owner.frame_timer.is_none());
                owner.sync(
                    PresentationState {
                        config: Arc::clone(&config),
                        panels: &[&panel],
                        status: PlaybackStatus::Paused,
                        visible: false,
                        shutting_down: false,
                        track: None,
                        position: 0.0,
                        duration: None,
                    },
                    cx,
                );
                assert_eq!(owner.demand, DisplayDemand::default());
                assert!(owner.frame_timer.is_none());
                assert!(
                    owner.frame_pending,
                    "native callback keeps its slot until delivery"
                );
                assert_ne!(owner.frame_generation, generation);
            });
            window.simulate_next_frame(cx);
            owner.update(cx, |owner, cx| {
                assert!(!owner.frame_pending);
                owner.sync(
                    PresentationState {
                        config: Arc::clone(&config),
                        panels: &[&panel],
                        status: PlaybackStatus::Paused,
                        visible: true,
                        shutting_down: false,
                        track: None,
                        position: 0.0,
                        duration: None,
                    },
                    cx,
                );
                owner.request_frame(window, cx);
            });
            window.simulate_next_frame(cx);
            owner.update(cx, |owner, cx| {
                assert!(!owner.frame_pending);
                assert!(!owner.frame_requested);
                owner.request_frame(window, cx);
                assert!(
                    !owner.frame_pending,
                    "paused paints must not start another clock"
                );
                owner.sync(
                    PresentationState {
                        config: Arc::clone(&config),
                        panels: &[&panel],
                        status: PlaybackStatus::Playing,
                        visible: true,
                        shutting_down: false,
                        track: None,
                        position: 0.0,
                        duration: None,
                    },
                    cx,
                );
                owner.request_frame(window, cx);
            });
            window.simulate_next_frame(cx);
            owner.update(cx, |owner, cx| {
                assert!(owner.frame_timer.is_some());
                let mut still = config.as_ref().clone();
                still.still_mode = true;
                owner.sync(
                    PresentationState {
                        config: Arc::new(still),
                        panels: &[&panel],
                        status: PlaybackStatus::Playing,
                        visible: true,
                        shutting_down: false,
                        track: None,
                        position: 0.0,
                        duration: None,
                    },
                    cx,
                );
                assert_eq!(owner.demand, DisplayDemand::default());
                assert!(owner.frame_timer.is_none());
                assert!(owner.frame_interval().is_none());
                assert_eq!(owner.staging.settings, None);
                owner.sync(
                    PresentationState {
                        config: Arc::clone(&config),
                        panels: &[&panel],
                        status: PlaybackStatus::Playing,
                        visible: true,
                        shutting_down: false,
                        track: None,
                        position: 0.0,
                        duration: None,
                    },
                    cx,
                );
                assert!(owner.demand.enabled);
                owner.sync(
                    PresentationState {
                        config: Arc::clone(&config),
                        panels: &[&panel],
                        status: PlaybackStatus::Playing,
                        visible: true,
                        shutting_down: true,
                        track: None,
                        position: 0.0,
                        duration: None,
                    },
                    cx,
                );
                assert_eq!(owner.demand, DisplayDemand::default());
                assert!(owner.frame_timer.is_none());
                owner.sync(
                    PresentationState {
                        config: Arc::clone(&config),
                        panels: &[],
                        status: PlaybackStatus::Stopped,
                        visible: true,
                        shutting_down: false,
                        track: None,
                        position: 0.0,
                        duration: None,
                    },
                    cx,
                );
                assert!(owner.views.is_empty());
                owner.release_render_images(window, cx);
                assert_eq!(owner.demand, DisplayDemand::default());
            });
        });
    }
}
