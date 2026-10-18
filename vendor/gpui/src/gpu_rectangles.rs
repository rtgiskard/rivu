//! Experimental closed-schema rectangle rendering, removable when the generic
//! parameterized rendering schema is extracted. This is not a stable program API.

#[cfg(not(target_os = "macos"))]
use crate::Primitive;
use crate::{Bounds, ContentMask, DrawOrder, Pixels, ScaledPixels};
use anyhow::{Result, ensure};
use parking_lot::RwLock;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

static NEXT_FRAME_ID: AtomicU64 = AtomicU64::new(1);

/// One procedural rectangle in normalized drawing-placement coordinates.
#[derive(Clone, Copy, Debug)]
pub struct GpuRectangle {
    /// Normalized x, y, width and height, contained in the unit square.
    pub bounds: [f32; 4],
    /// Palette coordinates at the bottom and top, respectively, in `0..=1`.
    pub palette_range: [f32; 2],
}

/// Content parameters shared by every rectangle in a publication.
#[derive(Clone, Copy, Debug, Default)]
pub struct GpuRectangleParams {
    /// Filled segment height in logical pixels; zero selects solid rectangles.
    pub cell_height: f32,
    /// Empty space between bottom-aligned segments, in logical pixels.
    /// Must be zero for solid rectangles.
    pub cell_gap: f32,
}

/// Explicit, stable CPU content capacities for an experimental rectangle state.
#[derive(Clone, Copy, Debug)]
pub struct GpuRectangleLimits {
    /// Maximum rectangle count; zero permits only empty drawings.
    pub max_rectangles: usize,
    /// Maximum palette sample count; must be at least one.
    pub max_palette_colors: usize,
}

impl GpuRectangleLimits {
    fn validate_counts(self, rectangles: usize, palette: usize) -> Result<()> {
        ensure!(
            self.max_palette_colors > 0,
            "GPU rectangle palette capacity must be positive"
        );
        ensure!(
            rectangles <= self.max_rectangles,
            "GPU rectangle capacity exceeded"
        );
        ensure!(palette > 0, "GPU rectangle palette must not be empty");
        ensure!(
            palette <= self.max_palette_colors,
            "GPU rectangle palette capacity exceeded"
        );
        Ok(())
    }
}

/// One validated immutable publication. No GPU resources or window are retained.
#[derive(Debug)]
pub struct GpuRectangleFrame {
    id: u64,
    rectangles: Box<[GpuRectangle]>,
    palette: Box<[[f32; 4]]>,
    params: GpuRectangleParams,
}

impl GpuRectangleFrame {
    /// Validate the complete publication before making any owned content copies.
    /// Palette samples are straight-alpha sRGB values, linearly interpolated in
    /// sRGB numerical space by the renderer.
    pub fn new(
        rectangles: &[GpuRectangle],
        palette: &[[f32; 4]],
        params: GpuRectangleParams,
        limits: GpuRectangleLimits,
    ) -> Result<Arc<Self>> {
        limits.validate_counts(rectangles.len(), palette.len())?;
        ensure!(
            params.cell_height.is_finite()
                && params.cell_height >= 0.0
                && params.cell_gap.is_finite()
                && params.cell_gap >= 0.0
                && (params.cell_height + params.cell_gap).is_finite()
                && (params.cell_height != 0.0 || params.cell_gap == 0.0),
            "invalid GPU rectangle segmentation parameters"
        );
        for rectangle in rectangles {
            let [x, y, width, height] = rectangle.bounds;
            ensure!(
                rectangle
                    .bounds
                    .iter()
                    .all(|value| value.is_finite() && *value >= 0.0)
                    && x + width <= 1.0
                    && y + height <= 1.0,
                "GPU rectangle bounds must be contained in the unit square"
            );
            ensure!(
                rectangle
                    .palette_range
                    .iter()
                    .all(|value| value.is_finite() && (0.0..=1.0).contains(value)),
                "GPU rectangle palette coordinates must be finite and in 0..=1"
            );
        }
        ensure!(
            palette
                .iter()
                .flatten()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value)),
            "GPU rectangle palette channels must be finite and in 0..=1"
        );
        let mut owned_rectangles = Vec::new();
        owned_rectangles.try_reserve_exact(rectangles.len())?;
        owned_rectangles.extend_from_slice(rectangles);
        let mut owned_palette = Vec::new();
        owned_palette.try_reserve_exact(palette.len())?;
        owned_palette.extend_from_slice(palette);
        let id = NEXT_FRAME_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| anyhow::anyhow!("GPU rectangle frame identity space exhausted"))?;
        Ok(Arc::new(Self {
            id,
            rectangles: owned_rectangles.into_boxed_slice(),
            palette: owned_palette.into_boxed_slice(),
            params,
        }))
    }

    /// Process-unique immutable identity, never recycled even after frame release.
    /// Renderer caches still use weak ownership to determine content lifetime.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Immutable normalized instances.
    pub fn rectangles(&self) -> &[GpuRectangle] {
        &self.rectangles
    }

    /// Immutable straight-alpha sRGB palette samples.
    pub fn palette(&self) -> &[[f32; 4]] {
        &self.palette
    }

    /// Parameters belonging to this same publication.
    pub fn params(&self) -> GpuRectangleParams {
        self.params
    }
}

