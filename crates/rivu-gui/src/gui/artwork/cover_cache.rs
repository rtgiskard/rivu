//! Per-window admission and retirement of GPUI's atlas-backed cover variants.
use gpui::{ImageBlurEdge, ImagePreblur, Pixels, RenderImage, Size, Window};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::Arc,
};

const MAX_ENTRIES: usize = 8;
const MAX_BYTES: usize = 48 * 1024 * 1024;
const MAX_VARIANT_BYTES: usize = 16 * 1024 * 1024;

fn descriptor(
    source: &RenderImage,
    fitted: Size<Pixels>,
    radius: f32,
    reflect: bool,
) -> Option<ImagePreblur> {
    let dimensions = source.size(0);
    let texels = radius * dimensions.width.0 as f32 / f32::from(fitted.width);
    if !texels.is_finite() || texels <= 0. || fitted.height <= Pixels::ZERO {
        return None;
    }
    // Keep six fractional significand bits. Neighbouring resize steps share a
    // filter, with at most 0.8% relative error even for sub-texel radii.
    let radius_bits = (texels.to_bits() + (1 << 16)) & !((1 << 17) - 1);
    let radius = f32::from_bits(radius_bits);
    if !radius.is_finite() || radius == 0. || dimensions.height.0 == 0 {
        return None;
    }
    let edge = if reflect {
        ImageBlurEdge::Reflect
    } else {
        ImageBlurEdge::Clamp
    };
    // When the cover was uploaded only for this variant, GPUI can reuse
    // its tile in place. A source already in use elsewhere stays cached.
    Some(ImagePreblur::new(radius, edge).release_source())
}

struct Variant {
    source: Arc<RenderImage>,
    preblur: ImagePreblur,
}

#[derive(Default)]
struct State {
    generation: u64,
    // Each measured surface moves its variant into this frame's prefix.
    variants: Vec<Variant>,
    wanted: usize,
    wanted_bytes: usize,
}

impl State {
    fn begin(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.wanted = 0;
        self.wanted_bytes = 0;
    }

    fn admit(&mut self, source: &Arc<RenderImage>, preblur: ImagePreblur) -> Option<ImagePreblur> {
        let existing = self
            .variants
            .iter()
            .position(|variant| variant.source.id == source.id && variant.preblur == preblur);
        if let Some(index) = existing
            && index < self.wanted
        {
            return Some(preblur);
        }
        let bytes = source.as_bytes(0)?.len();
        if bytes == 0
            || bytes > MAX_VARIANT_BYTES
            || self.wanted == MAX_ENTRIES
            || self.wanted_bytes + bytes > MAX_BYTES
        {
            return None;
        }
        let index = existing.unwrap_or_else(|| {
            self.variants.push(Variant {
                source: source.clone(),
                preblur,
            });
            self.variants.len() - 1
        });
        let next = self.wanted;
        if index != next {
            self.variants.swap(index, next);
        }
        self.wanted += 1;
        self.wanted_bytes += bytes;
        Some(preblur)
    }

    fn retire_unused(&mut self, mut retire: impl FnMut(Arc<RenderImage>)) {
        while self.variants.len() > self.wanted {
            let removed = self.variants.pop().unwrap();
            // GPUI removes every radius/edge for this source in one call.
            // Surviving variants will be derived again in the ensuing paint.
            if !self.variants[self.wanted..]
                .iter()
                .any(|other| other.source.id == removed.source.id)
            {
                retire(removed.source);
            }
        }
    }

    fn trim(&mut self, window: &mut Window) {
        self.retire_unused(|source| {
            let _ = window.drop_image(source);
        });
    }
}

#[derive(Default)]
pub(in crate::gui) struct CoverBlurCache(Rc<RefCell<State>>);

impl CoverBlurCache {
    pub(in crate::gui) fn begin(&self, window: &mut Window) -> CoverBlurFrame {
        let mut state = self.0.borrow_mut();
        state.trim(window);
        state.begin();
        CoverBlurFrame {
            cache: Rc::downgrade(&self.0),
            generation: state.generation,
        }
    }

