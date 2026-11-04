use rivu_metadata::artwork::{Artwork as LoadedArtwork, ArtworkIdentity, ArtworkManager};
use std::{collections::HashMap, sync::Arc, time::Instant};

use gpui::{
    App, Bounds, BoxShadow, Context, Entity, ObjectFit, Pixels, Render, Transformation, Window,
    canvas, div, point, prelude::*, px, radians, rgb, size,
};

use rivu_format::config::ArtworkDirection;
use rivu_format::config::ArtworkMode;
use rivu_format::config::ArtworkOptions;
use rivu_format::config::ArtworkStyle;
use rivu_format::config::Config;
use rivu_presentation::i18n::Localizer;
use rivu_protocol::model::{PlaybackStatus, Track};

use super::animation_frames::AnimationFrames;
use super::artwork::{
    ArtworkDecoration, ArtworkImageRegistry, ArtworkLoader, ArtworkView, DefaultArtwork,
};

pub(super) const ARTWORK_STYLES: [ArtworkStyle; 7] = [
    ArtworkStyle::Plain,
    ArtworkStyle::Card,
    ArtworkStyle::Vinyl,
    ArtworkStyle::Glass,
    ArtworkStyle::Metal,
    ArtworkStyle::Halo,
    ArtworkStyle::Duotone,
];

pub(super) fn artwork_style_id(style: ArtworkStyle) -> &'static str {
    match style {
        ArtworkStyle::Plain => "settings-plain",
        ArtworkStyle::Vinyl => "settings-vinyl",
        ArtworkStyle::Glass => "settings-glass",
        ArtworkStyle::Halo => "settings-halo",
        ArtworkStyle::Metal => "settings-metal",
        ArtworkStyle::Card => "settings-card",
        ArtworkStyle::Duotone => "settings-duotone",
    }
}

pub(super) fn artwork_style_label(i18n: &Localizer, style: ArtworkStyle) -> &str {
    i18n.text(artwork_style_id(style))
}

#[derive(Clone, Copy, PartialEq)]
struct RotationSettings {
    mode: ArtworkMode,
    period_seconds: f32,
    direction: ArtworkDirection,
}

impl RotationSettings {
    fn from_options(options: ArtworkOptions) -> Self {
        Self {
            mode: options.mode,
            period_seconds: options.period_seconds,
            direction: options.direction,
        }
    }
}

/// Time is accumulated only while the panel is visible and playing. Removing
/// the baseline at every suspension prevents hidden time from being caught up.
struct Rotation {
    angle: f64,
    settings: RotationSettings,
    running: bool,
    last_tick: Option<Instant>,
}

impl Rotation {
    fn new() -> Self {
        Self {
            angle: 0.,
            settings: RotationSettings {
                mode: ArtworkMode::Static,
                period_seconds: 20.,
                direction: ArtworkDirection::Clockwise,
            },
            running: false,
            last_tick: None,
        }
    }

    fn advance(&mut self, now: Instant) {
        if !self.running {
            return;
        }
        if let Some(last) = self.last_tick.replace(now) {
            let direction = match self.settings.direction {
                ArtworkDirection::Clockwise => 1.,
                ArtworkDirection::Counterclockwise => -1.,
            };
            self.angle = (self.angle
                + direction * std::f64::consts::TAU * now.duration_since(last).as_secs_f64()
                    / f64::from(self.settings.period_seconds))
            .rem_euclid(std::f64::consts::TAU);
        }
    }

    fn sync(
        &mut self,
        now: Instant,
        settings: RotationSettings,
        eligible: bool,
        reset: bool,
    ) -> bool {
        let running = eligible && settings.mode == ArtworkMode::Rotating;
        let changed = reset || self.settings != settings || self.running != running;
        if changed {
            self.advance(now);
        }
        if reset || settings.mode == ArtworkMode::Static {
            self.angle = 0.;
        }
        self.settings = settings;
        self.running = running;
        if changed || !running {
            self.last_tick = None;
        }
        changed
    }
}