/// Latest-wins dynamic content, with fixed capacities and no frame scheduling.
/// Cached scenes retain this slot, never a snapshot of its content.
///
/// A single synchronous owner must serialize `publish` and `clear` calls.
/// Snapshots may be captured concurrently. Clearing is not cancellation of a
/// concurrently preparing publication: serialize mutations before hiding a view
/// so that only an explicit subsequent `publish` can restore its content.
#[derive(Debug)]
pub struct GpuRectangleState {
    current: RwLock<Option<Arc<GpuRectangleFrame>>>,
    limits: GpuRectangleLimits,
}

impl GpuRectangleState {
    /// Attach an already validated immutable frame to a bounded state slot.
    pub fn new(initial: Arc<GpuRectangleFrame>, limits: GpuRectangleLimits) -> Result<Arc<Self>> {
        limits.validate_counts(initial.rectangles.len(), initial.palette.len())?;
        Ok(Arc::new(Self {
            current: RwLock::new(Some(initial)),
            limits,
        }))
    }

    /// Prepare and atomically publish all content. Failure leaves the previous
    /// publication unchanged. A successful publish also restores a cleared slot.
    /// The caller remains responsible for requesting a frame through GPUI.
    /// Mutation must be serialized with other `publish` and `clear` calls.
    pub fn publish(
        &self,
        rectangles: &[GpuRectangle],
        palette: &[[f32; 4]],
        params: GpuRectangleParams,
    ) -> Result<()> {
        let frame = GpuRectangleFrame::new(rectangles, palette, params, self.limits)?;
        let previous = self.current.write().replace(frame);
        drop(previous);
        Ok(())
    }

    /// Capture one coherent publication, or no content after [`Self::clear`].
    /// Renderers resolve each distinct state only once per prepared frame.
    pub fn snapshot(&self) -> Option<Arc<GpuRectangleFrame>> {
        self.current.read().clone()
    }

    /// Revoke the slot's content even while cached scenes retain the slot.
    /// Already captured frames stay valid until their consumers release them.
    /// This does not request a frame or revoke independently recorded fixed draws.
    /// The owner must serialize this with publication; see [`GpuRectangleState`].
    pub fn clear(&self) {
        let previous = self.current.write().take();
        drop(previous);
    }
}

/// Explicit replay policy for a rectangle drawing.
#[derive(Clone, Debug)]
pub enum GpuRectangleSource {
    /// Replay the exact immutable publication recorded at paint time.
    Fixed(Arc<GpuRectangleFrame>),
    /// Resolve the latest complete publication once per rendered frame.
    Dynamic(Arc<GpuRectangleState>),
}

/// Content-independent placement captured by the scene.
#[derive(Clone, Debug)]
pub struct GpuRectangleBatch {
    /// Scene-computed ordering key.
    pub order: DrawOrder,
    /// Physical-pixel placement of the normalized rectangle data.
    pub bounds: Bounds<ScaledPixels>,
    /// Inherited physical-pixel clipping mask.
    pub content_mask: ContentMask<ScaledPixels>,
    /// Inherited element opacity, independent of shared content.
    pub opacity: f32,
    /// Logical-to-physical scale for segmentation parameters.
    pub scale_factor: f32,
    /// Fixed or dynamically resolved content.
    pub source: GpuRectangleSource,
}

