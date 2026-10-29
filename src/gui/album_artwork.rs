use std::{collections::HashMap, sync::Arc, time::Instant};

use gpui::{
    App, Bounds, BoxShadow, Context, Entity, ObjectFit, Pixels, Render, Transformation, Window,
    canvas, div, point, prelude::*, px, radians, rgb, size,
};

use crate::{
    artwork::{Artwork as LoadedArtwork, ArtworkIdentity, ArtworkManager},
    config::{
        AlbumArtworkDirection, AlbumArtworkMode, AlbumArtworkOptions, AlbumArtworkStyle, Config,
    },
    i18n::Localizer,
};
use rivu_protocol::model::{PlaybackStatus, Track};

use super::animation_frames::AnimationFrames;
use super::artwork::{
    ArtworkDecoration, ArtworkImageRegistry, ArtworkLoader, ArtworkView, DefaultArtwork,
};

pub(super) const ALBUM_ARTWORK_STYLES: [AlbumArtworkStyle; 7] = [
    AlbumArtworkStyle::Plain,
    AlbumArtworkStyle::Card,
    AlbumArtworkStyle::Vinyl,
    AlbumArtworkStyle::Glass,
    AlbumArtworkStyle::Metal,
    AlbumArtworkStyle::Halo,
    AlbumArtworkStyle::Duotone,
];

pub(super) fn album_artwork_style_id(style: AlbumArtworkStyle) -> &'static str {
    match style {
        AlbumArtworkStyle::Plain => "settings-plain",
        AlbumArtworkStyle::Vinyl => "settings-vinyl",
        AlbumArtworkStyle::Glass => "settings-glass",
        AlbumArtworkStyle::Halo => "settings-halo",
        AlbumArtworkStyle::Metal => "settings-metal",
        AlbumArtworkStyle::Card => "settings-card",
        AlbumArtworkStyle::Duotone => "settings-duotone",
    }
}

pub(super) fn album_artwork_style_label(i18n: &Localizer, style: AlbumArtworkStyle) -> &str {
    i18n.text(album_artwork_style_id(style))
}

#[derive(Clone, Copy, PartialEq)]
struct RotationSettings {
    mode: AlbumArtworkMode,
    period_seconds: f32,
    direction: AlbumArtworkDirection,
}