struct Instance {
    options: ArtworkOptions,
    rotation: Rotation,
    visible: bool,
    panel: Option<gpui::WeakEntity<ArtworkPanel>>,
}

const DECORATIONS: [&[u8]; 6] = [
    include_bytes!("../../data/artwork/vinyl.svg"),
    include_bytes!("../../data/artwork/glass.svg"),
    include_bytes!("../../data/artwork/halo.svg"),
    include_bytes!("../../data/artwork/metal.svg"),
    include_bytes!("../../data/artwork/card.svg"),
    include_bytes!("../../data/artwork/duotone.svg"),
];

fn decoration_index(style: ArtworkStyle) -> Option<usize> {
    match style {
        ArtworkStyle::Plain => None,
        ArtworkStyle::Vinyl => Some(0),
        ArtworkStyle::Glass => Some(1),
        ArtworkStyle::Halo => Some(2),
        ArtworkStyle::Metal => Some(3),
        ArtworkStyle::Card => Some(4),
        ArtworkStyle::Duotone => Some(5),
    }
}

/// Instances share one source owner, frame callback and cancellable deadline.
/// Rotation and source-animation cursors remain local to individual panels.
pub(super) struct ArtworkRuntime {
    manager: Arc<ArtworkManager>,
    loader: ArtworkLoader,
    view: ArtworkView,
    default_artwork: Entity<DefaultArtwork>,
    track: Option<Track>,
    status: PlaybackStatus,
    visible: bool,
    instances: HashMap<u64, Instance>,
    window_visible: bool,
    panel_revision: u64,
    images: ArtworkImageRegistry,
    decorations: [Option<ArtworkDecoration>; 6],
    fps: u16,
    frames: Entity<AnimationFrames>,
    frame_pending: bool,
    frame_generation: u64,
    activation_subscription: Option<gpui::Subscription>,
}

impl ArtworkRuntime {
    pub(super) fn new(
        manager: Arc<ArtworkManager>,
        default_artwork: Entity<DefaultArtwork>,
        images: ArtworkImageRegistry,
        frames: Entity<AnimationFrames>,
    ) -> Self {
        Self {
            manager,
            loader: ArtworkLoader::default(),
            view: ArtworkView::new(images.clone()),
            default_artwork,
            track: None,
            status: PlaybackStatus::Stopped,
            visible: false,
            window_visible: false,
            instances: HashMap::new(),
            panel_revision: 0,
            images,
            decorations: std::array::from_fn(|_| None),
            fps: 0,
            frames,
            frame_pending: false,
            frame_generation: 0,
            activation_subscription: None,
        }
    }