impl GpuRectangleBatch {
    pub(crate) fn from_logical(
        bounds: Bounds<Pixels>,
        content_mask: ContentMask<Pixels>,
        opacity: f32,
        scale_factor: f32,
        source: GpuRectangleSource,
    ) -> Result<Self> {
        ensure!(
            !cfg!(any(target_os = "macos", target_os = "ios")),
            "GPU rectangles require a WGPU backend"
        );
        ensure!(
            scale_factor.is_finite() && scale_factor > 0.0,
            "invalid GPU rectangle scale factor"
        );
        ensure!(
            opacity.is_finite() && (0.0..=1.0).contains(&opacity),
            "invalid GPU rectangle opacity"
        );
        ensure!(
            bounds.size.width.0 >= 0.0 && bounds.size.height.0 >= 0.0,
            "GPU rectangle placement dimensions must be nonnegative"
        );
        let bounds = bounds.scale(scale_factor);
        let content_mask = content_mask.scale(scale_factor);
        for (candidate, label) in [(bounds, "placement"), (content_mask.bounds, "clip")] {
            ensure!(
                candidate.origin.x.0.is_finite()
                    && candidate.origin.y.0.is_finite()
                    && candidate.size.width.0.is_finite()
                    && candidate.size.height.0.is_finite()
                    && candidate.size.width.0 >= 0.0
                    && candidate.size.height.0 >= 0.0
                    && candidate.right().0.is_finite()
                    && candidate.bottom().0.is_finite(),
                "GPU rectangle {label} must have finite, nonnegative dimensions"
            );
        }
        Ok(Self {
            order: 0,
            bounds,
            content_mask,
            opacity,
            scale_factor,
            source,
        })
    }
}

