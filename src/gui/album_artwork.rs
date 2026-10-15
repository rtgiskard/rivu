use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    App, Bounds, BoxShadow, Context, Entity, Image, ImageFormat, ObjectFit, Pixels, Render, Task,
    Transformation, Window, canvas, div, img, point, prelude::*, px, radians, rgb, size,
};

use crate::{
    artwork::{Artwork as LoadedArtwork, ArtworkIdentity, ArtworkManager},
    config::{AlbumArtworkDirection, AlbumArtworkMode, AlbumArtworkStyle, Config},
    model::{PlaybackStatus, Track},
};

use super::artwork::{Artwork, ArtworkLoader, ArtworkView};

const FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);

#[derive(Clone, Copy, PartialEq)]
struct RotationSettings {
    mode: AlbumArtworkMode,
    period_seconds: f32,
    direction: AlbumArtworkDirection,
}

impl RotationSettings {
    fn from_config(config: &Config) -> Self {
        Self {
            mode: config.album_artwork_mode,
            period_seconds: config.album_artwork_period_seconds,
            direction: config.album_artwork_direction,
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
        self.advance(now);
        let running = eligible && settings.mode == AlbumArtworkMode::Rotating;
        let changed = reset || self.settings != settings || self.running != running;
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

/// All panel instances share one source owner, rotation state, frame callback
/// and cancellable deadline. Source type and style never select another clock.
pub(super) struct AlbumArtwork {
    manager: Arc<ArtworkManager>,
    loader: ArtworkLoader,
    view: ArtworkView,
    default_album: Entity<Artwork>,
    track: Option<Track>,
    status: PlaybackStatus,
    visible: bool,
    still: bool,
    style: AlbumArtworkStyle,
    decorations: [Arc<Image>; 3],
    rotation: Rotation,
    frame_pending: bool,
    frame_generation: u64,
    frame_timer: Option<Task<()>>,
}

impl AlbumArtwork {
    pub(super) fn new(manager: Arc<ArtworkManager>, default_album: Entity<Artwork>) -> Self {
        Self {
            manager,
            loader: ArtworkLoader::default(),
            view: ArtworkView::default(),
            default_album,
            track: None,
            status: PlaybackStatus::Stopped,
            visible: false,
            still: false,
            style: AlbumArtworkStyle::Plain,
            decorations: [
                include_bytes!("../../assets/album_vinyl.svg").as_slice(),
                include_bytes!("../../assets/album_glass.svg").as_slice(),
                include_bytes!("../../assets/album_halo.svg").as_slice(),
            ]
            .map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Svg, bytes.to_vec()))),
            rotation: Rotation::new(),
            frame_pending: false,
            frame_generation: 0,
            frame_timer: None,
        }
    }

    pub(super) fn sync(
        &mut self,
        track: Option<&Track>,
        status: PlaybackStatus,
        config: &Config,
        visible: bool,
        cx: &mut Context<Self>,
    ) {
        let switched = match (&self.track, track) {
            (Some(previous), Some(next)) => {
                previous.id != next.id || previous.fingerprint != next.fingerprint
            }
            (None, None) => false,
            _ => true,
        };
        let settings = RotationSettings::from_config(config);
        let stopped = status == PlaybackStatus::Stopped && self.status != status;
        let changed = switched
            || self.status != status
            || self.visible != visible
            || self.still != config.still_mode
            || self.style != config.album_artwork_style
            || self.rotation.settings != settings;
        if switched {
            self.track = track.cloned();
            // Do not show the previous track while the latest lookup is pending.
            self.view.replace(None, cx);
        }
        self.status = status;
        self.visible = visible;
        self.still = config.still_mode;
        self.style = config.album_artwork_style;
        self.sync_rotation(settings, switched || stopped);
        self.prepare_artwork(cx);
        if changed {
            cx.notify();
        }
    }

    fn sync_rotation(&mut self, settings: RotationSettings, reset: bool) {
        let eligible = self.visible && !self.still && self.status == PlaybackStatus::Playing;
        if self
            .rotation
            .sync(Instant::now(), settings, eligible, reset)
        {
            self.cancel_frame();
        }
    }

    fn cancel_frame(&mut self) {
        self.frame_timer = None;
        self.frame_pending = false;
        // Native next-frame callbacks cannot be removed, so obsolete callbacks
        // are inert even if hiding and reopening happen before they are drained.
        self.frame_generation = self.frame_generation.wrapping_add(1);
    }