    pub(in crate::gui) fn clear(&self, window: &mut Window) {
        let mut state = self.0.borrow_mut();
        state.begin();
        state.trim(window);
    }
}

/// Only the latest sampled scene may change the window's atlas demand.
#[derive(Clone)]
pub(in crate::gui) struct CoverBlurFrame {
    cache: Weak<RefCell<State>>,
    generation: u64,
}

impl CoverBlurFrame {
    pub(in crate::gui) fn lookup(
        &self,
        source: &Arc<RenderImage>,
        fitted: Size<Pixels>,
        radius: f32,
        reflect: bool,
    ) -> Option<ImagePreblur> {
        let cache = self.cache.upgrade()?;
        let mut state = cache.borrow_mut();
        if state.generation != self.generation {
            return None;
        }
        state.admit(source, descriptor(source, fitted, radius, reflect)?)
    }

    /// The root paint runs after all surfaces submitted their measured demand
    /// and before the material sprites look up their tiles in the atlas.
    pub(in crate::gui) fn finish_prepaint(&self, window: &mut Window) {
        let Some(cache) = self.cache.upgrade() else {
            return;
        };
        let mut state = cache.borrow_mut();
        if state.generation == self.generation {
            state.trim(window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, ObjectFit, point, px, size};
    use image::{Frame, Rgba, RgbaImage};

    fn image(width: u32, height: u32) -> Arc<RenderImage> {
        Arc::new(RenderImage::new([Frame::new(RgbaImage::from_pixel(
            width,
            height,
            Rgba([12, 35, 61, 198]),
        ))]))
    }

    fn fitted(image: &RenderImage, width: f32, height: f32) -> Size<Pixels> {
        ObjectFit::Cover
            .get_bounds(
                Bounds::new(point(px(0.), px(0.)), size(px(width), px(height))),
                image.size(0),
            )
            .size
    }

    #[test]
    fn cover_scale_shares_nearby_fits_and_distinguishes_panels_and_wave_edges() {
        let source = image(120, 60);
        let blur = |width, height, reflect| {
            descriptor(&source, fitted(&source, width, height), 8., reflect).unwrap()
        };
        let root = blur(240., 90., false);
        assert_eq!(root.radius_texels, 4.);
        assert_eq!(root.edge, ImageBlurEdge::Clamp);
        assert_eq!(blur(240., 120., false), root);
        assert_eq!(blur(240.1, 100., false), root);
        assert_eq!(blur(90., 240., false).radius_texels, 2.);
        assert_eq!(blur(240., 90., true).edge, ImageBlurEdge::Reflect);
    }

    #[test]
    fn layout_and_transition_changes_retire_each_obsolete_source_once() {
        let previous = image(20, 12);
        let current = image(20, 12);
        let unrelated = image(20, 12);
        let root = ImagePreblur::new(8., ImageBlurEdge::Clamp);
        let panel = ImagePreblur::new(5., ImageBlurEdge::Clamp);
        let mut state = State::default();
        state.begin();
        for source in [&previous, &current, &unrelated] {
            assert!(state.admit(source, root).is_some());
            assert!(state.admit(source, panel).is_some());
        }
        state.begin();
        // The transition ended. The current cover lost one fitted size,
        // while the unrelated cover's two variants are both still in use.
        for (source, blur) in [(&unrelated, panel), (&current, root), (&unrelated, root)] {
            assert!(state.admit(source, blur).is_some());
        }
        let mut retired = Vec::new();
        state.retire_unused(|source| retired.push(source.id));
        assert_eq!(retired.len(), 2);
        assert!(retired.contains(&previous.id));
        assert!(retired.contains(&current.id));
        state.begin();
        for (source, blur) in [(&current, root), (&unrelated, root), (&unrelated, panel)] {
            assert!(state.admit(source, blur).is_some());
        }
        state.retire_unused(|_| panic!("stable demand invalidated a source"));
    }

    #[gpui::test]
    fn measured_root_and_panel_reuse_tiles_and_only_their_window_invalidates_them(
        cx: &mut gpui::TestAppContext,
    ) {
        let cache = CoverBlurCache::default();
        let mut window = cx.add_empty_window().clone();
        let other_window = cx.add_empty_window();
        let frame = window.update(|window, _| cache.begin(window));
        let source = image(31, 19);
        let destination = fitted(&source, 100., 90.);
        // Let the actual root and panel submit their laid-out bounds. The tall
        // panel has a different cover scale from the viewport.
        use crate::gui::appearance::texture_styles::{self, ArtworkBlend};
        use gpui::{InteractiveElement as _, ParentElement as _, Styled as _, div};
        use rivu_format::config::{MaterialParameters, MaterialPreset};
        let material = MaterialPreset::Aura;
        let mut settings = MaterialParameters::preset(material);
        settings.cover_blur = 7.;
        settings.wave = 3.;
        let theme = rivu_presentation::theme::palette(rivu_presentation::theme::Theme::RivuDark);
        let blend = || ArtworkBlend {
            previous: None,
            current: Some(source.clone()),
            weight: 1.,
            cover_blur: Some(frame.clone()),
        };
        window.draw(point(px(0.), px(0.)), size(px(100.), px(90.)), |_, _| {
            div()
                .relative()
                .size_full()
                .child(texture_styles::backdrop(
                    material,
                    settings,
                    theme,
                    blend(),
                    false,
                ))
                .child(texture_styles::panel(
                    material,
                    settings,
                    div().id("blur-test-panel").absolute().w(px(30.)).h(px(60.)),
                    "transport",
                    theme,
                    blend(),
                    false,
                ))
        });
        let panel = fitted(&source, 30., 60.);
        let state = cache.0.borrow();
        assert_eq!(state.variants.len(), 2);
        for bounds in [destination, panel] {
            let expected = descriptor(&source, bounds, 7., true).unwrap();
            assert!(state.variants.iter().any(|v| v.preblur == expected));
        }
        drop(state);
        // The headless preblur path caches only derived tiles. Painting an
        // ordinary copy afterward exposes source-key eviction to the test.
        window.update(|window, _| {
            frame.finish_prepaint(window);
            assert!(!window.has_image_atlas_entry(&source));
        });
        for target in [&mut window, &mut *other_window] {
            target.draw(point(px(0.), px(0.)), size(px(40.), px(30.)), |_, _| {
                gpui::img(source.clone()).size_full()
            });
            target.update(|window, _| assert!(window.has_image_atlas_entry(&source)));
        }
        window.update(|window, _| {
            let stable = cache.begin(window);
            assert!(frame.lookup(&source, destination, 7., true).is_none());
            for bounds in [panel, destination] {
                assert!(stable.lookup(&source, bounds, 7., true).is_some());
            }
            stable.finish_prepaint(window);
            assert!(window.has_image_atlas_entry(&source));
            let changed = cache.begin(window);
            assert!(changed.lookup(&source, destination, 7., true).is_some());
            stable.finish_prepaint(window);
            assert!(window.has_image_atlas_entry(&source));
            changed.finish_prepaint(window);
            assert!(!window.has_image_atlas_entry(&source));
            cache.clear(window);
            assert!(changed.lookup(&source, destination, 7., true).is_none());
            assert!(cache.0.borrow().variants.is_empty());
        });
        other_window.update(|window, _| assert!(window.has_image_atlas_entry(&source)));
    }

    #[test]
    fn repeated_variants_share_admission_within_entry_and_byte_limits() {
        for (width, height, limit) in [(24, 24, 8), (2048, 2048, 3), (2049, 2048, 0)] {
            let source = image(width, height);
            let mut state = State::default();
            state.begin();
            for i in 0..=limit {
                let blur = ImagePreblur::new(1. + i as f32, ImageBlurEdge::Clamp);
                assert_eq!(state.admit(&source, blur).is_some(), i < limit);
                assert_eq!(state.admit(&source, blur).is_some(), i < limit);
            }
        }
    }
}