    pub(super) fn sync(
        &mut self,
        track: Option<&Track>,
        status: PlaybackStatus,
        config: &Config,
        panels: &super::workspace::WorkspacePanels,
        window_visible: bool,
        cx: &mut Context<Self>,
    ) {
        let switched = match (&self.track, track) {
            (Some(previous), Some(next)) => {
                previous.id != next.id || previous.fingerprint != next.fingerprint
            }
            (None, None) => false,
            _ => true,
        };
        let source_changed = switched
            || matches!((&self.track, track), (Some(previous), Some(next)) if previous.path != next.path);
        let stopped = status == PlaybackStatus::Stopped && self.status != status;
        let mut changed = source_changed || self.status != status;
        let demand_changed =
            self.panel_revision != panels.revision || self.window_visible != window_visible;
        if self.panel_revision != panels.revision {
            self.instances.retain(|id, _| {
                panels
                    .panel(*id)
                    .is_some_and(|panel| panel.artwork.is_some())
            });
            for panel in panels.panels() {
                let Some(options) = panel.artwork else {
                    continue;
                };
                let instance = self.instances.entry(panel.id).or_insert_with(|| {
                    changed = true;
                    Instance {
                        options,
                        rotation: Rotation::new(),
                        visible: false,
                        panel: None,
                    }
                });
                changed |= instance.options != options;
                instance.options = options;
            }
            self.panel_revision = panels.revision;
        }
        let now = Instant::now();
        for (id, instance) in &mut self.instances {
            if demand_changed {
                let visible = window_visible && panels.is_panel_active(*id);
                changed |= instance.visible != visible;
                instance.visible = visible;
            }
            changed |= instance.rotation.sync(
                now,
                RotationSettings::from_options(instance.options),
                status == PlaybackStatus::Playing && instance.visible,
                switched || stopped,
            );
        }
        let mut visible = self.visible;
        if demand_changed {
            let visible_consumers: Vec<_> = self
                .instances
                .iter()
                .filter_map(|(id, instance)| instance.visible.then_some(*id))
                .collect();
            visible = !visible_consumers.is_empty();
            self.view.retain_consumers(&visible_consumers);
            let mut visible_styles = [false; 6];
            for instance in self.instances.values().filter(|instance| instance.visible) {
                if let Some(index) = decoration_index(instance.options.style) {
                    visible_styles[index] = true;
                }
            }
            for (index, decoration) in self.decorations.iter_mut().enumerate() {
                if visible_styles[index] {
                    if decoration.is_none() {
                        *decoration = Some(self.images.decoration(index, DECORATIONS[index], cx));
                        changed = true;
                    }
                } else if let Some(previous) = decoration.take() {
                    previous.retire();
                    changed = true;
                }
            }
        }
        changed |= self.visible != visible || self.fps != config.artwork_fps;
        if source_changed {
            self.track = track.cloned();
        }
        if source_changed || self.visible && !visible {
            // Source changes retire old pixels without resetting track rotation.
            // Hiding the last panel also releases its display owner.
            self.view.replace(None, cx);
        }
        self.status = status;
        self.visible = visible;
        self.window_visible = window_visible;
        if changed {
            self.cancel_frame(cx);
        }
        self.fps = config.artwork_fps;
        self.prepare_artwork(cx);
        if changed {
            cx.notify();
        }
    }

    fn cancel_frame(&mut self, cx: &mut App) {
        self.frames
            .update(cx, |frames, _| frames.cancel_artwork(self.frame_generation));
        self.frame_pending = false;
        self.frame_generation = self.frame_generation.wrapping_add(1);
    }

    fn replace_artwork(&mut self, artwork: Option<LoadedArtwork>, cx: &mut Context<Self>) {
        let changed = self.view.replace(artwork, cx);
        if changed {
            cx.notify();
        }
    }

    fn prepare_artwork(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            // The physical load retires normally; hidden panels own no demand.
            return;
        }
        let cached = self
            .track
            .as_ref()
            .and_then(|track| self.manager.cached_result(crate::artwork_source(track)));
        let cold = cached.is_none();
        self.replace_artwork(cached.flatten(), cx);
        if let Some(track) = self.track.as_ref().filter(|_| cold)
            && let Some(track) = self.loader.prepare(track)
        {
            self.start_artwork(track, cx);
        }
    }

    fn start_artwork(&mut self, track: Track, cx: &mut Context<Self>) {
        let identity = ArtworkIdentity::new(crate::artwork_source(&track));
        let manager = Arc::clone(&self.manager);
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { manager.load(crate::artwork_source(&track)) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.loader.finish(&identity) {
                    return;
                }
                if this.visible
                    && this
                        .track
                        .as_ref()
                        .is_some_and(|track| identity.matches(crate::artwork_source(track)))
                {
                    this.replace_artwork(loaded, cx);
                } else {
                    this.prepare_artwork(cx);
                }
            });
        })
        .detach();
    }

    fn request_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.activation_subscription.is_none() {
            self.activation_subscription =
                Some(cx.observe_window_activation(window, |this, window, cx| {
                    this.request_frame(window, cx);
                    cx.notify();
                }));
        }
        if !self.running(window, cx) || cx.reduce_motion() {
            if self.frame_pending {
                self.cancel_frame(cx);
            }
            for instance in self.instances.values_mut() {
                instance.rotation.last_tick = None;
            }
            return;
        }
        if self.frame_pending {
            return;
        }
        self.frame_pending = true;
        let owner = cx.weak_entity();
        self.frames.update(cx, |frames, cx| {
            frames.request_artwork(owner, self.frame_generation, self.fps, window, cx);
        });
    }

    pub(super) fn animation_frame(
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
        if !self.running(window, cx) || cx.reduce_motion() {
            self.cancel_frame(cx);
            for instance in self.instances.values_mut() {
                instance.rotation.last_tick = None;
            }
            return;
        }
        let source_animated = window.is_window_active() && self.view.is_animated(cx);
        for instance in self.instances.values_mut() {
            instance.rotation.advance(now);
            if instance.visible
                && (instance.rotation.running || source_animated)
                && let Some(panel) = &instance.panel
            {
                let _ = panel.update(cx, |_, cx| cx.notify());
            }
        }
        self.request_frame(window, cx);
    }

    #[cfg(test)]
    pub(super) fn rotation_angle(&self, panel_id: u64) -> f64 {
        self.instances[&panel_id].rotation.angle
    }

    pub(super) fn release_all_images(&mut self, window: &mut Window, cx: &mut App) {
        self.visible = false;
        self.track = None;
        self.instances.clear();
        self.cancel_frame(cx);
        self.view.release_all_images(window, cx);
        for decoration in &mut self.decorations {
            if let Some(decoration) = decoration.take() {
                decoration.release(window, cx);
            }
        }
    }

    fn running(&self, window: &Window, cx: &App) -> bool {
        self.visible
            && (self
                .instances
                .values()
                .any(|instance| instance.rotation.running)
                || (window.is_window_active() && self.view.is_animated(cx)))
    }
}