#[cfg(not(target_os = "macos"))]
impl From<GpuRectangleBatch> for Primitive {
    fn from(batch: GpuRectangleBatch) -> Self {
        Self::GpuRectangles(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: GpuRectangleLimits = GpuRectangleLimits {
        max_rectangles: 2,
        max_palette_colors: 2,
    };
    const RECTANGLE: GpuRectangle = GpuRectangle {
        bounds: [0.0, 0.0, 1.0, 1.0],
        palette_range: [0.0, 1.0],
    };
    const PALETTE: [[f32; 4]; 1] = [[0.0, 0.5, 1.0, 1.0]];

    fn frame() -> Arc<GpuRectangleFrame> {
        GpuRectangleFrame::new(
            &[RECTANGLE],
            &PALETTE,
            GpuRectangleParams::default(),
            LIMITS,
        )
        .unwrap()
    }

    #[test]
    fn immutable_identity_survives_clone_and_is_never_recycled() {
        let first = frame();
        let first_id = first.id();
        let clone = first.clone();
        assert_eq!(clone.id(), first_id);
        drop(first);
        drop(clone);
        let next = frame();
        assert!(next.id() > first_id);
        assert!(frame().id() > next.id());
    }

    fn placement(source: GpuRectangleSource) -> Result<GpuRectangleBatch> {
        let bounds = Bounds {
            origin: crate::point(Pixels(3.0), Pixels(4.0)),
            size: crate::size(Pixels(20.0), Pixels(30.0)),
        };
        let mask = ContentMask {
            bounds: Bounds {
                origin: bounds.origin,
                size: crate::size(Pixels(10.0), Pixels(15.0)),
            },
        };
        GpuRectangleBatch::from_logical(bounds, mask, 0.375, 2.0, source)
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn native_backend_rejects_rectangle_placements() {
        assert!(placement(GpuRectangleSource::Fixed(frame())).is_err());
    }

    #[test]
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    fn placement_captures_scaled_bounds_mask_and_opacity() {
        let fixed = frame();
        let batch = placement(GpuRectangleSource::Fixed(fixed.clone())).unwrap();
        assert_eq!(
            batch.bounds.origin,
            crate::point(ScaledPixels(6.0), ScaledPixels(8.0))
        );
        assert_eq!(
            batch.bounds.size,
            crate::size(ScaledPixels(40.0), ScaledPixels(60.0))
        );
        assert_eq!(
            batch.content_mask.bounds.size,
            crate::size(ScaledPixels(20.0), ScaledPixels(30.0))
        );
        assert_eq!(batch.opacity, 0.375);
        assert_eq!(batch.scale_factor, 2.0);
        assert!(
            matches!(&batch.source, GpuRectangleSource::Fixed(frame) if Arc::ptr_eq(frame, &fixed))
        );
        let bounds = Bounds {
            origin: crate::point(Pixels(0.0), Pixels(0.0)),
            size: crate::size(Pixels(1.0), Pixels(1.0)),
        };
        for width in [-1.0, f32::NAN, f32::INFINITY, f32::MAX] {
            let invalid = Bounds {
                size: crate::size(Pixels(width), Pixels(1.0)),
                ..bounds
            };
            assert!(
                GpuRectangleBatch::from_logical(
                    invalid,
                    ContentMask { bounds },
                    1.0,
                    2.0,
                    batch.source.clone()
                )
                .is_err()
            );
        }
        let invalid = Bounds {
            origin: crate::point(Pixels(f32::NAN), Pixels(0.0)),
            ..bounds
        };
        assert!(
            GpuRectangleBatch::from_logical(
                invalid,
                ContentMask { bounds },
                1.0,
                1.0,
                batch.source.clone()
            )
            .is_err()
        );
    }

    #[test]
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    fn recording_rejects_invalid_scaled_clip_bounds() {
        let bounds = Bounds::new(
            crate::point(Pixels(0.0), Pixels(0.0)),
            crate::size(Pixels(8.0), Pixels(8.0)),
        );
        let source = GpuRectangleSource::Fixed(frame());
        for field in 0..4 {
            for invalid in [f32::NAN, f32::INFINITY, f32::MAX] {
                let mut values = [0.0, 0.0, 8.0, 8.0];
                values[field] = invalid;
                let mask = ContentMask {
                    bounds: Bounds::new(
                        crate::point(Pixels(values[0]), Pixels(values[1])),
                        crate::size(Pixels(values[2]), Pixels(values[3])),
                    ),
                };
                assert!(
                    GpuRectangleBatch::from_logical(bounds, mask, 1.0, 2.0, source.clone())
                        .is_err()
                );
            }
        }
        for mask_size in [
            crate::size(Pixels(-1.0), Pixels(8.0)),
            crate::size(Pixels(8.0), Pixels(-1.0)),
        ] {
            let mask = ContentMask {
                bounds: Bounds::new(bounds.origin, mask_size),
            };
            assert!(
                GpuRectangleBatch::from_logical(bounds, mask, 1.0, 1.0, source.clone()).is_err()
            );
        }
    }

    #[test]
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    fn replay_keeps_fixed_content_but_resolves_dynamic_publication() {
        use crate::{PrimitiveBatch, Scene};
        let initial = frame();
        let initial_weak = Arc::downgrade(&initial);
        let state = GpuRectangleState::new(initial.clone(), LIMITS).unwrap();
        let fixed = placement(GpuRectangleSource::Fixed(initial)).unwrap();
        let dynamic = placement(GpuRectangleSource::Dynamic(state.clone())).unwrap();
        let bounds = fixed.bounds;
        let mut source = Scene::default();
        source.push_layer(bounds);
        source.push_layer(bounds);
        source.insert_primitive(fixed);
        source.pop_layer();
        source.insert_primitive(dynamic);
        source.pop_layer();
        source.finish();
        // Sorting reverses these nested-layer records, not their paint replay ranges.
        assert!(matches!(
            source.gpu_rectangles[0].source,
            GpuRectangleSource::Dynamic(_)
        ));
        assert!(matches!(
            source.gpu_rectangles[1].source,
            GpuRectangleSource::Fixed(_)
        ));
        state
            .publish(
                &[RECTANGLE; 2],
                &[[1.0; 4]],
                GpuRectangleParams {
                    cell_height: 3.0,
                    cell_gap: 1.0,
                },
            )
            .unwrap();
        let published = state.snapshot().unwrap();
        let published_weak = Arc::downgrade(&published);
        let mut replayed = Scene::default();
        replayed.replay(0..source.len(), &source);
        replayed.finish();
        let batches: Vec<_> = replayed.batches().collect();
        assert!(matches!(&batches[..], [PrimitiveBatch::GpuRectangles(range)] if *range == (0..2)));
        assert_eq!(batches[0].label(), "GPU rectangles (2)");
        assert_eq!(replayed.gpu_rectangles[0].opacity, 0.375);
        assert_eq!(replayed.gpu_rectangles[0].bounds, bounds);
        assert_eq!(replayed.gpu_rectangles[0].scale_factor, 2.0);
        let GpuRectangleSource::Dynamic(slot) = &replayed.gpu_rectangles[0].source else {
            panic!("expected dynamic source")
        };
        assert!(Arc::ptr_eq(&slot.snapshot().unwrap(), &published));
        let GpuRectangleSource::Fixed(recorded) = &replayed.gpu_rectangles[1].source else {
            panic!("expected fixed source")
        };
        assert_eq!(recorded.rectangles().len(), 1);
        assert_eq!(recorded.params().cell_height, 0.0);
        let mut partial = Scene::default();
        partial.replay(2..3, &source);
        assert!(matches!(
            partial.gpu_rectangles[0].source,
            GpuRectangleSource::Fixed(_)
        ));
        source.clear();
        state.clear();
        drop(published);
        assert!(published_weak.upgrade().is_none());
        // Cached dynamic draws hold the slot only, so revocation releases its frame.
        assert!(slot.snapshot().is_none());
        assert!(initial_weak.upgrade().is_some());
        replayed.clear();
        assert!(initial_weak.upgrade().is_some());
        partial.clear();
        assert!(initial_weak.upgrade().is_none());
        assert!(replayed.gpu_rectangles.is_empty());
        assert_eq!(replayed.batches().count(), 0);
    }

    #[test]
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    fn rectangle_batches_preserve_overlap_order_and_clipping() {
        use crate::{PrimitiveBatch, Quad, Scene};
        let batch = placement(GpuRectangleSource::Fixed(frame())).unwrap();
        let mut scene = Scene::default();
        scene.insert_primitive(batch.clone());
        scene.insert_primitive(Quad {
            bounds: batch.bounds,
            content_mask: batch.content_mask,
            ..Quad::default()
        });
        scene.insert_primitive(batch.clone());
        let mut clipped = batch;
        clipped.content_mask.bounds.origin.x = ScaledPixels(1000.0);
        scene.insert_primitive(clipped);
        scene.finish();
        assert_eq!(scene.len(), 3);
        let batches: Vec<_> = scene.batches().collect();
        assert!(
            matches!(&batches[..], [PrimitiveBatch::GpuRectangles(a), PrimitiveBatch::Quads(b), PrimitiveBatch::GpuRectangles(c)] if *a == (0..1) && *b == (0..1) && *c == (1..2))
        );
    }

    #[test]
    fn concurrent_snapshots_never_mix_publication_fields() {
        let state = GpuRectangleState::new(frame(), LIMITS).unwrap();
        std::thread::scope(|scope| {
            let writer = &state;
            scope.spawn(move || {
                for revision in 0..2000 {
                    let count = revision % 2 + 1;
                    writer
                        .publish(
                            &[RECTANGLE; 2][..count],
                            &[[count as f32 / 2.0; 4]],
                            GpuRectangleParams {
                                cell_height: count as f32,
                                cell_gap: 0.0,
                            },
                        )
                        .unwrap();
                }
            });
            for _ in 0..2000 {
                let snapshot = state.snapshot().unwrap();
                if snapshot.params().cell_height != 0.0 {
                    assert_eq!(
                        snapshot.rectangles().len() as f32,
                        snapshot.params().cell_height
                    );
                    assert_eq!(
                        snapshot.palette()[0][0] * 2.0,
                        snapshot.params().cell_height
                    );
                }
            }
        });
    }

    #[test]
    fn invalid_publications_preserve_the_entire_previous_frame() {
        let initial = frame();
        let state = GpuRectangleState::new(initial.clone(), LIMITS).unwrap();
        let mut bad_rectangle = RECTANGLE;
        bad_rectangle.bounds[0] = f32::NAN;
        assert!(
            state
                .publish(&[bad_rectangle], &PALETTE, GpuRectangleParams::default())
                .is_err()
        );
        assert!(
            state
                .publish(&[RECTANGLE; 3], &PALETTE, GpuRectangleParams::default())
                .is_err()
        );
        assert!(
            state
                .publish(&[], &[], GpuRectangleParams::default())
                .is_err()
        );
        assert!(
            state
                .publish(&[], &[PALETTE[0]; 3], GpuRectangleParams::default())
                .is_err()
        );
        assert!(
            state
                .publish(&[], &[[f32::INFINITY; 4]], GpuRectangleParams::default())
                .is_err()
        );
        assert!(
            state
                .publish(
                    &[],
                    &PALETTE,
                    GpuRectangleParams {
                        cell_height: 0.0,
                        cell_gap: 1.0
                    }
                )
                .is_err()
        );
        assert!(Arc::ptr_eq(&initial, &state.snapshot().unwrap()));
        assert!(
            GpuRectangleState::new(
                initial,
                GpuRectangleLimits {
                    max_rectangles: 0,
                    ..LIMITS
                }
            )
            .is_err()
        );
    }

    #[test]
    fn validation_covers_geometry_palette_and_segmentation() {
        for bounds in [
            [-0.1, 0.0, 1.0, 1.0],
            [0.0, 0.0, -0.1, 1.0],
            [0.5, 0.0, 0.6, 1.0],
            [0.0, 0.5, 1.0, 0.6],
            [0.0, 0.0, f32::INFINITY, 1.0],
        ] {
            assert!(
                GpuRectangleFrame::new(
                    &[GpuRectangle {
                        bounds,
                        ..RECTANGLE
                    }],
                    &PALETTE,
                    GpuRectangleParams::default(),
                    LIMITS
                )
                .is_err()
            );
        }
        for coordinate in [-0.1, 1.1, f32::NAN, f32::INFINITY] {
            assert!(
                GpuRectangleFrame::new(
                    &[GpuRectangle {
                        palette_range: [coordinate, 1.0],
                        ..RECTANGLE
                    }],
                    &PALETTE,
                    GpuRectangleParams::default(),
                    LIMITS
                )
                .is_err()
            );
            assert!(
                GpuRectangleFrame::new(
                    &[],
                    &[[coordinate, 0.0, 0.0, 1.0]],
                    GpuRectangleParams::default(),
                    LIMITS
                )
                .is_err()
            );
        }
        for params in [
            GpuRectangleParams {
                cell_height: -1.0,
                cell_gap: 0.0,
            },
            GpuRectangleParams {
                cell_height: 1.0,
                cell_gap: -1.0,
            },
            GpuRectangleParams {
                cell_height: f32::NAN,
                cell_gap: 0.0,
            },
            GpuRectangleParams {
                cell_height: 1.0,
                cell_gap: f32::INFINITY,
            },
            GpuRectangleParams {
                cell_height: f32::MAX,
                cell_gap: f32::MAX,
            },
        ] {
            assert!(GpuRectangleFrame::new(&[], &PALETTE, params, LIMITS).is_err());
        }
        assert!(
            GpuRectangleFrame::new(
                &[],
                &PALETTE,
                GpuRectangleParams::default(),
                GpuRectangleLimits {
                    max_rectangles: 0,
                    max_palette_colors: 1
                }
            )
            .is_ok()
        );
        assert!(
            GpuRectangleFrame::new(
                &[],
                &PALETTE,
                GpuRectangleParams::default(),
                GpuRectangleLimits {
                    max_palette_colors: 0,
                    ..LIMITS
                }
            )
            .is_err()
        );
    }

    #[test]
    fn clear_releases_slot_content_but_preserves_captured_frames() {
        let initial = frame();
        let weak = Arc::downgrade(&initial);
        let state = GpuRectangleState::new(initial, LIMITS).unwrap();
        let captured = state.snapshot().unwrap();
        state.clear();
        assert!(state.snapshot().is_none());
        assert!(weak.upgrade().is_some());
        drop(captured);
        assert!(weak.upgrade().is_none());
        assert!(
            state
                .publish(&[], &[], GpuRectangleParams::default())
                .is_err()
        );
        assert!(state.snapshot().is_none());
        state
            .publish(&[], &PALETTE, GpuRectangleParams::default())
            .unwrap();
        assert!(state.snapshot().unwrap().rectangles().is_empty());
    }
}