    fn replace_artwork(&mut self, artwork: Option<LoadedArtwork>, cx: &mut Context<Self>) {
        let artwork = artwork.filter(|artwork| !artwork.is_placeholder());
        let changed = self.view.replace(artwork, cx);
        self.sync_rotation(self.rotation.settings, false);
        if changed {
            cx.notify();
        }
    }

    fn prepare_artwork(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            // Forget queued demand, but let the shared in-flight load finish.
            let _ = self.loader.prepare(None);
            return;
        }
        let artwork = self
            .track
            .as_ref()
            .and_then(|track| self.manager.cached(track));
        self.replace_artwork(artwork, cx);
        let current = self
            .track
            .as_ref()
            .filter(|track| !self.manager.is_loaded(track));
        if let Some(track) = self.loader.prepare(current) {
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
                if this
                    .track
                    .as_ref()
                    .is_some_and(|track| identity.matches(track))
                {
                    this.replace_artwork(loaded, cx);
                }
                let current = this
                    .track
                    .as_ref()
                    .filter(|track| this.visible && !this.manager.is_loaded(track));
                if let Some(next) = this.loader.finish(&identity, current) {
                    this.start_artwork(next, cx);
                } else {
                    this.prepare_artwork(cx);
                }
            });
        })
        .detach();
    }

    fn request_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.rotation.running || self.frame_pending || self.frame_timer.is_some() {
            return;
        }
        self.frame_pending = true;
        let generation = self.frame_generation;
        let view = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = view.update(cx, |this, cx| {
                if generation != this.frame_generation || !this.rotation.running {
                    return;
                }
                this.frame_pending = false;
                this.rotation.advance(Instant::now());
                cx.notify();
                let timer = cx.background_executor().timer(FRAME_INTERVAL);
                this.frame_timer = Some(cx.spawn_in(window, async move |this, cx| {
                    timer.await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        if generation == this.frame_generation {
                            this.frame_timer = None;
                            this.request_frame(window, cx);
                        }
                    });
                }));
            });
        });
        cx.notify();
    }

    pub(super) fn image_releaser(&self) -> ArtworkView {
        self.view.clone()
    }

    pub(super) fn release_all_images(&mut self, window: &mut Window, cx: &mut App) {
        self.visible = false;
        self.track = None;
        let _ = self.loader.prepare(None);
        self.rotation.running = false;
        self.rotation.last_tick = None;
        self.cancel_frame();
        self.view.release_all_images(window, cx);
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
        AlbumArtworkStyle::Vinyl | AlbumArtworkStyle::Glass => 0.94,
        AlbumArtworkStyle::Plain | AlbumArtworkStyle::Halo => 1.0,
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

impl Render for AlbumArtwork {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_rotation(self.rotation.settings, false);
        self.request_frame(window, cx);
        let image = self
            .view
            .image(cx)
            .unwrap_or_else(|| self.default_album.read(cx).image());
        let style = self.style;
        let decoration = match style {
            AlbumArtworkStyle::Plain => None,
            AlbumArtworkStyle::Vinyl => Some(self.decorations[0].clone()),
            AlbumArtworkStyle::Glass => Some(self.decorations[1].clone()),
            AlbumArtworkStyle::Halo => Some(self.decorations[2].clone()),
        };
        let angle = self.rotation.angle as f32;
        canvas(
            move |bounds, window, cx| {
                let scale = window.scale_factor();
                let circle = artwork_bounds(bounds, style, scale);
                let side = circle.size.width;
                let ratio = match style {
                    AlbumArtworkStyle::Plain => 1.0,
                    AlbumArtworkStyle::Vinyl => 0.875,
                    AlbumArtworkStyle::Glass => 0.96875,
                    AlbumArtworkStyle::Halo => 0.75,
                };
                // Even device-pixel diameters keep all concentric layers on the
                // same center after pixel snapping, including fractional scale.
                let image_side = px((f32::from(side) * scale * ratio / 2.0).floor() * 2.0 / scale);
                let inset = (side - image_side) / 2.0;
                let surface = div()
                    .absolute()
                    .left(inset)
                    .top(inset)
                    .size(image_side)
                    .rounded_full()
                    .bg(rgb(super::BG))
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
                    );
                let mut element = div().relative().size(side);
                if style == AlbumArtworkStyle::Halo {
                    element =
                        element.child(img(decoration.clone().unwrap()).absolute().size_full());
                }
                element = element.child(surface);
                if let Some(decoration) = decoration.filter(|_| style != AlbumArtworkStyle::Halo) {
                    element = element.child(
                        img(decoration)
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .when(style == AlbumArtworkStyle::Vinyl, |image| {
                                image.with_transformation(Transformation::rotate(radians(angle)))
                            }),
                    );
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
