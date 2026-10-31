//! Window-local visualization demand, frame consumption, and panel invalidation.
//! Business state arrives through `sync`; the animation clock never reads or
//! refreshes the application root. Panel entities are the only redraw targets.

#[cfg(test)]
use crate::config::ConfigPersistence;
use crate::gui::{animation_frames::AnimationFrames, layout, visualization};
use crate::{
    config::{AudioVisualizerOptions, AudioVisualizerStyle, Config},
    core::CoreHandle,
    i18n::Localizer,
};
use gpui::{
    AnyElement, Context, Entity, Render, Subscription, Task, WeakEntity, Window, div, prelude::*,
    rgb,
};
use rivu_audio::analysis::{AnalysisFrame, DisplayDemand, VisualizerDemand};
use rivu_protocol::model::{PlaybackStatus, Track};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelKind {
    Spectrum(AudioVisualizerOptions),
    Spectrogram(AudioVisualizerOptions),
    Audio(AudioVisualizerOptions),
    Waveform(AudioVisualizerOptions),
}

impl PanelKind {
    fn from_panel(panel: &layout::Panel, config: &Config) -> Option<Self> {
        if panel.kind != "audio_visualizer" {
            return None;
        }
        let options = panel.audio_visualizer.unwrap_or_else(|| config.into());
        Some(match options.style {
            AudioVisualizerStyle::Spectrum => Self::Spectrum(options),
            AudioVisualizerStyle::Spectrogram => Self::Spectrogram(options),
            AudioVisualizerStyle::Waveform => Self::Waveform(options),
            _ => Self::Audio(options),
        })
    }

    fn analysis(self) -> bool {
        !matches!(self, Self::Waveform(_))
    }
}

pub(in crate::gui) struct VisualizerInputs<'a> {
    pub(in crate::gui) config: Arc<Config>,
    pub(in crate::gui) panels: &'a crate::gui::workspace::WorkspacePanels,
    pub(in crate::gui) status: PlaybackStatus,
    pub(in crate::gui) source_generation: u64,
    pub(in crate::gui) seek_revision: u64,
    pub(in crate::gui) visible: bool,
    pub(in crate::gui) shutting_down: bool,
    pub(in crate::gui) current_track_id: Option<i64>,
    pub(in crate::gui) waveform_track: Option<&'a Track>,
    pub(in crate::gui) position: f64,
    pub(in crate::gui) duration: Option<f64>,
}

/// Display time consumes accepted audio time at wall-clock speed, never inventing
/// samples during a stalled producer or catching up a hidden interval in one draw.
#[derive(Default)]
struct AnimationClock {
    source: Option<(u64, u32)>,
    latest: Option<Duration>,
    time: Duration,
    last_tick: Option<Instant>,
}

impl AnimationClock {
    fn accept(&mut self, frame: &AnalysisFrame, now: Instant) {
        if frame.settings.is_none() {
            *self = Self::default();
            return;
        }
        let source = (frame.generation, frame.sample_rate);
        let discontinuity = self.source != Some(source)
            || self.last_tick.is_none()
            || self.latest.is_none_or(|last| frame.sample_time < last);
        if discontinuity {
            self.time = frame.sample_time;
            self.last_tick = Some(now);
        }
        self.source = Some(source);
        self.latest = Some(frame.sample_time);
    }

    fn advance(&mut self, now: Instant) -> Duration {
        let elapsed = self
            .last_tick
            .replace(now)
            .map(|last| now.saturating_duration_since(last))
            .unwrap_or_default();
        if let Some(latest) = self.latest {
            self.time = self.time.saturating_add(elapsed).min(latest);
        }
        self.time
    }

    fn suspend(&mut self) {
        self.last_tick = None;
    }
}

pub(in crate::gui) struct VisualizerRuntime {
    handle: CoreHandle,
    labels: Arc<visualization::VisualLabels>,
    parameters: visualization::VisualParameters,
    config: Arc<Config>,
    frequency_views: visualization::FrequencyViews,
    style_coordinator: visualization::StyleCoordinator,
    waveform: Entity<visualization::Waveform>,
    panels: HashMap<u64, PanelKind>,
    layout_panels: HashMap<u64, PanelKind>,
    views: HashMap<u64, Entity<VisualizerPanel>>,
    panel_revision: u64,
    demand: DisplayDemand,
    sequence: u64,
    staging: AnalysisFrame,
    needs_refresh: bool,
    visible: bool,
    shutting_down: bool,
    status: PlaybackStatus,
    source_generation: u64,
    seek_revision: u64,
    current_track_id: Option<i64>,
    awaiting_replacement: bool,
    position: f64,
    duration: Option<f64>,
    frame_requested: bool,
    frame_pending: bool,
    frame_generation: u64,
    frames: Entity<AnimationFrames>,
    analysis_timer: Option<Task<()>>,
    animation_clock: AnimationClock,
    _waveform_subscription: Subscription,
}

impl VisualizerRuntime {
    pub(in crate::gui) fn new(
        handle: CoreHandle,
        i18n: Arc<Localizer>,
        waveform: Entity<visualization::Waveform>,
        frames: Entity<AnimationFrames>,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = handle.config_snapshot();
        let subscription = cx.observe(&waveform, |this, _, cx| {
            this.notify_panels(false, cx);
        });
        Self {
            handle,
            labels: visualization::visual_labels(&i18n),
            parameters: visualization::VisualParameters::from(config.as_ref()),
            config,
            frequency_views: visualization::FrequencyViews::new(),
            style_coordinator: visualization::StyleCoordinator::new(),
            waveform,
            panels: HashMap::new(),
            layout_panels: HashMap::new(),
            views: HashMap::new(),
            panel_revision: 0,
            demand: DisplayDemand::default(),
            sequence: 0,
            staging: AnalysisFrame::default(),
            needs_refresh: false,
            visible: false,
            shutting_down: false,
            status: PlaybackStatus::Stopped,
            source_generation: 0,
            seek_revision: 0,
            current_track_id: None,
            awaiting_replacement: false,
            position: 0.0,
            duration: None,
            frame_requested: false,
            frame_pending: false,
            frame_generation: 0,
            frames,
            analysis_timer: None,
            animation_clock: AnimationClock::default(),
            _waveform_subscription: subscription,
        }
    }

