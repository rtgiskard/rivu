//! One atmosphere consumer, independent of whether an artwork panel is present.
use super::material::MaterialAppearance;
use crate::gui::artwork::{ArtworkImageRegistry, ArtworkLoader, ArtworkSource, ArtworkView};
use gpui::{App, Context, RenderImage, Window};
use rivu_format::config::MaterialPreset;
use rivu_metadata::artwork::{Artwork, ArtworkIdentity, ArtworkManager};
use rivu_protocol::model::Track;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub(in crate::gui) struct Atmosphere {
    manager: Arc<ArtworkManager>,
    loader: ArtworkLoader,
    view: ArtworkView,
    previous: ArtworkView,
    material: Option<MaterialPreset>,
    transition_enabled: bool,
    transition_started: Option<Instant>,
    generation: u64,
    track: Option<Track>,
    enabled: bool,
}

impl Atmosphere {
    pub(in crate::gui) fn new(manager: Arc<ArtworkManager>, images: ArtworkImageRegistry) -> Self {
        Self {
            manager,
            loader: ArtworkLoader::default(),
            view: ArtworkView::new(images.clone()),
            previous: ArtworkView::new(images),
            material: None,
            transition_enabled: false,
            transition_started: None,
            generation: 0,
            track: None,
            enabled: false,
        }
    }

    /// Reconcile material resources and transition ownership as one update.
    /// Knob updates within one material keep its current transition clock.
    pub(in crate::gui) fn sync(
        &mut self,
        track: Option<&Track>,
        material: MaterialPreset,
        transition_seconds: f32,
        visible: bool,
        playback_visible: bool,
        cx: &mut Context<Self>,
    ) {
        let enabled = material.spec().artwork_demand(visible, playback_visible);
        let transition_enabled = material.is_texture() && transition_seconds > 0.;
        let material_changed = self.material.replace(material) != Some(material);
        let transition_disabled = self.transition_enabled && !transition_enabled;
        self.transition_enabled = transition_enabled;
        if (material_changed || transition_disabled) && self.clear_previous(cx) {
            cx.notify();
        }
        let switched = match (&self.track, track) {
            (Some(old), Some(new)) => old.id != new.id || old.fingerprint != new.fingerprint,
            (None, None) => false,
            _ => true,
        };
        let path_changed = self
            .track
            .as_ref()
            .zip(track)
            .is_some_and(|(old, new)| old.path != new.path);
        if switched || path_changed || self.enabled != enabled {
            self.generation = self.generation.wrapping_add(1);
        }
        if switched || self.enabled && !enabled {
            self.clear_previous(cx);
            if switched && self.enabled && enabled && self.transition_enabled && track.is_some() {
                // Move the existing lease, never a bitmap or a composite frame.
                // A second switch discards the older predecessor first.
                std::mem::swap(&mut self.view, &mut self.previous);
            } else {
                self.view.clear_ambient(cx);
                self.view.replace(None, cx);
            }
        }
        if switched || path_changed {
            self.track = track.cloned();
        }
        self.enabled = enabled;
        self.prepare(cx);
    }

    fn prepare(&mut self, cx: &mut Context<Self>) {
        self.loader
            .retain_negative(self.track.as_ref().filter(|_| self.enabled));
        if !self.enabled {
            return;
        }
        let cached = self
            .track
            .as_ref()
            .and_then(|track| self.manager.cached_result(crate::artwork_source(track)));
        if let Some(result) = &cached
            && let Some(track) = &self.track
        {
            self.loader.remember_result(
                ArtworkIdentity::new(crate::artwork_source(track)),
                result.is_some(),
            );
        }
        let resolved = self.track.is_none() || cached.is_some();
        self.apply_target(cached.flatten(), resolved, cx);
        if let Some(track) = self.track.as_ref().filter(|_| !resolved)
            && let Some(track) = self.loader.prepare(track)
        {
            self.start(track, cx);
        }
    }

    fn clear_previous(&mut self, cx: &mut App) -> bool {
        self.transition_started = None;
        self.previous.clear_ambient(cx);
        self.previous.replace(None, cx)
    }

    fn apply_target(&mut self, artwork: Option<Artwork>, resolved: bool, cx: &mut Context<Self>) {
        // Keep the predecessor only while a real target is pending/transitioning.
        // A missing cover or failed lookup must not display another track's art.
        let cleared = artwork.is_none() && resolved && self.clear_previous(cx);
        let replaced = self.view.replace(artwork, cx);
        if replaced {
            self.transition_started = None;
        }
        if cleared || replaced {
            cx.notify();
        }
    }