fn circle_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    let side = bounds.size.width.min(bounds.size.height).max(px(0.));
    Bounds {
        origin: bounds.origin
            + point(
                (bounds.size.width - side) / 2.,
                (bounds.size.height - side) / 2.,
            ),
        size: size(side, side),
    }
}

fn artwork_bounds(bounds: Bounds<Pixels>, style: ArtworkStyle, scale: f32) -> Bounds<Pixels> {
    let circle = circle_bounds(bounds);
    let reserve = match style {
        ArtworkStyle::Vinyl | ArtworkStyle::Glass | ArtworkStyle::Metal => 0.94,
        // Reserve the entire rotating square's diagonal, including SVG shadow.
        ArtworkStyle::Card => 0.94 * std::f32::consts::FRAC_1_SQRT_2,
        ArtworkStyle::Plain | ArtworkStyle::Halo | ArtworkStyle::Duotone => 1.0,
    };
    let side = (f32::from(circle.size.width) * scale * reserve / 2.0).floor() * 2.0;
    let center = circle
        .center()
        .map(|coordinate| (f32::from(coordinate) * scale).round());
    Bounds {
        origin: center.map(|coordinate| px((coordinate - side / 2.0) / scale)),
        size: size(px(side / scale), px(side / scale)),
    }
}

fn artwork_image_ratio(style: ArtworkStyle) -> f32 {
    match style {
        ArtworkStyle::Plain => 1.0,
        ArtworkStyle::Vinyl | ArtworkStyle::Card => 0.875,
        ArtworkStyle::Glass => 0.96875,
        ArtworkStyle::Halo | ArtworkStyle::Duotone => 0.75,
        ArtworkStyle::Metal => 0.90625,
    }
}

fn artwork_decode_side(side: Pixels, style: ArtworkStyle, aspect: Option<f32>) -> Pixels {
    if style == ArtworkStyle::Card {
        return side;
    }
    let aspect = aspect.unwrap_or(1.);
    // Cover scales the shorter source edge to the circular surface, so its
    // uncropped long edge needs proportionally more pixels than Contain.
    side * aspect.max(1. / aspect)
}