    pub(in crate::gui) fn sync(&mut self, state: VisualizerInputs<'_>, cx: &mut Context<Self>) {
        let VisualizerInputs {
            config,
            panels,
            status,
            source_generation,
            seek_revision,
            visible,
            shutting_down,
            current_track_id,
            waveform_track,
            position,
            duration,
        } = state;
        let config_changed = !Arc::ptr_eq(&self.config, &config);
        if config_changed {
            self.parameters = visualization::VisualParameters::from(config.as_ref());
        }
        let visualization_changed =
            self.config.visualization_enabled != config.visualization_enabled;
        let fps_changed = self.config.visualization_fps != config.visualization_fps;
        let visibility_changed = self.visible != visible || self.shutting_down != shutting_down;
        let status_changed = self.status != status;
        let position_changed = self.position != position || self.duration != duration;
        let settings_changed = rivu_audio::analysis::AnalysisSettings::from(self.config.as_ref())
            != rivu_audio::analysis::AnalysisSettings::from(config.as_ref());
        let signal_changed = self.source_generation != source_generation
            || self.seek_revision != seek_revision
            || settings_changed;
        let track_changed = self.current_track_id != current_track_id;
        let stopping = status_changed && status == PlaybackStatus::Stopped;
        // Only an exact Core Stop fence may retire the old picture with a fade.
        // Coalesced seeks or source changes must not cross this boundary.
        let fade_on_stop = stopping
            && current_track_id.is_some()
            && !track_changed
            && source_generation == self.source_generation.wrapping_add(1)
            && seek_revision == self.seek_revision.wrapping_add(1)
            && !visualization_changed
            && config.visualization_enabled
            && !shutting_down
            && !settings_changed;
        let invalidate =
            signal_changed || track_changed || visualization_changed || shutting_down || stopping;
        let membership_changed = self.panel_revision != panels.revision
            || AudioVisualizerOptions::from(self.config.as_ref())
                != AudioVisualizerOptions::from(config.as_ref());
        let mut panels_changed = false;
        let mut layout_panels_changed = false;
        if membership_changed {
            let active = panels
                .active_panels()
                .filter_map(|panel| {
                    PanelKind::from_panel(panel, &config).map(|kind| (panel.id, kind))
                })
                .collect::<HashMap<_, _>>();
            let all = panels
                .panels()
                .filter_map(|panel| {
                    PanelKind::from_panel(panel, &config).map(|kind| (panel.id, kind))
                })
                .collect::<HashMap<_, _>>();
            panels_changed = self.panels != active;
            layout_panels_changed = self.layout_panels != all;
            self.panels = active;
            self.layout_panels = all;
            self.panel_revision = panels.revision;
            self.views.retain(|id, _| self.panels.contains_key(id));
        }
        let was_running = self.analysis_interval();
        self.config = config;
        self.visible = visible;
        self.shutting_down = shutting_down;
        self.status = status;
        self.source_generation = source_generation;
        self.seek_revision = seek_revision;
        self.current_track_id = current_track_id;
        self.position = position;
        self.duration = duration;
        if status_changed && status == PlaybackStatus::Paused {
            self.style_coordinator.suspend();
            self.animation_clock.suspend();
        }

        if invalidate {
            self.staging = AnalysisFrame::default();
            self.sequence = self.handle.analysis.read().sequence;
            self.needs_refresh = false;
            self.animation_clock = AnimationClock::default();
            self.frequency_views.clear_presentation();
            if fade_on_stop {
                self.style_coordinator
                    .stop(Instant::now(), self.config.radial_fade_when_idle);
            } else {
                self.style_coordinator.clear();
            }
        }
        if status_changed && status != PlaybackStatus::Stopped {
            self.style_coordinator.retire_idle_fade();
        }
        if invalidate || visibility_changed || status_changed {
            // A suspension retires input, not its bounded last presentation.
            // Do not animate old targets while acquiring replacement audio.
            self.awaiting_replacement = status != PlaybackStatus::Stopped;
        }
        let enabled = self.active();
        let demand_changed = panels_changed || visibility_changed || visualization_changed;
        let mut spectrum_restored = false;
        if layout_panels_changed {
            let mut retained_spectrum = Vec::new();
            let mut retained_spectrogram = Vec::new();
            let mut retained_audio = Vec::new();
            for (id, kind) in &self.layout_panels {
                match kind {
                    PanelKind::Spectrum(_) => retained_spectrum.push(*id),
                    PanelKind::Spectrogram(_) => retained_spectrogram.push(*id),
                    PanelKind::Audio(options) => retained_audio.push(*options),
                    _ => {}
                }
            }
            self.frequency_views
                .retain_presentation_panels(&retained_spectrum, &retained_spectrogram);
            self.style_coordinator.retain_presentations(retained_audio);
        }
        if demand_changed {
            self.frequency_views.set_enabled(enabled);
            let spectrum_ids = self
                .panels
                .iter()
                .filter(|(_, kind)| enabled && matches!(kind, PanelKind::Spectrum(_)))
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            let spectrogram_ids = self
                .panels
                .iter()
                .filter(|(_, kind)| enabled && matches!(kind, PanelKind::Spectrogram(_)))
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            self.frequency_views.retain_spectrum_panels(&spectrum_ids);
            self.frequency_views
                .retain_spectrogram_panels(&spectrogram_ids);
            spectrum_restored = self
                .frequency_views
                .set_spectrum_visible(!spectrum_ids.is_empty());
            let mut visualizer = VisualizerDemand::default();
            for kind in self.panels.values().filter(|_| enabled) {
                if let PanelKind::Audio(options) = kind {
                    match options.style {
                        AudioVisualizerStyle::StereoDensity => visualizer.stereo_density = true,
                        AudioVisualizerStyle::SpatialSpectrum => visualizer.spatial_spectrum = true,
                        AudioVisualizerStyle::Oscilloscope => visualizer.pcm = true,
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
            retain_analysis_staging(&mut self.staging, demand);
            self.handle.set_display_demand(demand);
            if self.demand.enabled != demand.enabled {
                self.cancel_clock(cx);
            }
            self.demand = demand;
        }
        if config_changed || demand_changed {
            let audio_changed = self.style_coordinator.sync(
                &self.parameters,
                self.panels.values().filter_map(|kind| match kind {
                    PanelKind::Audio(options) => Some(*options),
                    _ => None,
                }),
                enabled,
            );
            if self.demand.enabled {
                let spectrum_changed = self.frequency_views.configure(&self.parameters);
                self.needs_refresh |= spectrum_restored || spectrum_changed || audio_changed;
            } else {
                self.needs_refresh = false;
            }
        }
        if was_running != self.analysis_interval()
            || visibility_changed
            || visualization_changed
            || fps_changed
        {
            self.cancel_clock(cx);
        }
        let waveform_enabled = self.config.visualization_enabled && !self.shutting_down;
        let waveform_visible = self.active();
        self.waveform.update(cx, |waveform, cx| {
            waveform.retain_panels(
                |id| {
                    waveform_visible && matches!(self.panels.get(&id), Some(PanelKind::Waveform(_)))
                },
                cx,
            );
            // Visibility retires rendering resources, not explicit scan results.
            waveform.set_enabled(waveform_enabled);
            waveform.set_seek_enabled(waveform_visible && self.config.waveform_seek_enabled);
            if waveform_enabled {
                waveform.sync(waveform_track, cx);
            }
        });
        if config_changed || panels_changed || visibility_changed || status_changed || invalidate {
            self.frame_requested = self.demand.enabled;
            self.notify_panels(true, cx);
            self.notify_panels(false, cx);
        } else if position_changed {
            self.notify_panels(false, cx);
        }
    }

    fn active(&self) -> bool {
        self.visible && !self.shutting_down && self.config.visualization_enabled
    }

    fn analysis_interval(&self) -> Option<Duration> {
        (self.active() && self.demand.enabled && self.status == PlaybackStatus::Playing)
            .then(|| Duration::from_secs_f64(1.0 / self.config.analysis_rate_hz as f64))
    }

    fn cancel_clock(&mut self, cx: &mut gpui::App) {
        self.frames.update(cx, |frames, _| {
            frames.cancel_presentation(self.frame_generation)
        });
        self.frame_generation = self.frame_generation.wrapping_add(1);
        self.analysis_timer.take();
        self.frame_pending = false;
        self.animation_clock.suspend();
        self.frame_requested = false;
    }

    fn start_analysis_clock(&mut self, cx: &mut Context<Self>) {
        let Some(interval) = self.analysis_interval() else {
            return;
        };
        if self.analysis_timer.is_some() {
            return;
        }
        self.handle.request_analysis_frame();
        let generation = self.frame_generation;
        let executor = cx.background_executor().clone();
        self.analysis_timer = Some(cx.spawn(async move |this, cx| {
            let mut deadline = Instant::now() + interval;
            loop {
                executor
                    .timer(deadline.saturating_duration_since(Instant::now()))
                    .await;
                let running = this
                    .update(cx, |this, _| {
                        if this.frame_generation != generation || this.analysis_interval().is_none()
                        {
                            return false;
                        }
                        this.handle.request_analysis_frame();
                        true
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
                deadline += interval;
                let now = Instant::now();
                if deadline <= now {
                    // Coalesce missed requests instead of replaying a timer backlog.
                    deadline = now + interval;
                }
            }
        }));
    }

    pub(in crate::gui) fn request_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.active()
            || !self.demand.enabled
            || (!self.frame_requested
                && self.analysis_interval().is_none()
                && !self.style_coordinator.idle_fading(Instant::now()))
            || self.frame_pending
        {
            return;
        }
        self.start_analysis_clock(cx);
        self.frame_pending = true;
        let owner = cx.weak_entity();
        self.frames.update(cx, |frames, cx| {
            frames.request_presentation(
                owner,
                self.frame_generation,
                self.config.visualization_fps,
                window,
                cx,
            );
        });
    }

    pub(in crate::gui) fn animation_frame(
        &mut self,
        generation: u64,
        now: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if generation != self.frame_generation {
            return;
        }
        self.frame_pending = false;
        if !self.active() || !self.demand.enabled {
            return;
        }
        self.frame_requested = false;
        if self.analysis_interval().is_some() {
            self.consume_analysis(now, cx);
            if !self.awaiting_replacement {
                let time = self.animation_clock.advance(now);
                let visuals_changed = self.frequency_views.animate(time);
                let audio_changed = self.style_coordinator.animate(time);
                self.notify_animation(visuals_changed, audio_changed, cx);
            }
        } else {
            self.animation_clock.suspend();
            self.style_coordinator.advance_idle_fade(now);
            self.notify_animation(false, true, cx);
        }
        self.request_frame(window, cx);
    }

    fn consume_analysis(&mut self, now: Instant, cx: &mut Context<Self>) {
        // Suspended playback never consumes audio, including the retired Stop
        // fade. Semantic invalidations are handled by sync.
        if self.status != PlaybackStatus::Playing {
            return;
        }
        let frame = self.handle.analysis.read();
        // Capture restarts may discard an incomplete FFT without breaking the
        // signal. Only the producer's continuity fence can revoke this picture.
        if frame.settings.is_none()
            && (self.awaiting_replacement
                || self.animation_clock.source == Some((frame.generation, frame.sample_rate)))
        {
            return;
        }
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
        self.awaiting_replacement = false;
        self.animation_clock.accept(&self.staging, now);
        let time = self.animation_clock.advance(now);
        self.frequency_views.update(&self.staging, time);
        self.style_coordinator.update(&self.staging, time);
        for (id, view) in &self.views {
            if let Some(kind) = self.panels.get(id)
                && kind.analysis()
            {
                view.update(cx, |_, cx| cx.notify());
            }
        }
    }

    fn notify_animation(&self, visuals: bool, audio: bool, cx: &mut Context<Self>) {
        for (id, view) in &self.views {
            let changed = match self.panels.get(id) {
                Some(PanelKind::Spectrum(_) | PanelKind::Spectrogram(_)) => visuals,
                Some(PanelKind::Audio(options)) => {
                    audio
                        && !matches!(
                            options.style,
                            AudioVisualizerStyle::StereoDensity
                                | AudioVisualizerStyle::SpatialSpectrum
                                | AudioVisualizerStyle::Oscilloscope
                        )
                }
                _ => false,
            };
            if changed {
                view.update(cx, |_, cx| cx.notify());
            }
        }
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

    pub(in crate::gui) fn panel(
        &mut self,
        id: u64,
        cx: &mut Context<Self>,
    ) -> Entity<VisualizerPanel> {
        self.views
            .entry(id)
            .or_insert_with(|| {
                let owner = cx.weak_entity();
                cx.new(|_| VisualizerPanel { owner, id })
            })
            .clone()
    }

    fn render_panel(&mut self, id: u64, cx: &mut Context<Self>) -> AnyElement {
        if !self.active() {
            return div()
                .size_full()
                .bg(rgb(self.config.visualization_background.rgb()))
                .into_any_element();
        }
        let theme = rivu_ui::theme::palette(cx);
        match self.panels.get(&id).copied() {
            Some(PanelKind::Spectrum(options)) => {
                self.frequency_views
                    .spectrum(theme, &self.labels, id, options)
            }
            Some(PanelKind::Spectrogram(options)) => {
                self.frequency_views
                    .spectrogram(theme, &self.labels, id, options)
            }
            Some(PanelKind::Audio(options)) => {
                self.style_coordinator
                    .view(&self.labels, id, self.status, options)
            }
            Some(PanelKind::Waveform(options)) => self.waveform.update(cx, |waveform, cx| {
                waveform.view(
                    theme,
                    &self.labels,
                    rivu_visuals::WaveformViewState {
                        panel_id: id,
                        position: self.position,
                        duration: self.duration,
                        options,
                    },
                    &self.parameters,
                    cx,
                )
            }),
            None => div().size_full().into_any_element(),
        }
    }

    /// Called by the shell's first paint, after every panel's prepaint.
    pub(in crate::gui) fn finish_paint(&self, cx: &mut Context<Self>) {
        if self.frequency_views.finish_prepaint() {
            // A resized panel can alter history shared by cached siblings.
            // Repaint siblings to reflect the shared history extent.
            for (id, view) in &self.views {
                if matches!(self.panels.get(id), Some(PanelKind::Spectrogram(_))) {
                    view.update(cx, |_, cx| cx.notify());
                }
            }
        }
    }

    /// Destroy the window-local presentation; temporary hide goes through sync.
    pub(in crate::gui) fn release_display_resources(&mut self, cx: &mut Context<Self>) {
        self.visible = false;
        self.cancel_clock(cx);
        self.demand = DisplayDemand::default();
        self.handle.set_display_demand(self.demand);
        self.staging = AnalysisFrame::default();
        self.needs_refresh = false;
        self.awaiting_replacement = false;
        self.frequency_views.clear_presentation();
        self.style_coordinator.clear();
        self.frequency_views.set_enabled(false);
        self.frequency_views.retain_spectrogram_panels(&[]);
        self.frequency_views.retain_spectrum_panels(&[]);
        self.frequency_views.set_spectrum_visible(false);
        self.style_coordinator.sync(&self.parameters, [], false);
        self.waveform.update(cx, |waveform, cx| {
            waveform.retain_panels(|_| false, cx);
        });
    }
}

impl Drop for VisualizerRuntime {
    fn drop(&mut self) {
        self.handle.set_display_demand(DisplayDemand::default());
    }
}

pub(in crate::gui) struct VisualizerPanel {
    owner: WeakEntity<VisualizerRuntime>,
    id: u64,
}

impl Render for VisualizerPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.owner
            .update(cx, |owner, cx| owner.render_panel(self.id, cx))
            .unwrap_or_else(|_| div().into_any_element())
    }
}

pub(in crate::gui) fn retain_analysis_staging(frame: &mut AnalysisFrame, demand: DisplayDemand) {
    if !demand.enabled {
        *frame = AnalysisFrame::default();
        return;
    }
    // Retain allocations still demanded by another panel, but release optional
    // snapshots immediately rather than waiting for the next worker publication.
    let visualizer = &mut frame.visualizer;
    if !demand.visualizer.stereo_density {
        visualizer.demand.stereo_density = false;
        visualizer.density = rivu_audio::analysis::DensitySnapshot::default();
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
    if !demand.visualizer.pcm {
        visualizer.demand.pcm = false;
        visualizer.pcm = rivu_audio::analysis::PcmSnapshot::default();
    }
}

pub(in crate::gui) fn stage_analysis_frame(
    staging: &mut AnalysisFrame,
    sequence: &mut u64,
    needs_refresh: bool,
    frame: &AnalysisFrame,
    frame_is_current: bool,
) -> bool {
    if !frame_is_current {
        return false;
    }
    if frame.sequence == *sequence
        && frame.generation == staging.generation
        && frame.settings == staging.settings
        && frame.display_token == staging.display_token
        && frame.audio_epoch == staging.audio_epoch
        && frame.audio_intent == staging.audio_intent
    {
        return needs_refresh && staging.settings.is_some();
    }
    staging.frequencies_hz.clone_from(&frame.frequencies_hz);
    staging.spectrum_db.clone_from(&frame.spectrum_db);
    staging.sequence = frame.sequence;
    staging.display_token = frame.display_token;
    staging.audio_epoch = frame.audio_epoch;
    staging.audio_intent = frame.audio_intent;
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
    use crate::core::CoreRuntime;

    fn demand(layout: &layout::Layout) -> crate::gui::workspace::WorkspacePanels {
        let mut panels = crate::gui::workspace::WorkspacePanels::default();
        panels.reconcile(layout);
        panels
    }

    #[test]
    fn display_clock_moves_between_analysis_frames_and_stops_at_accepted_audio() {
        let start = Instant::now();
        let mut frame = AnalysisFrame {
            generation: 1,
            sample_rate: 48_000,
            sample_time: Duration::from_secs(1),
            settings: Some((&Config::default()).into()),
            ..Default::default()
        };
        let mut clock = AnimationClock::default();
        clock.accept(&frame, start);
        assert_eq!(
            clock.advance(start + Duration::from_millis(190)),
            frame.sample_time
        );
        frame.sample_time += Duration::from_millis(200);
        clock.accept(&frame, start + Duration::from_millis(200));
        for millis in (200..=390).step_by(10) {
            assert_eq!(
                clock.advance(start + Duration::from_millis(millis)),
                Duration::from_millis(1000 + millis - 190)
            );
        }
        assert_eq!(
            clock.advance(start + Duration::from_secs(5)),
            frame.sample_time
        );
        clock.accept(&frame, start + Duration::from_secs(5));
        assert_eq!(
            clock.advance(start + Duration::from_secs(6)),
            frame.sample_time
        );
    }

    #[test]
    fn display_clock_suspension_and_discontinuities_do_not_replay_elapsed_time() {
        let start = Instant::now();
        let mut frame = AnalysisFrame {
            generation: 1,
            sample_rate: 48_000,
            sample_time: Duration::from_secs(1),
            settings: Some((&Config::default()).into()),
            ..Default::default()
        };
        let mut clock = AnimationClock::default();
        clock.accept(&frame, start);
        frame.sample_time += Duration::from_millis(200);
        clock.accept(&frame, start);
        assert_eq!(
            clock.advance(start + Duration::from_millis(10)),
            Duration::from_millis(1010)
        );
        clock.suspend();
        assert_eq!(
            clock.advance(start + Duration::from_secs(5)),
            Duration::from_millis(1010)
        );
        for change in 0..3 {
            match change {
                0 => frame.generation += 1,
                1 => frame.sample_rate = 44_100,
                _ => frame.sample_time = Duration::from_millis(100),
            }
            clock.accept(&frame, start + Duration::from_secs(6));
            assert_eq!(
                clock.advance(start + Duration::from_secs(6)),
                frame.sample_time
            );
        }
        frame.settings = None;
        clock.accept(&frame, start + Duration::from_secs(7));
        assert_eq!(
            clock.advance(start + Duration::from_secs(8)),
            Duration::ZERO
        );
    }

    #[test]
    fn display_clock_large_publication_interval_does_not_imply_discontinuity() {
        let start = Instant::now();
        let mut frame = AnalysisFrame {
            generation: 1,
            sample_rate: 48_000,
            sample_time: Duration::from_secs(1),
            settings: Some((&Config::default()).into()),
            ..Default::default()
        };
        let mut clock = AnimationClock::default();
        clock.accept(&frame, start);
        frame.sample_time = Duration::from_secs(3);
        clock.accept(&frame, start + Duration::from_millis(10));
        assert_eq!(
            clock.advance(start + Duration::from_millis(10)),
            Duration::from_millis(1010)
        );
        clock.suspend();
        clock.accept(&frame, start + Duration::from_secs(10));
        assert_eq!(
            clock.advance(start + Duration::from_secs(10)),
            frame.sample_time
        );
    }

    #[test]
    fn same_sequence_invalidation_retires_staged_visualizer_data() {
        let mut frame = AnalysisFrame {
            sequence: 7,
            generation: 1,
            settings: Some((&Config::default()).into()),
            ..Default::default()
        };
        frame.visualizer.demand.band_energy = true;
        frame.visualizer.band_rms = [0.5; 3];
        let mut staging = AnalysisFrame::default();
        let mut sequence = 0;
        assert!(stage_analysis_frame(
            &mut staging,
            &mut sequence,
            false,
            &frame,
            true
        ));

        // Continuity loss invalidates the worker frame before another FFT is ready.
        frame.generation += 1;
        frame.settings = None;
        frame.visualizer = Default::default();
        assert!(stage_analysis_frame(
            &mut staging,
            &mut sequence,
            true,
            &frame,
            true
        ));
        assert!(staging.settings.is_none());
        assert_eq!(staging.visualizer.band_rms, [0.0; 3]);
        assert!(!stage_analysis_frame(
            &mut staging,
            &mut sequence,
            true,
            &frame,
            true
        ));
    }

    #[gpui::test]
    fn optional_tab_switch_keeps_other_consumers_running(cx: &mut gpui::TestAppContext) {
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
        let runtime = CoreRuntime::start(directory.path(), &config_path).unwrap();
        let config = runtime.handle.config_snapshot();
        let waveform =
            cx.new(|_| visualization::Waveform::new(Arc::clone(&runtime.handle.waveform)));
        let owner = cx.new(|cx| {
            VisualizerRuntime::new(
                runtime.handle.clone(),
                Arc::new(Localizer::new("en-US")),
                waveform,
                cx.new(|_| crate::gui::animation_frames::AnimationFrames::default()),
                cx,
            )
        });
        let panel = |id, kind: &str, style: Option<AudioVisualizerStyle>| layout::Panel {
            id,
            kind: kind.to_owned(),
            show_tab_bar: true,
            audio_visualizer: style.map(|style| AudioVisualizerOptions {
                style,
                reference_lines: false,
                labels: false,
                oscilloscope_overlay_channels: false,
            }),
            album_artwork: None,
        };
        let mut layout = layout::Layout {
            root: Some(layout::Node::Split {
                id: 10,
                axis: layout::Axis::Horizontal,
                boundaries: vec![0.25, 0.5, 0.75],
                children: vec![
                    layout::Node::Tabs {
                        id: 1,
                        panels: vec![panel(
                            2,
                            "audio_visualizer",
                            Some(AudioVisualizerStyle::FractalFlow),
                        )],
                        active: 2,
                    },
                    layout::Node::Tabs {
                        id: 3,
                        panels: vec![panel(
                            4,
                            "audio_visualizer",
                            Some(AudioVisualizerStyle::Spectrum),
                        )],
                        active: 4,
                    },
                    layout::Node::Tabs {
                        id: 5,
                        panels: vec![panel(
                            6,
                            "audio_visualizer",
                            Some(AudioVisualizerStyle::Spectrogram),
                        )],
                        active: 6,
                    },
                    layout::Node::Tabs {
                        id: 7,
                        panels: vec![
                            panel(8, "audio_visualizer", Some(AudioVisualizerStyle::Bars)),
                            panel(
                                9,
                                "audio_visualizer",
                                Some(AudioVisualizerStyle::Oscilloscope),
                            ),
                        ],
                        active: 8,
                    },
                ],
            }),
            next_id: 11,
            locked: false,
        };
        let mut panels = demand(&layout);
        owner.update(cx, |owner, cx| {
            owner.sync(
                VisualizerInputs {
                    config: Arc::clone(&config),
                    panels: &panels,
                    status: PlaybackStatus::Playing,
                    source_generation: 0,
                    seek_revision: 0,
                    visible: true,
                    shutting_down: false,
                    current_track_id: None,
                    waveform_track: None,
                    position: 0.0,
                    duration: None,
                },
                cx,
            );
            let now = Instant::now();
            owner.staging = AnalysisFrame {
                sequence: 7,
                generation: 1,
                sample_rate: 48_000,
                sample_time: Duration::from_secs(1),
                settings: Some(config.as_ref().into()),
                ..Default::default()
            };
            owner.animation_clock.accept(&owner.staging, now);
            owner.awaiting_replacement = false;
            let generation = owner.frame_generation;
            assert!(!owner.demand.visualizer.pcm);
            assert!(layout.activate(9));
            panels.reconcile(&layout);
            owner.sync(
                VisualizerInputs {
                    config: Arc::clone(&config),
                    panels: &panels,
                    status: PlaybackStatus::Playing,
                    source_generation: 0,
                    seek_revision: 0,
                    visible: true,
                    shutting_down: false,
                    current_track_id: None,
                    waveform_track: None,
                    position: 0.0,
                    duration: None,
                },
                cx,
            );
            assert!(owner.demand.visualizer.pcm);
            assert!(owner.demand.visualizer.band_energy);
            assert!(!owner.awaiting_replacement);
            assert_eq!(owner.frame_generation, generation);
            assert_eq!(owner.animation_clock.last_tick, Some(now));
            assert_eq!(owner.staging.sequence, 7);
            assert!(matches!(owner.panels.get(&4), Some(PanelKind::Spectrum(_))));
            assert!(matches!(
                owner.panels.get(&6),
                Some(PanelKind::Spectrogram(_))
            ));
            let view = owner.panel(4, cx);
            let demand = owner.demand;
            // Transport, Seek and source fences all use the same workspace
            // demand, but must still update dynamic state and invalidate input.
            for (position, source_generation, seek_revision) in
                [(1.0, 0, 0), (2.0, 0, 1), (3.0, 1, 1)]
            {
                owner.sync(
                    VisualizerInputs {
                        config: Arc::clone(&config),
                        panels: &panels,
                        status: PlaybackStatus::Playing,
                        source_generation,
                        seek_revision,
                        visible: true,
                        shutting_down: false,
                        current_track_id: None,
                        waveform_track: None,
                        position,
                        duration: Some(120.0),
                    },
                    cx,
                );
                assert_eq!(owner.position, position);
                assert_eq!(owner.demand, demand);
                assert_eq!(owner.panel(4, cx).entity_id(), view.entity_id());
                if seek_revision != 0 {
                    assert!(owner.staging.settings.is_none());
                }
            }
            assert!(layout.set_audio_visualizer_options(
                9,
                AudioVisualizerOptions {
                    style: AudioVisualizerStyle::StereoDensity,
                    reference_lines: false,
                    labels: false,
                    oscilloscope_overlay_channels: false,
                }
            ));
            panels.reconcile(&layout);
            owner.sync(
                VisualizerInputs {
                    config: Arc::clone(&config),
                    panels: &panels,
                    status: PlaybackStatus::Playing,
                    source_generation: 1,
                    seek_revision: 1,
                    visible: true,
                    shutting_down: false,
                    current_track_id: None,
                    waveform_track: None,
                    position: 3.0,
                    duration: Some(120.0),
                },
                cx,
            );
            assert!(owner.demand.visualizer.stereo_density);
            assert!(!owner.demand.visualizer.pcm);
            assert!(owner.demand.visualizer.band_energy);
            assert_eq!(owner.panel(4, cx).entity_id(), view.entity_id());
        });
    }

    #[gpui::test]
    fn pause_preserves_displayed_publication_instead_of_consuming_worker_reset(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let config = Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            ..Config::default()
        };
        config.save(&config_path).unwrap();
        let runtime = CoreRuntime::start(directory.path(), &config_path).unwrap();
        let waveform =
            cx.new(|_| visualization::Waveform::new(Arc::clone(&runtime.handle.waveform)));
        let owner = cx.new(|cx| {
            VisualizerRuntime::new(
                runtime.handle.clone(),
                Arc::new(Localizer::new("en-US")),
                waveform,
                cx.new(|_| crate::gui::animation_frames::AnimationFrames::default()),
                cx,
            )
        });
        owner.update(cx, |owner, cx| {
            // Cached input is the last publication already drawn by the renderers.
            owner.staging = AnalysisFrame {
                sequence: 7,
                generation: 1,
                sample_rate: 48_000,
                sample_time: Duration::from_secs(1),
                settings: Some((&config).into()),
                frequencies_hz: vec![100.0, 1000.0],
                spectrum_db: vec![-12.0, -24.0],
                ..Default::default()
            };
            owner.staging.visualizer.demand.pcm = true;
            owner.staging.visualizer.pcm = vec![[0.25, -0.5]; 512].into();
            owner.staging.visualizer.band_rms = [0.25, 0.5, 0.75];
            owner.sequence = owner.staging.sequence;
            owner.status = PlaybackStatus::Playing;
            let layout = layout::Layout {
                root: None,
                next_id: 1,
                locked: false,
            };
            let panels = demand(&layout);
            owner.sync(
                VisualizerInputs {
                    config: Arc::clone(&owner.config),
                    panels: &panels,
                    status: PlaybackStatus::Paused,
                    source_generation: owner.source_generation,
                    seek_revision: owner.seek_revision,
                    visible: owner.visible,
                    shutting_down: false,
                    current_track_id: None,
                    waveform_track: None,
                    position: owner.position,
                    duration: owner.duration,
                },
                cx,
            );
            owner.needs_refresh = true;
            owner.consume_analysis(Instant::now(), cx);
            assert_eq!(owner.sequence, 7);
            assert_eq!(owner.staging.spectrum_db, [-12.0, -24.0]);
            assert_eq!(owner.staging.visualizer.pcm[0], [0.25, -0.5]);
            assert_eq!(owner.staging.visualizer.band_rms, [0.25, 0.5, 0.75]);
            assert_eq!(owner.staging.sample_time, Duration::from_secs(1));
            assert_eq!(owner.staging.settings, Some((&config).into()));

            // Resume can observe the worker's pause-reset before fresh FFT data.
            // Keep the accepted picture and do not restart animation from it.
            owner.status = PlaybackStatus::Playing;
            owner.awaiting_replacement = true;
            owner.consume_analysis(Instant::now(), cx);
            assert!(owner.awaiting_replacement);
            assert_eq!(owner.staging.spectrum_db, [-12.0, -24.0]);
            assert_eq!(owner.sequence, 7);

            // Stop does not consume even a still-current worker publication;
            // sync owns retirement and discards staged input at the boundary.
            owner.awaiting_replacement = false;
            owner.status = PlaybackStatus::Stopped;
            owner.consume_analysis(Instant::now(), cx);
            assert_eq!(owner.staging.spectrum_db, [-12.0, -24.0]);
            assert_eq!(owner.sequence, 7);
        });
    }

    #[gpui::test]
    fn hidden_and_paused_semantic_changes_discard_retained_input(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let config = Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            ..Config::default()
        };
        config.save(&config_path).unwrap();
        let runtime = CoreRuntime::start(directory.path(), &config_path).unwrap();
        let config = Arc::new(config);
        let mut layout = layout::Layout {
            root: None,
            next_id: 1,
            locked: false,
        };
        let panel_id = layout.add("audio_visualizer", None);
        layout.set_audio_visualizer_options(
            panel_id,
            AudioVisualizerOptions {
                style: AudioVisualizerStyle::Spectrum,
                reference_lines: false,
                labels: false,
                oscilloscope_overlay_channels: false,
            },
        );
        let panels = demand(&layout);
        let waveform =
            cx.new(|_| visualization::Waveform::new(Arc::clone(&runtime.handle.waveform)));
        let owner = cx.new(|cx| {
            VisualizerRuntime::new(
                runtime.handle.clone(),
                Arc::new(Localizer::new("en-US")),
                waveform,
                cx.new(|_| crate::gui::animation_frames::AnimationFrames::default()),
                cx,
            )
        });
        owner.update(cx, |owner, cx| {
            owner.status = PlaybackStatus::Paused;
            for transition in 0..4 {
                owner.staging.settings = Some(config.as_ref().into());
                owner.staging.spectrum_db = vec![-12.0];
                let mut changed = config.as_ref().clone();
                if transition == 2 {
                    changed.analysis_fft_size = 4096;
                }
                if transition == 3 {
                    changed.visualization_enabled = false;
                }
                owner.sync(
                    VisualizerInputs {
                        config: Arc::new(changed),
                        panels: &panels,
                        status: PlaybackStatus::Paused,
                        source_generation: u64::from(transition >= 1),
                        seek_revision: 1,
                        visible: false,
                        shutting_down: false,
                        current_track_id: None,
                        waveform_track: None,
                        position: 25.0,
                        duration: Some(100.0),
                    },
                    cx,
                );
                assert!(owner.staging.settings.is_none());
                assert!(owner.staging.spectrum_db.is_empty());
                assert!(owner.analysis_timer.is_none());
                assert_eq!(owner.demand, DisplayDemand::default());
            }
        });
        let state = |seek_revision| VisualizerInputs {
            config: Arc::clone(&config),
            panels: &panels,
            status: PlaybackStatus::Paused,
            source_generation: 1,
            seek_revision,
            visible: false,
            shutting_down: false,
            current_track_id: None,
            waveform_track: None,
            position: 25.0,
            duration: Some(100.0),
        };
        let panel = owner.update(cx, |owner, cx| {
            owner.sync(state(1), cx);
            owner.panel(panel_id, cx)
        });
        cx.run_until_parked();
        let notifications = std::rc::Rc::new(std::cell::Cell::new(0));
        let observed = notifications.clone();
        let _subscription = cx.update(|cx| {
            cx.observe(&panel, move |_, _| {
                observed.set(observed.get() + 1);
            })
        });
        // No config/visibility/status/position change can mask this redraw.
        owner.update(cx, |owner, cx| owner.sync(state(2), cx));
        cx.run_until_parked();
        assert_eq!(
            notifications.get(),
            1,
            "semantic clear must invalidate cached panel scenes"
        );
    }

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
        let runtime = CoreRuntime::start(directory.path(), &config_path).unwrap();
        let config = runtime.handle.config_snapshot();
        let window = cx.add_empty_window();
        let owner = window.update(|_, cx| {
            let waveform =
                cx.new(|_| visualization::Waveform::new(Arc::clone(&runtime.handle.waveform)));
            cx.new(|cx| {
                VisualizerRuntime::new(
                    runtime.handle.clone(),
                    Arc::new(Localizer::new("en-US")),
                    waveform,
                    cx.new(|_| crate::gui::animation_frames::AnimationFrames::default()),
                    cx,
                )
            })
        });
        let empty_layout = layout::Layout {
            root: None,
            next_id: 1,
            locked: false,
        };
        let mut layout = empty_layout.clone();
        let panel_id = layout.add("audio_visualizer", None);
        layout.set_audio_visualizer_options(
            panel_id,
            AudioVisualizerOptions {
                style: AudioVisualizerStyle::Spectrogram,
                reference_lines: false,
                labels: false,
                oscilloscope_overlay_channels: false,
            },
        );
        let waveform_id = layout.add("audio_visualizer", None);
        layout.set_audio_visualizer_options(
            waveform_id,
            AudioVisualizerOptions {
                style: AudioVisualizerStyle::Waveform,
                reference_lines: false,
                labels: false,
                oscilloscope_overlay_channels: false,
            },
        );
        assert!(layout.activate(panel_id));
        let mut panels = demand(&layout);
        window.update(|window, cx| {
            owner.update(cx, |owner, cx| {
                owner.sync(
                    VisualizerInputs {
                        config: Arc::clone(&config),
                        panels: &panels,
                        status: PlaybackStatus::Paused,
                        visible: true,
                        shutting_down: false,
                        current_track_id: None,
                        waveform_track: None,
                        position: 0.0,
                        duration: None,
                        source_generation: 0,
                        seek_revision: 0,
                    },
                    cx,
                );
                assert_eq!(owner.panels.len(), 1);
                assert!(matches!(
                    owner.panels.get(&panel_id),
                    Some(PanelKind::Spectrogram(_))
                ));
                let view = owner.panel(panel_id, cx);
                assert_eq!(view.entity_id(), owner.panel(panel_id, cx).entity_id());
                owner.request_frame(window, cx);
                assert!(owner.frame_pending);
                let generation = owner.frame_generation;
                owner.request_frame(window, cx);
                assert_eq!(owner.frame_generation, generation);
                assert!(owner.analysis_timer.is_none());
                owner.sync(
                    VisualizerInputs {
                        config: Arc::clone(&config),
                        panels: &panels,
                        status: PlaybackStatus::Paused,
                        visible: false,
                        shutting_down: false,
                        current_track_id: None,
                        waveform_track: None,
                        position: 0.0,
                        duration: None,
                        source_generation: 0,
                        seek_revision: 0,
                    },
                    cx,
                );
                assert_eq!(owner.demand, DisplayDemand::default());
                assert!(owner.analysis_timer.is_none());
                assert!(
                    !owner.frame_pending,
                    "cancellation removes only this client's slot"
                );
                assert_ne!(owner.frame_generation, generation);
            });
            window.simulate_next_frame(cx);
            owner.update(cx, |owner, cx| {
                assert!(!owner.frame_pending);
                owner.sync(
                    VisualizerInputs {
                        config: Arc::clone(&config),
                        panels: &panels,
                        status: PlaybackStatus::Paused,
                        visible: true,
                        shutting_down: false,
                        current_track_id: None,
                        waveform_track: None,
                        position: 0.0,
                        duration: None,
                        source_generation: 0,
                        seek_revision: 0,
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
                    VisualizerInputs {
                        config: Arc::clone(&config),
                        panels: &panels,
                        status: PlaybackStatus::Playing,
                        visible: true,
                        shutting_down: false,
                        current_track_id: None,
                        waveform_track: None,
                        position: 0.0,
                        duration: None,
                        source_generation: 0,
                        seek_revision: 0,
                    },
                    cx,
                );
                owner.request_frame(window, cx);
            });
            window.simulate_next_frame(cx);
            owner.update(cx, |owner, cx| {
                assert!(owner.analysis_timer.is_some());
                assert!(
                    owner.frame_pending,
                    "native animation continues while analysis waits"
                );
                let mut capped = config.as_ref().clone();
                capped.visualization_fps = 1;
                owner.sync(
                    VisualizerInputs {
                        config: Arc::new(capped),
                        panels: &panels,
                        status: PlaybackStatus::Playing,
                        visible: true,
                        shutting_down: false,
                        current_track_id: None,
                        waveform_track: None,
                        position: 0.0,
                        duration: None,
                        source_generation: 0,
                        seek_revision: 0,
                    },
                    cx,
                );
                owner.request_frame(window, cx);
            });
            // The existing native gate accepts the replacement configuration,
            // then arms one shared deadline instead of another native callback.
            window.simulate_next_frame(cx);
            owner.update(cx, |owner, cx| {
                assert!(owner.frame_pending);
                assert!(owner.analysis_timer.is_some());
                assert_eq!(
                    owner.analysis_interval(),
                    Some(Duration::from_secs_f64(
                        1.0 / config.analysis_rate_hz as f64
                    ))
                );
                // Repeated wakeups must coalesce behind the one delayed request.
                owner.request_frame(window, cx);
                assert!(owner.frame_pending);
                let mut disabled = config.as_ref().clone();
                disabled.visualization_enabled = false;
                owner.sync(
                    VisualizerInputs {
                        config: Arc::new(disabled),
                        panels: &panels,
                        status: PlaybackStatus::Playing,
                        visible: true,
                        shutting_down: false,
                        current_track_id: None,
                        waveform_track: None,
                        position: 0.0,
                        duration: None,
                        source_generation: 0,
                        seek_revision: 0,
                    },
                    cx,
                );
                assert_eq!(owner.demand, DisplayDemand::default());
                assert!(owner.analysis_timer.is_none());
                assert!(!owner.frame_pending);
                assert!(owner.analysis_interval().is_none());
                assert_eq!(owner.staging.settings, None);
                owner.sync(
                    VisualizerInputs {
                        config: Arc::clone(&config),
                        panels: &panels,
                        status: PlaybackStatus::Playing,
                        visible: true,
                        shutting_down: false,
                        current_track_id: None,
                        waveform_track: None,
                        position: 0.0,
                        duration: None,
                        source_generation: 0,
                        seek_revision: 0,
                    },
                    cx,
                );
                assert!(owner.demand.enabled);
                owner.sync(
                    VisualizerInputs {
                        config: Arc::clone(&config),
                        panels: &panels,
                        status: PlaybackStatus::Playing,
                        visible: true,
                        shutting_down: true,
                        current_track_id: None,
                        waveform_track: None,
                        position: 0.0,
                        duration: None,
                        source_generation: 0,
                        seek_revision: 0,
                    },
                    cx,
                );
                assert_eq!(owner.demand, DisplayDemand::default());
                assert!(owner.analysis_timer.is_none());
                panels.reconcile(&empty_layout);
                owner.sync(
                    VisualizerInputs {
                        config: Arc::clone(&config),
                        panels: &panels,
                        status: PlaybackStatus::Stopped,
                        visible: true,
                        shutting_down: false,
                        current_track_id: None,
                        waveform_track: None,
                        position: 0.0,
                        duration: None,
                        source_generation: 0,
                        seek_revision: 0,
                    },
                    cx,
                );
                assert!(owner.views.is_empty());
                assert!(layout.activate(waveform_id));
                panels.reconcile(&layout);
                owner.sync(
                    VisualizerInputs {
                        config: Arc::clone(&config),
                        panels: &panels,
                        status: PlaybackStatus::Paused,
                        visible: true,
                        shutting_down: false,
                        current_track_id: None,
                        waveform_track: None,
                        position: 12.0,
                        duration: Some(120.0),
                        source_generation: 0,
                        seek_revision: 0,
                    },
                    cx,
                );
                assert_eq!(owner.panels.len(), 1);
                assert!(matches!(
                    owner.panels.get(&waveform_id),
                    Some(PanelKind::Waveform(_))
                ));
                assert!(!owner.demand.enabled);
                owner.sync(
                    VisualizerInputs {
                        config: Arc::clone(&config),
                        panels: &panels,
                        status: PlaybackStatus::Paused,
                        visible: true,
                        shutting_down: false,
                        current_track_id: None,
                        waveform_track: None,
                        position: 13.0,
                        duration: Some(120.0),
                        source_generation: 0,
                        seek_revision: 0,
                    },
                    cx,
                );
                assert_eq!(owner.position, 13.0);
                assert_eq!(owner.duration, Some(120.0));
                // Removing an inactive tab changes retained ownership even
                // though config, active demand, status and position are stable.
                assert!(owner.layout_panels.contains_key(&panel_id));
                assert!(layout.remove(panel_id));
                panels.reconcile(&layout);
                let generation = owner.frame_generation;
                owner.sync(
                    VisualizerInputs {
                        config: Arc::clone(&config),
                        panels: &panels,
                        status: PlaybackStatus::Paused,
                        visible: true,
                        shutting_down: false,
                        current_track_id: None,
                        waveform_track: None,
                        position: 13.0,
                        duration: Some(120.0),
                        source_generation: 0,
                        seek_revision: 0,
                    },
                    cx,
                );
                assert!(!owner.layout_panels.contains_key(&panel_id));
                assert_eq!(owner.layout_panels.len(), 1);
                assert_eq!(owner.panels.len(), 1);
                assert_eq!(owner.frame_generation, generation);
                owner.release_display_resources(cx);
                assert_eq!(owner.demand, DisplayDemand::default());
            });
        });
    }

    #[gpui::test]
    fn core_stop_fences_audio_but_keeps_a_bounded_fade_until_cleanup(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            radial_fade_when_idle: true,
            ..Config::default()
        }
        .save(&config_path)
        .unwrap();
        let runtime = CoreRuntime::start(directory.path(), &config_path).unwrap();
        let config = runtime.handle.config_snapshot();
        let mut layout = layout::Layout {
            root: None,
            next_id: 1,
            locked: false,
        };
        layout.add("audio_visualizer", None);
        let panels = demand(&layout);
        let waveform =
            cx.new(|_| visualization::Waveform::new(Arc::clone(&runtime.handle.waveform)));
        let owner = cx.new(|cx| {
            VisualizerRuntime::new(
                runtime.handle.clone(),
                Arc::new(Localizer::new("en-US")),
                waveform,
                cx.new(|_| crate::gui::animation_frames::AnimationFrames::default()),
                cx,
            )
        });
        let before = runtime.handle.gui_snapshot();
        let track = crate::artwork::tests::track(directory.path().join("track.wav"));
        let state = |status, source_generation, seek_revision| VisualizerInputs {
            config: Arc::clone(&config),
            panels: &panels,
            status,
            source_generation,
            seek_revision,
            visible: true,
            shutting_down: false,
            current_track_id: Some(track.id),
            waveform_track: Some(&track),
            position: 0.0,
            duration: None,
        };
        owner.update(cx, |owner, cx| {
            owner.sync(
                state(
                    PlaybackStatus::Playing,
                    before.source_generation,
                    before.playback.seek_revision,
                ),
                cx,
            );
            let frame = AnalysisFrame {
                generation: before.source_generation,
                sample_rate: 48_000,
                sample_time: Duration::from_secs(1),
                settings: Some(config.as_ref().into()),
                frequencies_hz: vec![20.0, 200.0, 2000.0, 20_000.0],
                spectrum_db: vec![-12.0; 4],
                ..Default::default()
            };
            owner.style_coordinator.update(&frame, frame.sample_time);
            owner.staging = frame;
        });
        // Execute the real Core Stop command: it advances both source and seek
        // identities, unlike a synthetic status-only presentation transition.
        let ack = futures::executor::block_on(
            runtime
                .handle
                .request_ack(rivu_protocol::model::Command::Stop),
        );
        assert!(ack.is_success(), "{:?}", ack.error);
        let stopped = runtime.handle.gui_snapshot();
        assert_ne!(stopped.source_generation, before.source_generation);
        assert_ne!(
            stopped.playback.seek_revision,
            before.playback.seek_revision
        );
        owner.update(cx, |owner, cx| {
            owner.sync(
                state(
                    stopped.playback.status,
                    stopped.source_generation,
                    stopped.playback.seek_revision,
                ),
                cx,
            );
            assert!(owner.style_coordinator.idle_fading(Instant::now()));
            assert!(owner.staging.settings.is_none());
            assert!(owner.staging.spectrum_db.is_empty());
            assert!(owner.analysis_interval().is_none());
            owner.consume_analysis(Instant::now(), cx);
            assert!(owner.style_coordinator.idle_fading(Instant::now()));
            assert!(owner.staging.settings.is_none());
            owner
                .style_coordinator
                .advance_idle_fade(Instant::now() + Duration::from_secs(3));
            assert!(!owner.style_coordinator.idle_fading(Instant::now()));
            owner.frame_requested = false;
        });
        let window = cx.add_empty_window();
        window.update(|window, cx| {
            owner.update(cx, |owner, cx| {
                owner.request_frame(window, cx);
                assert!(!owner.frame_pending);
                assert!(owner.analysis_timer.is_none());
            });
        });
    }

    #[gpui::test]
    fn stopped_fade_does_not_cross_resume_source_seek_still_or_shutdown(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            radial_fade_when_idle: true,
            ..Config::default()
        }
        .save(&config_path)
        .unwrap();
        let runtime = CoreRuntime::start(directory.path(), &config_path).unwrap();
        let config = runtime.handle.config_snapshot();
        let mut layout = layout::Layout {
            root: None,
            next_id: 1,
            locked: false,
        };
        layout.add("audio_visualizer", None);
        let panels = demand(&layout);
        let track = crate::artwork::tests::track(directory.path().join("track.wav"));
        let mut replacement = track.clone();
        replacement.id += 1;
        for boundary in 0..7 {
            let waveform =
                cx.new(|_| visualization::Waveform::new(Arc::clone(&runtime.handle.waveform)));
            let owner = cx.new(|cx| {
                VisualizerRuntime::new(
                    runtime.handle.clone(),
                    Arc::new(Localizer::new("en-US")),
                    waveform,
                    cx.new(|_| crate::gui::animation_frames::AnimationFrames::default()),
                    cx,
                )
            });
            let mut state = VisualizerInputs {
                config: Arc::clone(&config),
                panels: &panels,
                status: PlaybackStatus::Playing,
                source_generation: 1,
                seek_revision: 1,
                visible: true,
                shutting_down: false,
                current_track_id: Some(track.id),
                waveform_track: Some(&track),
                position: 1.0,
                duration: None,
            };
            owner.update(cx, |owner, cx| {
                owner.sync(
                    VisualizerInputs {
                        config: Arc::clone(&state.config),
                        ..state
                    },
                    cx,
                );
                let frame = AnalysisFrame {
                    generation: 1,
                    sample_rate: 48_000,
                    sample_time: Duration::from_secs(1),
                    settings: Some(config.as_ref().into()),
                    frequencies_hz: vec![20.0, 200.0, 2000.0, 20_000.0],
                    spectrum_db: vec![-12.0; 4],
                    ..Default::default()
                };
                owner.style_coordinator.update(&frame, frame.sample_time);
                state.status = PlaybackStatus::Stopped;
                state.source_generation += 1;
                state.seek_revision += 1;
                if boundary >= 5 {
                    state.waveform_track = if boundary == 5 {
                        None
                    } else {
                        Some(&replacement)
                    };
                    state.current_track_id = state.waveform_track.map(|track| track.id);
                }
                owner.sync(
                    VisualizerInputs {
                        config: Arc::clone(&state.config),
                        ..state
                    },
                    cx,
                );
                if boundary >= 5 {
                    assert!(!owner.style_coordinator.idle_fading(Instant::now()));
                    return;
                }
                assert!(owner.style_coordinator.idle_fading(Instant::now()));
                match boundary {
                    0 => state.status = PlaybackStatus::Playing,
                    1 => state.source_generation += 1,
                    2 => state.seek_revision += 1,
                    3 => {
                        state.config = Arc::new(Config {
                            visualization_enabled: false,
                            ..config.as_ref().clone()
                        });
                    }
                    _ => state.shutting_down = true,
                }
                owner.sync(state, cx);
                assert!(!owner.style_coordinator.idle_fading(Instant::now()));
                assert!(owner.staging.settings.is_none());
                assert!(owner.staging.spectrum_db.is_empty());
            });
        }
    }