impl RotationSettings {
    fn from_options(options: AlbumArtworkOptions) -> Self {
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
                mode: AlbumArtworkMode::Static,
                period_seconds: 20.,
                direction: AlbumArtworkDirection::Clockwise,
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
                AlbumArtworkDirection::Clockwise => 1.,
                AlbumArtworkDirection::Counterclockwise => -1.,
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
        let running = eligible && settings.mode == AlbumArtworkMode::Rotating;
        let changed = reset || self.settings != settings || self.running != running;
        if changed {
            self.advance(now);
        }
        if reset || settings.mode == AlbumArtworkMode::Static {
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
    options: AlbumArtworkOptions,
    rotation: Rotation,
    visible: bool,
}

const DECORATIONS: [&[u8]; 6] = [
    include_bytes!("../../data/artwork/vinyl.svg"),
    include_bytes!("../../data/artwork/glass.svg"),
    include_bytes!("../../data/artwork/halo.svg"),
    include_bytes!("../../data/artwork/metal.svg"),
    include_bytes!("../../data/artwork/card.svg"),
    include_bytes!("../../data/artwork/duotone.svg"),
];

fn decoration_index(style: AlbumArtworkStyle) -> Option<usize> {
    match style {
        AlbumArtworkStyle::Plain => None,
        AlbumArtworkStyle::Vinyl => Some(0),
        AlbumArtworkStyle::Glass => Some(1),
        AlbumArtworkStyle::Halo => Some(2),
        AlbumArtworkStyle::Metal => Some(3),
        AlbumArtworkStyle::Card => Some(4),
        AlbumArtworkStyle::Duotone => Some(5),
    }
}

/// Instances share one source owner, frame callback and cancellable deadline.
/// Rotation and source-animation cursors remain local to individual panels.
pub(super) struct AlbumArtwork {
    manager: Arc<ArtworkManager>,
    loader: ArtworkLoader,
    view: ArtworkView,
    default_album: Entity<DefaultArtwork>,
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

impl AlbumArtwork {
    pub(super) fn new(
        manager: Arc<ArtworkManager>,
        default_album: Entity<DefaultArtwork>,
        images: ArtworkImageRegistry,
        frames: Entity<AnimationFrames>,
    ) -> Self {
        Self {
            manager,
            loader: ArtworkLoader::default(),
            view: ArtworkView::new(images.clone()),
            default_album,
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
                    .is_some_and(|panel| panel.album_artwork.is_some())
            });
            for panel in panels.panels() {
                let Some(options) = panel.album_artwork else {
                    continue;
                };
                let instance = self.instances.entry(panel.id).or_insert_with(|| {
                    changed = true;
                    Instance {
                        options,
                        rotation: Rotation::new(),
                        visible: false,
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
        changed |= self.visible != visible || self.fps != config.album_artwork_fps;
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
        self.fps = config.album_artwork_fps;
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
            .and_then(|track| self.manager.cached_result(track));
        let cold = cached.is_none();
        self.replace_artwork(cached.flatten(), cx);
        if let Some(track) = self.track.as_ref().filter(|_| cold)
            && let Some(track) = self.loader.prepare(track)
        {
            self.start_artwork(track, cx);
        }
    }

    fn start_artwork(&mut self, track: Track, cx: &mut Context<Self>) {
        let identity = ArtworkIdentity::for_track(&track);
        let manager = Arc::clone(&self.manager);
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { manager.load(&track) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.loader.finish(&identity) {
                    return;
                }
                if this.visible
                    && this
                        .track
                        .as_ref()
                        .is_some_and(|track| identity.matches(track))
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
        for instance in self.instances.values_mut() {
            instance.rotation.advance(now);
        }
        cx.notify();
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

fn artwork_bounds(bounds: Bounds<Pixels>, style: AlbumArtworkStyle, scale: f32) -> Bounds<Pixels> {
    let circle = circle_bounds(bounds);
    let reserve = match style {
        AlbumArtworkStyle::Vinyl | AlbumArtworkStyle::Glass | AlbumArtworkStyle::Metal => 0.94,
        // Reserve the entire rotating square's diagonal, including SVG shadow.
        AlbumArtworkStyle::Card => 0.94 * std::f32::consts::FRAC_1_SQRT_2,
        AlbumArtworkStyle::Plain | AlbumArtworkStyle::Halo | AlbumArtworkStyle::Duotone => 1.0,
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

fn artwork_image_ratio(style: AlbumArtworkStyle) -> f32 {
    match style {
        AlbumArtworkStyle::Plain => 1.0,
        AlbumArtworkStyle::Vinyl | AlbumArtworkStyle::Card => 0.875,
        AlbumArtworkStyle::Glass => 0.96875,
        AlbumArtworkStyle::Halo | AlbumArtworkStyle::Duotone => 0.75,
        AlbumArtworkStyle::Metal => 0.90625,
    }
}

fn artwork_decode_side(side: Pixels, style: AlbumArtworkStyle, aspect: Option<f32>) -> Pixels {
    if style == AlbumArtworkStyle::Card {
        return side;
    }
    let aspect = aspect.unwrap_or(1.);
    // Cover scales the shorter source edge to the circular surface, so its
    // uncropped long edge needs proportionally more pixels than Contain.
    side * aspect.max(1. / aspect)
}

impl AlbumArtwork {
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
            image.interactivity().element_id = Some(("album-artwork-cover", id).into());
            image = image.with_automatic_animation_frames(false);
            image
        });
        let sizing = self.view.sizing(id);
        let image_aspect = self.view.aspect_ratio(cx);
        let default_album = self.default_album.clone();
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
                    .unwrap_or_else(|| default_album.read(cx).image(theme, image_side, window));
                let surface = if style == AlbumArtworkStyle::Card {
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
                            matches!(style, AlbumArtworkStyle::Vinyl | AlbumArtworkStyle::Glass),
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
                let behind = matches!(style, AlbumArtworkStyle::Halo | AlbumArtworkStyle::Duotone);
                if behind && let Some(decoration) = decoration.take() {
                    element = element.child(decoration.absolute().size_full());
                }
                element = element.child(surface);
                if let Some(decoration) = decoration {
                    element =
                        element.child(decoration.absolute().top_0().left_0().size_full().when(
                            matches!(style, AlbumArtworkStyle::Vinyl | AlbumArtworkStyle::Card),
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

pub(super) struct AlbumArtworkPanel {
    id: u64,
    owner: Entity<AlbumArtwork>,
    _subscription: gpui::Subscription,
}

impl AlbumArtworkPanel {
    pub(super) fn new(id: u64, owner: Entity<AlbumArtwork>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&owner, |_, _, cx| cx.notify());
        Self {
            id,
            owner,
            _subscription: subscription,
        }
    }
}

impl Render for AlbumArtworkPanel {
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
        let original = crate::artwork::tests::track(directory.path().join("before/track.wav"));
        let mut relocated = original.clone();
        relocated.path = directory.path().join("after/track.wav");
        let manager = Arc::new(ArtworkManager::new(directory.path()));
        let old = manager.load(&original).unwrap();
        let new = manager.load(&relocated).unwrap();
        assert_ne!(old, new);
        let config = Config::default();
        let root = Node::Tabs {
            id: 1,
            active: 2,
            panels: vec![Panel {
                id: 2,
                kind: "album_artwork".into(),
                show_tab_bar: false,
                audio_visualizer: None,
                album_artwork: Some(AlbumArtworkOptions {
                    mode: AlbumArtworkMode::Rotating,
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
                AlbumArtwork::new(
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
            assert!(!ArtworkIdentity::for_track(&original).matches(owner.track.as_ref().unwrap()));
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
                assert!(owner.manager.load(&pressure).is_none());
            }
            assert!(owner.manager.cached_result(&relocated).is_none());
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
            assert!(owner.manager.cached_result(&relocated).is_some());
            assert!(owner.manager.cached_result(&missing).is_none());
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
            assert_eq!(owner.manager.cached_result(&missing), Some(None));
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
            assert!(owner.loader.finish(&ArtworkIdentity::for_track(&missing)));
        });
    }

    fn rotating() -> RotationSettings {
        RotationSettings {
            mode: AlbumArtworkMode::Rotating,
            period_seconds: 20.,
            direction: AlbumArtworkDirection::Clockwise,
        }
    }

    fn assert_angle(rotation: &Rotation, expected: f64) {
        assert!((rotation.angle - expected).abs() < 1e-10);
    }

    #[test]
    fn cover_decode_demand_preserves_short_edge_density() {
        for aspect in [2., 0.5] {
            assert_eq!(
                artwork_decode_side(px(512.), AlbumArtworkStyle::Plain, Some(aspect)),
                px(1024.)
            );
            assert_eq!(
                artwork_decode_side(px(512.), AlbumArtworkStyle::Vinyl, Some(aspect)),
                px(1024.)
            );
            assert_eq!(
                artwork_decode_side(px(512.), AlbumArtworkStyle::Card, Some(aspect)),
                px(512.)
            );
        }
        assert_eq!(
            artwork_decode_side(px(512.), AlbumArtworkStyle::Plain, Some(1.)),
            px(512.)
        );
        assert_eq!(
            artwork_decode_side(px(512.), AlbumArtworkStyle::Plain, None),
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
                    mode: AlbumArtworkMode::Static,
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
            direction: AlbumArtworkDirection::Counterclockwise,
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
    fn circle_is_centered_and_bounded_in_wide_and_tall_panels() {
        for (width, height, x, y) in [
            (400., 200., 110., 20.),
            (200., 400., 10., 120.),
            (0., 200., 10., 120.),
        ] {
            let circle = circle_bounds(Bounds {
                origin: point(px(10.), px(20.)),
                size: size(px(width), px(height)),
            });
            assert_eq!(circle.origin, point(px(x), px(y)));
            assert_eq!(
                circle.size,
                size(px(width.min(height)), px(width.min(height)))
            );
        }
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
                AlbumArtworkStyle::Plain,
                AlbumArtworkStyle::Vinyl,
                AlbumArtworkStyle::Glass,
                AlbumArtworkStyle::Halo,
                AlbumArtworkStyle::Metal,
                AlbumArtworkStyle::Card,
                AlbumArtworkStyle::Duotone,
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
        assert_eq!(artwork_image_ratio(AlbumArtworkStyle::Card), 448. / 512.);
        for (width, height) in [(32., 96.), (400., 200.), (200., 400.), (997.5, 601.25)] {
            let bounds = Bounds {
                origin: point(px(10.25), px(20.25)),
                size: size(px(width), px(height)),
            };
            for scale in [1.0, 1.25, 1.5, 2.0] {
                let card = artwork_bounds(bounds, AlbumArtworkStyle::Card, scale);
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