impl ArtworkRuntime {
    fn render_panel(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = rivu_ui::theme::palette(cx);
        self.request_frame(window, cx);
        let image = self.view.image(cx).map(|mut image| {
            // Multiple panels share a decode, not their playback cursor.
            image.interactivity().element_id = Some(("artwork-cover", id).into());
            image = image.with_automatic_animation_frames(false);
            image
        });
        let sizing = self.view.sizing(id);
        let image_aspect = self.view.aspect_ratio(cx);
        let default_artwork = self.default_artwork.clone();
        let instance = self
            .instances
            .get(&id)
            .expect("artwork panel initialized before render");
        let style = instance.options.style;
        let mut decoration = decoration_index(style)
            .and_then(|index| self.decorations[index].as_ref())
            .map(ArtworkDecoration::image);
        let angle = instance.rotation.angle as f32;
        canvas(
            move |bounds, window, cx| {
                let scale = window.scale_factor();
                let circle = artwork_bounds(bounds, style, scale);
                let side = circle.size.width;
                let ratio = artwork_image_ratio(style);
                // Even device-pixel diameters keep all concentric layers on the
                // same center after pixel snapping, including fractional scale.
                let image_side = px((f32::from(side) * scale * ratio / 2.0).floor() * 2.0 / scale);
                if let Some(sizing) = sizing {
                    sizing.request(
                        artwork_decode_side(image_side, style, image_aspect),
                        window,
                        cx,
                    );
                }
                let inset = (side - image_side) / 2.0;
                let image = image
                    .unwrap_or_else(|| default_artwork.read(cx).image(theme, image_side, window));
                let surface = if style == ArtworkStyle::Card {
                    // The complete source fits inside the SVG's 32..480 inner
                    // square. Source and frame rotate about the same center;
                    // the square's reserved diagonal keeps every corner inside.
                    div()
                        .absolute()
                        .left(inset)
                        .top(inset)
                        .size(image_side)
                        .child(
                            image
                                .size_full()
                                .object_fit(ObjectFit::Contain)
                                .rounded(side * 0.035)
                                .with_transformation(Transformation::rotate(radians(angle))),
                        )
                } else {
                    div()
                        .absolute()
                        .left(inset)
                        .top(inset)
                        .size(image_side)
                        .rounded_full()
                        .bg(rgb(theme.bg))
                        .when(
                            matches!(style, ArtworkStyle::Vinyl | ArtworkStyle::Glass),
                            |surface| {
                                surface.shadow(vec![BoxShadow {
                                    color: rgb(0x000000).alpha(0.28).into(),
                                    offset: point(px(0.0), side * 0.008),
                                    blur_radius: side * 0.025,
                                    spread_radius: px(0.0),
                                    inset: false,
                                }])
                            },
                        )
                        .child(
                            image
                                .size_full()
                                .object_fit(ObjectFit::Cover)
                                .rounded_full()
                                .with_transformation(Transformation::rotate(radians(angle))),
                        )
                };
                let mut element = div().relative().size(side);
                let behind = matches!(style, ArtworkStyle::Halo | ArtworkStyle::Duotone);
                if behind && let Some(decoration) = decoration.take() {
                    element = element.child(decoration.absolute().size_full());
                }
                element = element.child(surface);
                if let Some(decoration) = decoration {
                    element =
                        element.child(decoration.absolute().top_0().left_0().size_full().when(
                            matches!(style, ArtworkStyle::Vinyl | ArtworkStyle::Card),
                            |image| {
                                image.with_transformation(Transformation::rotate(radians(angle)))
                            },
                        ));
                }
                let mut element = element.into_any_element();
                let _ = element.prepaint_as_root(
                    circle.origin,
                    circle.size.map(Into::into),
                    window,
                    cx,
                );
                element
            },
            |_, mut element, window, cx| element.paint(window, cx),
        )
        .size_full()
    }
}

pub(super) struct ArtworkPanel {
    id: u64,
    owner: Entity<ArtworkRuntime>,
    _subscription: gpui::Subscription,
}

impl ArtworkPanel {
    pub(super) fn new(id: u64, owner: Entity<ArtworkRuntime>, cx: &mut Context<Self>) -> Self {
        let panel = cx.weak_entity();
        owner.update(cx, |owner, _| {
            owner
                .instances
                .get_mut(&id)
                .expect("artwork panel initialized before view")
                .panel = Some(panel);
        });
        let subscription = cx.observe(&owner, |_, _, cx| cx.notify());
        Self {
            id,
            owner,
            _subscription: subscription,
        }
    }
}