    fn start(&mut self, track: Track, cx: &mut Context<Self>) {
        let identity = ArtworkIdentity::new(crate::artwork_source(&track));
        let manager = Arc::clone(&self.manager);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let artwork = cx
                .background_executor()
                .spawn(async move { manager.load(crate::artwork_source(&track)) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.loader.finish(&identity) {
                    return;
                }
                if this.enabled
                    && this.generation == generation
                    && this
                        .track
                        .as_ref()
                        .is_some_and(|track| identity.matches(crate::artwork_source(track)))
                {
                    this.loader.remember_result(identity, artwork.is_some());
                    this.apply_target(artwork, true, cx);
                } else {
                    this.prepare(cx);
                }
            });
        })
        .detach();
    }

    pub(in crate::gui) fn source(&self) -> Option<ArtworkSource> {
        self.view.source()
    }

    /// Previous/current full-resolution sources plus the current source's weight.
    /// Call from the root render via `Entity::update`. The GPUI executor clock
    /// drives only live fades; pending decodes notify through the image cache.
    pub(in crate::gui) fn transition_sources(
        &mut self,
        duration: Duration,
        motion: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Option<Arc<RenderImage>>, Option<Arc<RenderImage>>, f32) {
        if !self.enabled {
            self.clear_previous(cx);
            return (None, None, 1.);
        }
        let animate =
            self.transition_enabled && motion && !cx.reduce_motion() && !duration.is_zero();
        if !animate {
            self.clear_previous(cx);
        }
        let current = match self.view.ready_static_source(window, cx) {
            Some(Ok(image)) => Some(image),
            Some(Err(_)) => {
                // Preserve the terminal decode error in its cache lease so a
                // render cannot restart it; release only the now-wrong old art.
                self.clear_previous(cx);
                return (None, None, 1.);
            }
            None => None,
        };
        if !animate {
            return (None, current, 1.);
        }
        let previous = match self.previous.cached_static_source(window, cx) {
            Some(Ok(image)) => Some(image),
            Some(Err(_)) => {
                self.clear_previous(cx);
                None
            }
            None => None,
        };
        let Some(previous) = previous else {
            // Do not revive an undecoded predecessor after the target is visible.
            if current.is_some() {
                self.clear_previous(cx);
            }
            return (None, current, 1.);
        };
        let Some(current) = current else {
            return (Some(previous), None, 0.);
        };
        let now = cx.background_executor().now();
        let start = *self.transition_started.get_or_insert(now);
        let phase =
            (now.duration_since(start).as_secs_f64() / duration.as_secs_f64()).min(1.) as f32;
        if phase >= 1. {
            self.clear_previous(cx);
            return (None, Some(current), 1.);
        }
        window.request_animation_frame();
        (Some(previous), Some(current), phase)
    }

    pub(in crate::gui) fn release(&mut self, window: &mut Window, cx: &mut App) {
        self.enabled = false;
        self.loader.retain_negative(None);
        self.generation = self.generation.wrapping_add(1);
        self.transition_started = None;
        self.view.clear_ambient(cx);
        self.view.release_all_images(window, cx);
        self.previous.clear_ambient(cx);
        self.previous.release_all_images(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ConfigFixture;
    use gpui::{AppContext, Entity, IntoElement, Render, div, prelude::*};
    use std::{cell::Cell, rc::Rc};

    fn sources() -> (tempfile::TempDir, Arc<ArtworkManager>, [Track; 4]) {
        std::fs::create_dir_all(".cache").unwrap();
        let directory = tempfile::tempdir_in(".cache").unwrap();
        let manager = Arc::new(ArtworkManager::new(directory.path()));
        let tracks = std::array::from_fn(|index| {
            let mut track = crate::test_support::artwork_track(
                directory.path().join(format!("track-{index}.wav")),
            );
            track.id = index as i64 + 1;
            if index < 2 {
                rivu_metadata::artwork::test_support::tagged_wav(&track.path);
                manager.load(crate::artwork_source(&track)).unwrap();
            }
            track
        });
        (directory, manager, tracks)
    }

    #[gpui::test]
    fn material_scope_and_visibility_reconcile_without_sampling(cx: &mut gpui::TestAppContext) {
        let (_directory, manager, tracks) = sources();
        let atmosphere = cx.new(|_| Atmosphere::new(manager, ArtworkImageRegistry::default()));
        atmosphere.update(cx, |this, cx| {
            for (material, visible, playback_visible, expected) in [
                (MaterialPreset::Glass, true, false, true),
                (MaterialPreset::Atmosphere, true, false, false),
                (MaterialPreset::Atmosphere, true, true, true),
                (MaterialPreset::Atmosphere, false, true, false),
                (MaterialPreset::Dissolve, true, false, true),
                (MaterialPreset::Dissolve, false, false, false),
                (MaterialPreset::Mirror, true, false, true),
                (MaterialPreset::Original, true, true, false),
            ] {
                this.sync(
                    Some(&tracks[0]),
                    material,
                    1.,
                    visible,
                    playback_visible,
                    cx,
                );
                assert_eq!(this.enabled, expected);
                assert_eq!(this.source().is_some(), expected);
                assert!(this.previous.source().is_none());
                assert!(this.transition_started.is_none());
            }
        });
    }

    #[gpui::test]
    fn visible_atmosphere_keeps_negative_artwork_after_shared_cache_eviction(
        cx: &mut gpui::TestAppContext,
    ) {
        for cached in [false, true] {
            let (directory, manager, tracks) = sources();
            let selected = &tracks[2];
            if cached {
                assert!(manager.load(crate::artwork_source(selected)).is_none());
            }
            let atmosphere =
                cx.new(|_| Atmosphere::new(manager.clone(), ArtworkImageRegistry::default()));
            atmosphere.update(cx, |this, cx| {
                this.sync(Some(selected), MaterialPreset::Glass, 0., true, false, cx);
            });
            cx.run_until_parked();
            for id in 100..170 {
                let mut pressure = selected.clone();
                pressure.id = id;
                pressure.path = directory.path().join(format!("pressure-{id}.wav"));
                assert!(manager.load(crate::artwork_source(&pressure)).is_none());
            }
            assert!(
                manager
                    .cached_result(crate::artwork_source(selected))
                    .is_none()
            );
            rivu_metadata::artwork::test_support::tagged_wav(&selected.path);
            atmosphere.update(cx, |this, cx| {
                this.sync(Some(selected), MaterialPreset::Glass, 0., true, false, cx);
            });
            cx.run_until_parked();
            atmosphere.update(cx, |this, cx| {
                assert!(this.view.source().is_none());
                assert!(
                    manager
                        .cached_result(crate::artwork_source(selected))
                        .is_none()
                );
                this.sync(Some(selected), MaterialPreset::Glass, 0., false, false, cx);
                this.sync(Some(selected), MaterialPreset::Glass, 0., true, false, cx);
            });
            cx.run_until_parked();
            atmosphere.update(cx, |this, _| {
                assert!(this.view.source().is_some());
            });
        }
    }

    #[gpui::test]
    fn transitions_keep_one_predecessor_and_reject_obsolete_loads(cx: &mut gpui::TestAppContext) {
        let (_directory, manager, tracks) = sources();
        let atmosphere = cx.new(|_| Atmosphere::new(manager, ArtworkImageRegistry::default()));
        atmosphere.update(cx, |this, cx| {
            this.sync(
                Some(&tracks[0]),
                MaterialPreset::Dissolve,
                1.,
                true,
                true,
                cx,
            );
            this.sync(
                Some(&tracks[1]),
                MaterialPreset::Dissolve,
                1.,
                true,
                true,
                cx,
            );
            assert!(this.previous.source().is_some());
            assert!(this.view.source().is_some());
            this.sync(
                Some(&tracks[2]),
                MaterialPreset::Dissolve,
                1.,
                true,
                true,
                cx,
            );
            assert!(this.previous.source().is_some());
            assert!(this.view.source().is_none());
            // A second pending switch cannot carry the original source forward.
            this.sync(
                Some(&tracks[3]),
                MaterialPreset::Dissolve,
                1.,
                true,
                true,
                cx,
            );
            assert!(this.previous.source().is_none());
            assert!(this.view.source().is_none());
        });
        cx.run_until_parked();
        atmosphere.update(cx, |this, cx| {
            assert_eq!(this.track.as_ref().unwrap().id, tracks[3].id);
            assert!(this.previous.source().is_none());
            assert!(this.view.source().is_none());
            this.sync(
                Some(&tracks[0]),
                MaterialPreset::Dissolve,
                1.,
                true,
                true,
                cx,
            );
            // Missing/error lookup completion clears a single retained source.
            this.sync(
                Some(&tracks[2]),
                MaterialPreset::Dissolve,
                1.,
                true,
                true,
                cx,
            );
            assert!(this.previous.source().is_none());
            assert!(this.view.source().is_none());
            this.sync(
                Some(&tracks[0]),
                MaterialPreset::Dissolve,
                1.,
                true,
                true,
                cx,
            );
            this.sync(
                Some(&tracks[1]),
                MaterialPreset::Dissolve,
                1.,
                true,
                true,
                cx,
            );
            this.sync(
                Some(&tracks[1]),
                MaterialPreset::Dissolve,
                0.,
                true,
                true,
                cx,
            );
            assert!(this.previous.source().is_none());
            this.sync(
                Some(&tracks[0]),
                MaterialPreset::Dissolve,
                0.,
                true,
                true,
                cx,
            );
            assert!(this.previous.source().is_none());
            this.sync(
                Some(&tracks[1]),
                MaterialPreset::Dissolve,
                1.,
                true,
                true,
                cx,
            );
            this.sync(
                Some(&tracks[1]),
                MaterialPreset::Dissolve,
                1.,
                false,
                true,
                cx,
            );
            assert!(this.previous.source().is_none());
            assert!(this.view.source().is_none());
            this.sync(
                Some(&tracks[0]),
                MaterialPreset::Dissolve,
                1.,
                true,
                true,
                cx,
            );
            this.sync(
                Some(&tracks[1]),
                MaterialPreset::Dissolve,
                1.,
                true,
                true,
                cx,
            );
            this.sync(None, MaterialPreset::Dissolve, 1., true, true, cx);
            assert!(this.previous.source().is_none());
            assert!(this.view.source().is_none());
        });
    }

    #[gpui::test]
    fn repaired_path_retries_failed_lookup_without_a_content_identity_switch(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_directory, manager, tracks) = sources();
        assert!(manager.load(crate::artwork_source(&tracks[2])).is_none());
        let atmosphere = cx.new(|_| Atmosphere::new(manager, ArtworkImageRegistry::default()));
        let mut repaired = tracks[2].clone();
        repaired.path = tracks[0].path.clone();
        atmosphere.update(cx, |this, cx| {
            this.sync(
                Some(&tracks[2]),
                MaterialPreset::Dissolve,
                1.,
                true,
                true,
                cx,
            );
            assert!(this.view.source().is_none());
            let generation = this.generation;
            this.sync(
                Some(&repaired),
                MaterialPreset::Dissolve,
                1.,
                true,
                true,
                cx,
            );
            assert_ne!(this.generation, generation);
            assert_eq!(this.track.as_ref().unwrap().path, repaired.path);
            assert!(this.previous.source().is_none());
        });
        cx.run_until_parked();
        atmosphere.update(cx, |this, _| {
            assert!(this.view.source().is_some());
            assert!(this.previous.source().is_none());
        });
    }

    struct SourceScene {
        atmosphere: Entity<Atmosphere>,
        duration: Duration,
        motion: bool,
        observed: Rc<Cell<(bool, bool, f32)>>,
    }

    impl Render for SourceScene {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let (previous, current, phase) = self.atmosphere.update(cx, |atmosphere, cx| {
                atmosphere.transition_sources(self.duration, self.motion, window, cx)
            });
            self.observed
                .set((previous.is_some(), current.is_some(), phase));
            div()
                .size_full()
                .children(previous.map(gpui::img))
                .children(current.map(gpui::img))
        }
    }

    fn draw_sources(
        window: &mut gpui::VisualTestContext,
        atmosphere: &Entity<Atmosphere>,
        duration: Duration,
        motion: bool,
    ) -> (bool, bool, f32) {
        let observed = Rc::new(Cell::new((false, false, 1.)));
        window.draw(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            gpui::size(gpui::px(32.), gpui::px(32.)),
            |_, cx| {
                cx.new(|_| SourceScene {
                    atmosphere: atmosphere.clone(),
                    duration,
                    motion,
                    observed: observed.clone(),
                })
                .into_any_element()
            },
        );
        observed.get()
    }

    #[gpui::test]
    fn material_switch_retires_previous_while_same_material_duration_keeps_the_clock(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_directory, manager, tracks) = sources();
        let atmosphere = cx.new(|_| Atmosphere::new(manager, ArtworkImageRegistry::default()));
        let window = cx.add_empty_window();
        window.update(|_, cx| {
            atmosphere.update(cx, |this, cx| {
                this.sync(
                    Some(&tracks[0]),
                    MaterialPreset::Dissolve,
                    1.,
                    true,
                    true,
                    cx,
                );
            });
        });
        draw_sources(window, &atmosphere, Duration::from_secs(1), true);
        window.run_until_parked();
        window.update(|_, cx| {
            atmosphere.update(cx, |this, cx| {
                this.sync(
                    Some(&tracks[1]),
                    MaterialPreset::Dissolve,
                    1.,
                    true,
                    true,
                    cx,
                )
            });
        });
        assert_eq!(
            draw_sources(window, &atmosphere, Duration::from_secs(1), true),
            (true, true, 0.)
        );
        window.executor().advance_clock(Duration::from_millis(250));
        window.update(|_, cx| {
            atmosphere.update(cx, |this, cx| {
                let started = this.transition_started;
                this.sync(
                    Some(&tracks[1]),
                    MaterialPreset::Dissolve,
                    2.,
                    true,
                    true,
                    cx,
                );
                assert_eq!(this.transition_started, started);
            });
        });
        assert_eq!(
            draw_sources(window, &atmosphere, Duration::from_secs(2), true),
            (true, true, 0.125)
        );
        window.update(|_, cx| {
            atmosphere.update(cx, |this, cx| {
                let generation = this.generation;
                this.sync(
                    Some(&tracks[1]),
                    MaterialPreset::Storybook,
                    2.,
                    true,
                    true,
                    cx,
                );
                assert!(this.previous.source().is_none());
                assert!(this.transition_started.is_none());
                assert_eq!(this.generation, generation);
                assert_eq!(this.track.as_ref().unwrap().id, tracks[1].id);
            });
        });
        assert_eq!(
            draw_sources(window, &atmosphere, Duration::from_secs(2), true),
            (false, true, 1.)
        );
    }

    #[gpui::test]
    fn target_decode_readiness_not_lookup_or_switch_time_starts_the_fade(
        cx: &mut gpui::TestAppContext,
    ) {
        let (directory, manager, tracks) = sources();
        // A second cache gives the same cover a distinct immutable source path.
        let other_manager = ArtworkManager::new(&directory.path().join("other-cache"));
        let target = other_manager
            .load(crate::artwork_source(&tracks[1]))
            .unwrap();
        let atmosphere = cx.new(|_| Atmosphere::new(manager, ArtworkImageRegistry::default()));
        let window = cx.add_empty_window();
        window.update(|_, cx| {
            atmosphere.update(cx, |this, cx| {
                this.sync(
                    Some(&tracks[0]),
                    MaterialPreset::Dissolve,
                    1.,
                    true,
                    true,
                    cx,
                );
            });
        });
        draw_sources(window, &atmosphere, Duration::from_secs(1), true);
        window.run_until_parked();
        window.update(|_, cx| {
            atmosphere.update(cx, |this, cx| {
                this.sync(
                    Some(&tracks[1]),
                    MaterialPreset::Dissolve,
                    1.,
                    true,
                    true,
                    cx,
                );
                this.apply_target(Some(target), true, cx);
            });
        });
        assert_eq!(
            draw_sources(window, &atmosphere, Duration::from_secs(1), true),
            (true, false, 0.)
        );
        window.run_until_parked();
        window.executor().advance_clock(Duration::from_secs(5));
        assert_eq!(
            draw_sources(window, &atmosphere, Duration::from_secs(1), true),
            (true, true, 0.)
        );
        window.executor().advance_clock(Duration::from_millis(500));
        assert_eq!(
            draw_sources(window, &atmosphere, Duration::from_secs(1), true),
            (true, true, 0.5)
        );
        window.update(|window, cx| {
            atmosphere.update(cx, |this, cx| {
                this.sync(
                    Some(&tracks[1]),
                    MaterialPreset::Dissolve,
                    1.,
                    false,
                    true,
                    cx,
                );
                assert!(this.previous.source().is_none());
                assert!(this.view.source().is_none());
                this.release(window, cx);
                this.sync(
                    Some(&tracks[0]),
                    MaterialPreset::Dissolve,
                    1.,
                    true,
                    true,
                    cx,
                );
                assert!(this.previous.source().is_none());
                assert!(this.view.source().is_some());
            });
        });
    }

    #[gpui::test]
    fn transition_clock_finishes_and_static_paths_release_previous(cx: &mut gpui::TestAppContext) {
        let (_directory, manager, tracks) = sources();
        let atmosphere = cx.new(|_| Atmosphere::new(manager, ArtworkImageRegistry::default()));
        let window = cx.add_empty_window();
        window.update(|_, cx| {
            atmosphere.update(cx, |this, cx| {
                this.sync(
                    Some(&tracks[0]),
                    MaterialPreset::Dissolve,
                    1.,
                    true,
                    true,
                    cx,
                );
            });
        });
        draw_sources(window, &atmosphere, Duration::from_secs(1), true);
        window.run_until_parked();
        window.update(|_, cx| {
            atmosphere.update(cx, |this, cx| {
                this.sync(
                    Some(&tracks[1]),
                    MaterialPreset::Dissolve,
                    1.,
                    true,
                    true,
                    cx,
                )
            });
        });
        assert_eq!(
            draw_sources(window, &atmosphere, Duration::from_secs(1), true),
            (true, true, 0.)
        );
        window.executor().advance_clock(Duration::from_millis(500));
        assert_eq!(
            draw_sources(window, &atmosphere, Duration::from_secs(1), true),
            (true, true, 0.5)
        );
        window.executor().advance_clock(Duration::from_millis(500));
        assert_eq!(
            draw_sources(window, &atmosphere, Duration::from_secs(1), true),
            (false, true, 1.)
        );
        for (duration, motion) in [(Duration::ZERO, true), (Duration::from_secs(1), false)] {
            window.update(|_, cx| {
                atmosphere.update(cx, |this, cx| {
                    this.sync(
                        Some(&tracks[0]),
                        MaterialPreset::Dissolve,
                        1.,
                        true,
                        true,
                        cx,
                    )
                });
            });
            assert_eq!(
                draw_sources(window, &atmosphere, duration, motion),
                (false, true, 1.)
            );
            window.update(|_, cx| {
                atmosphere.update(cx, |this, cx| {
                    this.sync(
                        Some(&tracks[1]),
                        MaterialPreset::Dissolve,
                        1.,
                        true,
                        true,
                        cx,
                    )
                });
            });
        }
        window.update(|window, cx| {
            atmosphere.update(cx, |this, cx| {
                this.release(window, cx);
                assert!(this.previous.source().is_none());
                assert!(this.view.source().is_none());
                assert!(this.transition_started.is_none());
            });
        });
    }

    #[gpui::test]
    fn failed_target_and_reduced_motion_cannot_keep_wrong_art(cx: &mut gpui::TestAppContext) {
        let (directory, manager, tracks) = sources();
        let corrupt_manager = ArtworkManager::new(&directory.path().join("corrupt-cache"));
        let corrupt = corrupt_manager
            .load(crate::artwork_source(&tracks[0]))
            .unwrap();
        std::fs::write(corrupt.path(), b"not an image").unwrap();
        let atmosphere = cx.new(|_| Atmosphere::new(manager, ArtworkImageRegistry::default()));
        let window = cx.add_empty_window();
        window.update(|_, cx| {
            atmosphere.update(cx, |this, cx| {
                this.sync(
                    Some(&tracks[0]),
                    MaterialPreset::Dissolve,
                    1.,
                    true,
                    true,
                    cx,
                );
            });
        });
        draw_sources(window, &atmosphere, Duration::from_secs(1), true);
        window.run_until_parked();
        window.update(|_, cx| {
            atmosphere.update(cx, |this, cx| {
                this.sync(
                    Some(&tracks[1]),
                    MaterialPreset::Dissolve,
                    1.,
                    true,
                    true,
                    cx,
                );
                this.apply_target(Some(corrupt), true, cx);
            });
        });
        assert_eq!(
            draw_sources(window, &atmosphere, Duration::from_secs(1), true),
            (true, false, 0.)
        );
        window.run_until_parked();
        assert_eq!(
            draw_sources(window, &atmosphere, Duration::from_secs(1), true),
            (false, false, 1.)
        );
        window.update(|_, cx| {
            atmosphere.update(cx, |this, cx| {
                assert!(this.previous.source().is_none());
                // The failed lease remains terminal; it is not reacquired each frame.
                assert!(this.view.source().is_some());
                this.sync(
                    Some(&tracks[0]),
                    MaterialPreset::Dissolve,
                    1.,
                    true,
                    true,
                    cx,
                );
                this.sync(
                    Some(&tracks[1]),
                    MaterialPreset::Dissolve,
                    1.,
                    true,
                    true,
                    cx,
                );
            });
            cx.set_reduce_motion(true);
        });
        assert_eq!(
            draw_sources(window, &atmosphere, Duration::from_secs(1), true),
            (false, true, 1.)
        );
    }

    #[gpui::test]
    fn paused_material_intents_reconcile_without_revision_and_mirror_needs_no_panel(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::{
            gui::{
                WindowView,
                appearance::material::{MaterialIntent, MaterialSession},
                layout::persistence::{LayoutPersistence, LoadedLayouts},
            },
            test_support::CoreRuntime,
        };
        use rivu_format::config::Config;
        use rivu_format::config::material::MaterialParameter;
        use rivu_presentation::i18n::Localizer;
        use rivu_protocol::model::PlaybackStatus;
        let (directory, manager, tracks) = sources();
        let config_path = directory.path().join("config.toml");
        Config {
            volume: 0.,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            material: rivu_format::config::MaterialPreferences {
                enabled: true,
                ..Default::default()
            },
            ..Config::default()
        }
        .save_fixture(&config_path)
        .unwrap();
        let runtime = CoreRuntime::start(directory.path(), &config_path).unwrap();
        let save =
            LayoutPersistence::new(LoadedLayouts::load(directory.path().join("layout.json")));
        cx.update(rivu_ui::input::Input::init);
        let window = cx.add_empty_window();
        let app = window.update(|window, cx| {
            cx.new(|cx| {
                WindowView::new(
                    runtime.handle.clone(),
                    save.handle(),
                    manager,
                    std::rc::Weak::new(),
                    Arc::new(Localizer::new("en-US")),
                    window,
                    cx,
                )
            })
        });
        app.update(cx, |app, cx| {
            app.workspace.layouts.current_mut().root = None;
            app.workspace
                .panels
                .reconcile(app.workspace.layouts.current());
            app.window_visible = true;
            let state = Rc::make_mut(&mut app.state);
            state.current_track = Some(Arc::new(tracks[0].clone()));
            state.playback.status = PlaybackStatus::Paused;
            let revisions = state.revisions;
            // An independently demanded feature must not be reconciled by a
            // material action. Only the explicit feature-demand entry owns it.
            app.library
                .update(cx, |library, cx| library.set_visible(true, cx));
            // T -> Q -> T acquires and retires without render or business facts.
            for (material, enabled) in [
                (MaterialPreset::CgWater, false),
                (MaterialPreset::Dissolve, true),
                (MaterialPreset::CgWater, false),
                (MaterialPreset::Mirror, true),
            ] {
                app.material_intent_fenced(MaterialIntent::Select(material), cx);
                assert_eq!(app.state.revisions, revisions);
                assert_eq!(app.atmosphere.read(cx).enabled, enabled);
                assert_eq!(app.atmosphere.read(cx).source().is_some(), enabled);
                assert!(app.library.read(cx).demanded());
            }
            assert!(!app.panel_demand("transport"));
            assert!(!app.panel_demand(crate::gui::shell::panels::PanelKind::Artwork.key()));
            app.reconcile_feature_demand(cx);
            assert!(!app.library.read(cx).demanded());
            app.workspace
                .layouts
                .current_mut()
                .add(crate::gui::shell::panels::PanelKind::Artwork.key(), None);
            app.workspace
                .panels
                .reconcile(app.workspace.layouts.current());
            for material in [MaterialPreset::Atmosphere, MaterialPreset::Stage] {
                app.material_intent_fenced(MaterialIntent::Select(material), cx);
                assert!(!app.panel_demand("transport"));
                assert!(app.panel_demand(crate::gui::shell::panels::PanelKind::Artwork.key()));
                assert!(app.atmosphere.read(cx).enabled);
                assert!(app.atmosphere.read(cx).source().is_some());
            }
            app.material_intent_fenced(MaterialIntent::Select(MaterialPreset::Dissolve), cx);
            app.material_intent_fenced(
                MaterialIntent::Set {
                    material: MaterialPreset::Dissolve,
                    parameter: MaterialParameter::Alpha,
                    value: 0.5,
                },
                cx,
            );
            let adjusted = MaterialSession::current(cx).settings;
            Rc::make_mut(&mut app.state).current_track = Some(Arc::new(tracks[1].clone()));
            app.reconcile_feature_demand(cx);
            assert!(app.atmosphere.read(cx).previous.source().is_some());
            for enabled in [false, true, false, true] {
                app.material_intent_fenced(MaterialIntent::SetEnabled(enabled), cx);
                let session = MaterialSession::current(cx);
                assert_eq!(session.enabled, enabled);
                assert_eq!(session.material, MaterialPreset::Dissolve);
                assert_eq!(session.settings, adjusted);
                assert_eq!(
                    MaterialPreset::current(cx),
                    if enabled {
                        MaterialPreset::Dissolve
                    } else {
                        MaterialPreset::Original
                    }
                );
                assert_eq!(app.state.revisions, revisions);
                assert!(app.panel_demand(crate::gui::shell::panels::PanelKind::Artwork.key()));
                let atmosphere = app.atmosphere.read(cx);
                assert_eq!(atmosphere.enabled, enabled);
                assert_eq!(atmosphere.source().is_some(), enabled);
                assert!(atmosphere.previous.source().is_none());
                assert!(atmosphere.transition_started.is_none());
            }
            // Numeric edits preserve the paused predecessor unless transition policy
            // actually crosses zero; changing a duration must not restart its clock.
            Rc::make_mut(&mut app.state).current_track = Some(Arc::new(tracks[0].clone()));
            app.reconcile_feature_demand(cx);
            let started = cx.background_executor().now();
            app.atmosphere.update(cx, |atmosphere, _| {
                atmosphere.transition_started = Some(started);
            });
            for (parameter, value) in [
                (MaterialParameter::Alpha, 0.6),
                (MaterialParameter::Transition, 1.2),
            ] {
                app.material_intent_fenced(
                    MaterialIntent::Set {
                        material: MaterialPreset::Dissolve,
                        parameter,
                        value,
                    },
                    cx,
                );
                let atmosphere = app.atmosphere.read(cx);
                assert!(atmosphere.previous.source().is_some());
                assert_eq!(atmosphere.transition_started, Some(started));
            }
            for value in [0.75, 0.4, 0., 0.] {
                app.material_intent_fenced(
                    MaterialIntent::Set {
                        material: MaterialPreset::Dissolve,
                        parameter: MaterialParameter::Transition,
                        value,
                    },
                    cx,
                );
                let enabled = MaterialSession::current(cx).settings.transition_seconds > 0.;
                let atmosphere = app.atmosphere.read(cx);
                assert_eq!(atmosphere.transition_enabled, enabled);
                assert_eq!(atmosphere.previous.source().is_some(), enabled);
                assert_eq!(atmosphere.transition_started, enabled.then_some(started));
            }
            assert_eq!(MaterialSession::current(cx).settings.transition_seconds, 0.);
            for value in [0.1, 0.] {
                app.material_intent_fenced(
                    MaterialIntent::Set {
                        material: MaterialPreset::Dissolve,
                        parameter: MaterialParameter::Transition,
                        value,
                    },
                    cx,
                );
                assert_eq!(app.atmosphere.read(cx).transition_enabled, value > 0.);
                assert!(app.atmosphere.read(cx).previous.source().is_none());
            }
            // Reset must restore predecessor policy before the next track sync,
            // without relying on render or a playback revision to reconcile it.
            app.material_intent_fenced(MaterialIntent::Reset(MaterialPreset::Dissolve), cx);
            assert!(app.atmosphere.read(cx).transition_enabled);
            Rc::make_mut(&mut app.state).current_track = Some(Arc::new(tracks[1].clone()));
            app.reconcile_feature_demand(cx);
            assert!(app.atmosphere.read(cx).previous.source().is_some());
            assert_eq!(app.state.revisions, revisions);
            app.material_intent_fenced(MaterialIntent::SetEnabled(false), cx);
            let disabled = MaterialSession::current(cx);
            for intent in [
                MaterialIntent::Select(MaterialPreset::Mirror),
                MaterialIntent::Reset(MaterialPreset::Dissolve),
                MaterialIntent::Set {
                    material: MaterialPreset::Dissolve,
                    parameter: MaterialParameter::Alpha,
                    value: 0.2,
                },
            ] {
                app.material_intent_fenced(intent, cx);
                let session = MaterialSession::current(cx);
                assert!(!session.enabled);
                assert_eq!(session.material, disabled.material);
                assert_eq!(session.settings, disabled.settings);
            }
            assert!(app.atmosphere.read(cx).source().is_none());
            app.material_intent_fenced(MaterialIntent::SetEnabled(true), cx);
            app.material_intent_fenced(MaterialIntent::Select(MaterialPreset::Mirror), cx);
            assert_eq!(
                MaterialSession::current(cx).material,
                MaterialPreset::Mirror
            );
            assert!(app.atmosphere.read(cx).source().is_some());
            app.workspace.layouts.current_mut().root = None;
            for material in MaterialPreset::ALL
                .into_iter()
                .filter(|material| material.is_texture())
            {
                app.material_intent_fenced(MaterialIntent::Select(material), cx);
                let baseline = MaterialSession::current(cx).settings;
                app.material_intent_fenced(
                    MaterialIntent::Set {
                        material,
                        parameter: MaterialParameter::Alpha,
                        value: 0.2,
                    },
                    cx,
                );
                let edited = MaterialSession::current(cx).settings;
                assert_ne!(edited, baseline);
                app.toggle_material_controls(cx);
                app.close_material_controls(cx);
                app.toggle_material_controls(cx);
                assert_eq!(MaterialSession::current(cx).settings, edited);
                app.material_intent_fenced(MaterialIntent::Reset(material), cx);
                assert_eq!(MaterialSession::current(cx).settings, baseline);
                app.material_intent_fenced(
                    MaterialIntent::Set {
                        material,
                        parameter: MaterialParameter::Alpha,
                        value: 0.2,
                    },
                    cx,
                );
                app.material_intent_fenced(MaterialIntent::Select(material), cx);
                assert_eq!(MaterialSession::current(cx).settings, edited);
                app.material_intent_fenced(MaterialIntent::Reset(material), cx);
                assert_eq!(MaterialSession::current(cx).settings, baseline);
            }
            app.material_intent_fenced(MaterialIntent::Select(MaterialPreset::StillWater), cx);
            let fresh = MaterialSession::current(cx).settings;
            app.material_intent_fenced(
                MaterialIntent::Set {
                    material: MaterialPreset::Dissolve,
                    parameter: MaterialParameter::Alpha,
                    value: 0.6,
                },
                cx,
            );
            app.material_intent_fenced(MaterialIntent::Reset(MaterialPreset::Dissolve), cx);
            assert_eq!(MaterialSession::current(cx).settings, fresh);
            assert_eq!(MaterialPreset::current(cx), MaterialPreset::StillWater);
        });
    }
}