    #[gpui::test]
    fn stop_fade_requires_a_single_fence_and_retained_actual_current(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            radial_fade_when_idle: true,
            ..Config::default()
        }
        .save(&config_path)
        .unwrap();
        let runtime = CoreRuntime::start(directory.path(), &config_path).unwrap();
        let config = runtime.handle.config_snapshot();
        let mut layout = layout::Layout {
            root: None,
            next_id: 1,
            locked: false,
        };
        layout.add("audio_visualizer", None);
        let panels = demand(&layout);
        let track = crate::artwork::tests::track(directory.path().join("track.wav"));
        let current = Some(track.id);
        let wrap = u64::MAX;
        for (case, before_current, after_current, (generation, revision), steps, fades) in [
            ("ordinary Stop", current, current, (10, 20), (1, 1), true),
            (
                "wrapping Stop",
                current,
                current,
                (wrap, wrap),
                (1, 1),
                true,
            ),
            (
                "coalesced Seek Stop",
                current,
                current,
                (10, 20),
                (1, 2),
                false,
            ),
            (
                "coalesced source Stop",
                current,
                current,
                (10, 20),
                (2, 1),
                false,
            ),
            (
                "unconfirmed Stop",
                current,
                current,
                (10, 20),
                (0, 0),
                false,
            ),
            (
                "ClearQueue, selected metadata",
                current,
                None,
                (10, 20),
                (1, 1),
                false,
            ),
            (
                "metadata without current",
                None,
                None,
                (10, 20),
                (1, 1),
                false,
            ),
        ] {
            let waveform =
                cx.new(|_| visualization::Waveform::new(Arc::clone(&runtime.handle.waveform)));
            let owner = cx.new(|cx| {
                VisualizerRuntime::new(
                    runtime.handle.clone(),
                    Arc::new(Localizer::new("en-US")),
                    waveform,
                    cx.new(|_| crate::gui::animation_frames::AnimationFrames::default()),
                    cx,
                )
            });
            owner.update(cx, |owner, cx| {
                let state = |status, current_track_id, source_generation, seek_revision| {
                    VisualizerInputs {
                        config: Arc::clone(&config),
                        panels: &panels,
                        status,
                        source_generation,
                        seek_revision,
                        visible: true,
                        shutting_down: false,
                        current_track_id,
                        // Explicit metadata selection stays the waveform fallback
                        // after ClearQueue, but cannot become a playback owner.
                        waveform_track: Some(&track),
                        position: 0.0,
                        duration: track.duration,
                    }
                };
                owner.sync(
                    state(
                        PlaybackStatus::Playing,
                        before_current,
                        generation,
                        revision,
                    ),
                    cx,
                );
                assert_eq!(owner.current_track_id, before_current, "{case}");
                let frame = AnalysisFrame {
                    generation,
                    sample_rate: 48_000,
                    sample_time: Duration::from_secs(1),
                    settings: Some(config.as_ref().into()),
                    frequencies_hz: vec![20.0, 200.0, 2000.0, 20_000.0],
                    spectrum_db: vec![-12.0; 4],
                    ..Default::default()
                };
                owner.style_coordinator.update(&frame, frame.sample_time);
                owner.staging = frame;
                owner.sync(
                    state(
                        PlaybackStatus::Stopped,
                        after_current,
                        generation.wrapping_add(steps.0),
                        revision.wrapping_add(steps.1),
                    ),
                    cx,
                );
                assert_eq!(owner.current_track_id, after_current, "{case}");
                assert_eq!(
                    owner.style_coordinator.idle_fading(Instant::now()),
                    fades,
                    "{case}"
                );
                assert!(owner.staging.settings.is_none(), "{case}");
                assert!(owner.staging.spectrum_db.is_empty(), "{case}");
            });
        }
    }
}