impl Render for ArtworkPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.owner.update(cx, |owner, cx| {
            owner.render_panel(self.id, window, cx).into_any_element()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[gpui::test]
    fn relocation_replaces_display_without_resetting_rotation(cx: &mut gpui::TestAppContext) {
        use super::super::layout::{Node, Panel};

        let directory = tempfile::tempdir_in(".cache").unwrap();
        for (name, colour) in [("before", [255, 0, 0]), ("after", [0, 255, 0])] {
            let path = directory.path().join(name);
            std::fs::create_dir(&path).unwrap();
            image::RgbImage::from_pixel(2, 2, image::Rgb(colour))
                .save(path.join("cover.png"))
                .unwrap();
        }
        let original =
            crate::test_support::artwork_track(directory.path().join("before/track.wav"));
        let mut relocated = original.clone();
        relocated.path = directory.path().join("after/track.wav");
        let manager = Arc::new(ArtworkManager::new(directory.path()));
        let old = manager.load(crate::artwork_source(&original)).unwrap();
        let new = manager.load(crate::artwork_source(&relocated)).unwrap();
        assert_ne!(old, new);
        let config = Config::default();
        let root = Node::Tabs {
            id: 1,
            active: 2,
            panels: vec![Panel {
                id: 2,
                kind: "artwork".into(),
                show_tab_bar: false,
                audio_visualizer: None,
                artwork: Some(ArtworkOptions {
                    mode: ArtworkMode::Rotating,
                    ..(&config).into()
                }),
            }],
        };
        let mut panels = super::super::workspace::WorkspacePanels::default();
        panels.reconcile(&super::super::layout::Layout {
            root: Some(root),
            next_id: 3,
            locked: true,
        });
        let owner = cx.update(|cx| {
            let default = cx.new(|cx| DefaultArtwork::new(&Localizer::new("en"), cx));
            cx.new(|cx| {
                ArtworkRuntime::new(
                    manager,
                    default,
                    ArtworkImageRegistry::default(),
                    cx.new(|_| crate::gui::animation_frames::AnimationFrames::default()),
                )
            })
        });
        owner.update(cx, |owner, cx| {
            owner.sync(
                Some(&original),
                PlaybackStatus::Paused,
                &config,
                &panels,
                true,
                cx,
            );
            assert!(!owner.view.replace(Some(old.clone()), cx));
            owner.instances.get_mut(&2).unwrap().rotation.angle = 0.75;
            owner.sync(
                Some(&relocated),
                PlaybackStatus::Paused,
                &config,
                &panels,
                true,
                cx,
            );
            assert!(!owner.view.replace(Some(new), cx));
            assert_eq!(owner.instances[&2].rotation.angle, 0.75);
            assert!(
                !ArtworkIdentity::new(crate::artwork_source(&original))
                    .matches(crate::artwork_source(owner.track.as_ref().unwrap()))
            );
        });
        owner.update(cx, |owner, cx| {
            owner.sync(
                Some(&relocated),
                PlaybackStatus::Paused,
                &config,
                &panels,
                false,
                cx,
            );
            assert!(owner.view.image(cx).is_none());
        });
        cx.run_until_parked();
        owner.update(cx, |owner, cx| {
            for id in 100..170 {
                let mut pressure = relocated.clone();
                pressure.id = id;
                pressure.path = directory.path().join("pressure").join(format!("{id}.wav"));
                assert!(
                    owner
                        .manager
                        .load(crate::artwork_source(&pressure))
                        .is_none()
                );
            }
            assert!(
                owner
                    .manager
                    .cached_result(crate::artwork_source(&relocated))
                    .is_none()
            );
            owner.sync(
                Some(&relocated),
                PlaybackStatus::Paused,
                &config,
                &panels,
                true,
                cx,
            );
            assert!(owner.view.image(cx).is_none());
            assert!(owner.loader.prepare(&relocated).is_none());
            // Hide while the physical worker remains active, then change demand.
            owner.sync(
                Some(&relocated),
                PlaybackStatus::Paused,
                &config,
                &panels,
                false,
                cx,
            );
        });
        let mut missing = relocated.clone();
        missing.path = directory.path().join("missing/track.wav");
        owner.update(cx, |owner, cx| {
            owner.sync(
                Some(&missing),
                PlaybackStatus::Paused,
                &config,
                &panels,
                false,
                cx,
            );
        });
        cx.run_until_parked();
        owner.update(cx, |owner, cx| {
            assert!(
                owner
                    .manager
                    .cached_result(crate::artwork_source(&relocated))
                    .is_some()
            );
            assert!(
                owner
                    .manager
                    .cached_result(crate::artwork_source(&missing))
                    .is_none()
            );
            assert!(owner.view.image(cx).is_none());
            owner.sync(
                Some(&missing),
                PlaybackStatus::Paused,
                &config,
                &panels,
                true,
                cx,
            );
        });
        cx.run_until_parked();
        owner.update(cx, |owner, cx| {
            assert_eq!(
                owner.manager.cached_result(crate::artwork_source(&missing)),
                Some(None)
            );
            owner.sync(
                Some(&missing),
                PlaybackStatus::Paused,
                &config,
                &panels,
                false,
                cx,
            );
            owner.sync(
                Some(&missing),
                PlaybackStatus::Paused,
                &config,
                &panels,
                true,
                cx,
            );
            assert!(owner.view.image(cx).is_none());
            assert!(owner.loader.prepare(&missing).is_some());
            assert!(
                owner
                    .loader
                    .finish(&ArtworkIdentity::new(crate::artwork_source(&missing)))
            );
        });
    }

    fn rotating() -> RotationSettings {
        RotationSettings {
            mode: ArtworkMode::Rotating,
            period_seconds: 20.,
            direction: ArtworkDirection::Clockwise,
        }
    }

    fn assert_angle(rotation: &Rotation, expected: f64) {
        assert!((rotation.angle - expected).abs() < 1e-10);
    }

    #[test]
    fn cover_decode_demand_preserves_short_edge_density() {
        for aspect in [2., 0.5] {
            assert_eq!(
                artwork_decode_side(px(512.), ArtworkStyle::Plain, Some(aspect)),
                px(1024.)
            );
            assert_eq!(
                artwork_decode_side(px(512.), ArtworkStyle::Vinyl, Some(aspect)),
                px(1024.)
            );
            assert_eq!(
                artwork_decode_side(px(512.), ArtworkStyle::Card, Some(aspect)),
                px(512.)
            );
        }
        assert_eq!(
            artwork_decode_side(px(512.), ArtworkStyle::Plain, Some(1.)),
            px(512.)
        );
        assert_eq!(
            artwork_decode_side(px(512.), ArtworkStyle::Plain, None),
            px(512.)
        );
    }

    #[test]
    fn unchanged_sync_does_not_advance_between_animation_frames() {
        let start = Instant::now();
        let mut rotation = Rotation::new();
        rotation.sync(start, rotating(), true, true);
        rotation.advance(start);
        for millis in [50, 100, 200, 500, 999] {
            assert!(!rotation.sync(
                start + Duration::from_millis(millis),
                rotating(),
                true,
                false,
            ));
            assert_angle(&rotation, 0.);
        }
        rotation.advance(start + Duration::from_secs(1));
        assert_angle(&rotation, std::f64::consts::TAU / 20.);
    }

    #[test]
    fn suspension_resumes_without_elapsed_catchup() {
        let start = Instant::now();
        let mut rotation = Rotation::new();
        rotation.sync(start, rotating(), true, true);
        rotation.advance(start);
        rotation.sync(start + Duration::from_secs(5), rotating(), false, false);
        assert_angle(&rotation, std::f64::consts::FRAC_PI_2);
        rotation.sync(start + Duration::from_secs(100), rotating(), true, false);
        rotation.advance(start + Duration::from_secs(100));
        assert_angle(&rotation, std::f64::consts::FRAC_PI_2);
        rotation.advance(start + Duration::from_secs(105));
        assert_angle(&rotation, std::f64::consts::PI);
    }

    #[test]
    fn static_stop_and_track_switch_reset_upright() {
        let start = Instant::now();
        for (settings, reset) in [
            (
                RotationSettings {
                    mode: ArtworkMode::Static,
                    ..rotating()
                },
                false,
            ),
            (rotating(), true),
        ] {
            let mut rotation = Rotation::new();
            rotation.sync(start, rotating(), true, true);
            rotation.advance(start);
            rotation.advance(start + Duration::from_secs(5));
            rotation.sync(start + Duration::from_secs(5), settings, false, reset);
            assert_angle(&rotation, 0.);
            rotation.advance(start + Duration::from_secs(100));
            assert_angle(&rotation, 0.);
        }
    }

    #[test]
    fn speed_and_direction_preserve_angle_and_use_new_rate() {
        let start = Instant::now();
        let mut rotation = Rotation::new();
        rotation.sync(start, rotating(), true, true);
        rotation.advance(start);
        rotation.advance(start + Duration::from_secs(5));
        let next = RotationSettings {
            period_seconds: 40.,
            direction: ArtworkDirection::Counterclockwise,
            ..rotating()
        };
        rotation.sync(start + Duration::from_secs(5), next, true, false);
        assert_angle(&rotation, std::f64::consts::FRAC_PI_2);
        rotation.advance(start + Duration::from_secs(5));
        rotation.advance(start + Duration::from_secs(10));
        assert_angle(&rotation, std::f64::consts::FRAC_PI_4);
    }

    #[test]
    fn long_rotation_wraps() {
        let start = Instant::now();
        let mut rotation = Rotation::new();
        rotation.sync(start, rotating(), true, true);
        rotation.advance(start);
        rotation.advance(start + Duration::from_secs(45));
        assert_angle(&rotation, std::f64::consts::FRAC_PI_2);
    }

    #[test]
    fn styled_artwork_stays_bounded_and_concentric_at_fractional_scale() {
        let bounds = Bounds {
            origin: point(px(10.25), px(20.25)),
            size: size(px(997.5), px(601.25)),
        };
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let center = bounds
                .center()
                .map(|coordinate| (f32::from(coordinate) * scale).round());
            for style in [
                ArtworkStyle::Plain,
                ArtworkStyle::Vinyl,
                ArtworkStyle::Glass,
                ArtworkStyle::Halo,
                ArtworkStyle::Metal,
                ArtworkStyle::Card,
                ArtworkStyle::Duotone,
            ] {
                let circle = artwork_bounds(bounds, style, scale);
                assert!(circle.size.width >= px(0.0));
                assert!(circle.size.width <= bounds.size.height);
                assert_eq!(circle.size.width, circle.size.height);
                let actual = circle
                    .center()
                    .map(|coordinate| f32::from(coordinate) * scale);
                assert!((actual.x - center.x).abs() < 0.001);
                assert!((actual.y - center.y).abs() < 0.001);
            }
        }
    }

    #[test]
    fn card_preserves_inner_square_and_bounds_every_rotated_corner() {
        for (width, height) in [(32., 96.), (400., 200.), (200., 400.), (997.5, 601.25)] {
            let bounds = Bounds {
                origin: point(px(10.25), px(20.25)),
                size: size(px(width), px(height)),
            };
            for scale in [1.0, 1.25, 1.5, 2.0] {
                let card = artwork_bounds(bounds, ArtworkStyle::Card, scale);
                for step in 0..32 {
                    let angle = step as f32 * std::f32::consts::TAU / 32.;
                    let extent = card.size.width * (angle.cos().abs() + angle.sin().abs()) / 2.;
                    let center = card.center();
                    assert!(center.x - extent >= bounds.origin.x);
                    assert!(center.y - extent >= bounds.origin.y);
                    assert!(center.x + extent <= bounds.origin.x + bounds.size.width);
                    assert!(center.y + extent <= bounds.origin.y + bounds.size.height);
                }
            }
        }
    }
}
