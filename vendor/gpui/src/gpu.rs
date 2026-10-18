//! Bounded, immutable GPU publications for rectangles, scalar fields and paths.
//!
//! These are closed geometry/material schemas, not an arbitrary shader interface.
//! Publication owns no device resources and does not schedule a window repaint.

#[cfg(not(target_os = "macos"))]
use crate::Primitive;
use crate::{Bounds, ContentMask, DrawOrder, Path, Pixels, ScaledPixels};
use anyhow::{Result, ensure};
use parking_lot::RwLock;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

static NEXT_FRAME_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_PALETTE_ID: AtomicU64 = AtomicU64::new(1);

fn next_id(counter: &AtomicU64) -> Result<u64> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| anyhow::anyhow!("GPU content identity space exhausted"))
}

fn copy_slice<T: Copy>(values: &[T]) -> Result<Box<[T]>> {
    let mut owned = Vec::new();
    owned.try_reserve_exact(values.len())?;
    owned.extend_from_slice(values);
    Ok(owned.into_boxed_slice())
}

/// An independently reusable immutable straight-alpha sRGB palette.
/// Channels are numerical sRGB values; no color conversion is performed.
#[derive(Debug)]
pub struct GpuPalette {
    id: u64,
    colors: Box<[[f32; 4]]>,
}

impl GpuPalette {
    /// Validate nonempty, finite `0..=1` samples before allocating their copy.
    pub fn new(colors: &[[f32; 4]], max_colors: usize) -> Result<Arc<Self>> {
        ensure!(!colors.is_empty(), "GPU palette must not be empty");
        ensure!(
            colors.len() <= max_colors && colors.len() <= u32::MAX as usize,
            "GPU palette capacity exceeded"
        );
        ensure!(
            colors
                .iter()
                .flatten()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v)),
            "GPU palette channels must be finite and in 0..=1"
        );
        Ok(Arc::new(Self {
            id: next_id(&NEXT_PALETTE_ID)?,
            colors: copy_slice(colors)?,
        }))
    }

    /// Process-unique identity, never recycled. Renderer caches use weak ownership.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Immutable straight-alpha samples shared across frames and geometry kinds.
    pub fn colors(&self) -> &[[f32; 4]] {
        &self.colors
    }
}

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

/// Independent butt-capped line in normalized placement coordinates.
#[derive(Clone, Copy, Debug)]
pub struct GpuSegment {
    /// Finite endpoints; content masks, not the unit square, clip geometry.
    pub endpoints: [f32; 4],
    /// Base palette index, offset by each layer.
    pub color: u32,
}

/// Ordered stroke layer sharing the same endpoint storage.
#[derive(Clone, Copy, Debug)]
pub struct GpuStrokeLayer {
    /// Positive logical pixel width, independent of placement size.
    pub width: f32,
    /// Added to each segment's palette index.
    pub palette_offset: u32,
}

/// Stable CPU capacities. Unused geometry capacities may be zero.
#[derive(Clone, Copy, Debug)]
pub struct GpuLimits {
    /// Maximum procedural rectangle count.
    pub max_rectangles: usize,
    /// Maximum independent line count.
    pub max_segments: usize,
    /// Maximum ordered stroke layer count.
    pub max_stroke_layers: usize,
    /// Maximum scalar field width, including for empty fields.
    pub max_field_width: u32,
    /// Maximum scalar field height, including for empty fields.
    pub max_field_height: u32,
    /// Maximum scalar cell count; the dimension product is checked before copying.
    pub max_field_cells: usize,
    /// Maximum existing tessellated path vertex count.
    pub max_path_vertices: usize,
    /// Maximum palette sample count; must be positive.
    pub max_palette_colors: usize,
}

impl Default for GpuLimits {
    fn default() -> Self {
        Self {
            max_rectangles: 0,
            max_segments: 0,
            max_stroke_layers: 0,
            max_field_width: 0,
            max_field_height: 0,
            max_field_cells: 0,
            max_path_vertices: 0,
            max_palette_colors: 1,
        }
    }
}

impl GpuLimits {
    fn palette(self, palette: &GpuPalette) -> Result<()> {
        ensure!(
            self.max_palette_colors > 0 && palette.colors.len() <= self.max_palette_colors,
            "GPU palette capacity exceeded"
        );
        Ok(())
    }

    fn field(self, width: u32, height: u32, count: usize) -> Result<()> {
        ensure!(
            width <= self.max_field_width && height <= self.max_field_height,
            "GPU field extent capacity exceeded"
        );
        let cells = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| anyhow::anyhow!("GPU field dimensions overflow"))?;
        ensure!(
            cells <= self.max_field_cells && cells <= u32::MAX as usize,
            "GPU field cell capacity exceeded"
        );
        ensure!(
            count == cells,
            "GPU field dimensions do not match its cell count"
        );
        Ok(())
    }

    fn segments(self, count: usize, layers: usize) -> Result<()> {
        ensure!(
            count <= self.max_segments
                && layers <= self.max_stroke_layers
                && count <= u32::MAX as usize
                && layers <= u32::MAX as usize,
            "GPU segment capacity exceeded"
        );
        ensure!(
            count
                .checked_mul(layers)
                .is_some_and(|n| n <= u32::MAX as usize),
            "GPU segment instance count overflow"
        );
        Ok(())
    }

    fn content(self, content: &GpuContent) -> Result<()> {
        match content {
            GpuContent::Rectangles { rectangles, .. } => ensure!(
                rectangles.len() <= self.max_rectangles && rectangles.len() <= u32::MAX as usize,
                "GPU rectangle capacity exceeded"
            ),
            GpuContent::Segments { segments, layers } => {
                self.segments(segments.len(), layers.len())?;
            }
            GpuContent::Field {
                width,
                height,
                values,
                ..
            } => self.field(*width, *height, values.len())?,
            GpuContent::Path { path, .. } => ensure!(
                path.vertices.len() <= self.max_path_vertices
                    && path.vertices.len() <= u32::MAX as usize,
                "GPU path vertex capacity exceeded"
            ),
        }
        Ok(())
    }
}

/// Closed content kind, fixed for the lifetime of a dynamic publication slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpuSchema {
    /// Procedural rectangles with optional bottom-aligned segmentation.
    Rectangles,
    /// A row-major numerical scalar field.
    Field,
    /// Existing tessellated path geometry with palette material.
    Path,
    /// Independent lines expanded by the GPU, with ordered shared-geometry layers.
    Segments,
}

/// Scalar-to-palette coordinate mapping, followed by clamping to `0..=1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GpuValueMapping {
    /// `(value - min) / (max - min)`; finite ordered bounds and finite difference.
    /// Nonfinite input maps to coordinate zero.
    Linear {
        /// Scalar corresponding to palette coordinate zero.
        min: f32,
        /// Scalar corresponding to palette coordinate one.
        max: f32,
    },
    /// `(10 * log10(value) + db_range) / db_range`, for finite positive range.
    /// Nonfinite or nonpositive input maps to coordinate zero.
    Logarithmic {
        /// Positive logarithmic range in decibels.
        db_range: f32,
    },
}

impl GpuValueMapping {
    fn validate(self) -> Result<()> {
        match self {
            Self::Linear { min, max } => ensure!(
                min.is_finite() && max.is_finite() && min < max && (max - min).is_finite(),
                "invalid GPU linear mapping range"
            ),
            Self::Logarithmic { db_range } => ensure!(
                db_range.is_finite() && db_range > 0.0,
                "invalid GPU logarithmic mapping range"
            ),
        }
        Ok(())
    }
}

/// Parameters published atomically with field dimensions, samples and palette.
///
/// Rows run top to bottom, columns left to right. Normalized coordinates map to
/// `uv * extent - 0.5`; neighboring indices clamp to the edge. Thus a one-cell
/// dimension repeats its only sample. Either zero extent means empty content.
/// Palette lookup rounds `coordinate * (palette.len() - 1)` to the nearest index.
/// Each of four neighbors is mapped and palette-quantized independently before
/// bilinear interpolation of straight RGBA colors, matching a precolored image.
#[derive(Clone, Copy, Debug)]
pub struct GpuFieldParams {
    /// Scalar-to-palette mapping.
    pub mapping: GpuValueMapping,
}

/// Palette material parameters for existing path geometry.
#[derive(Clone, Copy, Debug)]
pub struct GpuPathParams {
    /// Bottom and top palette coordinates over the full original path bounds,
    /// unaffected by clipping. Palette colors are linearly interpolated.
    pub palette_range: [f32; 2],
}

impl Default for GpuPathParams {
    fn default() -> Self {
        Self {
            palette_range: [0.0, 1.0],
        }
    }
}

/// Immutable payload exposed by a frame. The frame never exposes mutable access.
#[derive(Debug)]
pub enum GpuContent {
    /// One endpoint payload reused across ordered stroke layers.
    Segments {
        /// Immutable ordered endpoints and base palette indices.
        segments: Box<[GpuSegment]>,
        /// Draw order is layer first, then segment order.
        layers: Box<[GpuStrokeLayer]>,
    },
    /// Normalized procedural rectangles.
    Rectangles {
        /// Owned immutable instance data.
        rectangles: Box<[GpuRectangle]>,
        /// Parameters belonging to this same publication.
        params: GpuRectangleParams,
    },
    /// A numerical field, never a CPU-precolored image.
    Field {
        /// Number of columns.
        width: u32,
        /// Number of rows.
        height: u32,
        /// Row-major f32 samples; nonfinite values are valid missing data.
        values: Box<[f32]>,
        /// Mapping belonging to this same publication.
        params: GpuFieldParams,
    },
    /// Immutable existing path snapshot; its tessellated coverage is preserved.
    Path {
        /// Original logical coordinates, normalized from `path.bounds` onto the
        /// recorded draw bounds. Paint does not copy or scale this vertex array.
        /// Embedded path color, order and masks are not used; draw-level
        /// material, opacity, clipping and ordering are authoritative.
        path: Arc<Path<Pixels>>,
        /// Material parameters over the full original mapping bounds.
        params: GpuPathParams,
    },
}

/// One validated immutable publication. No GPU resources or window are retained.
#[derive(Debug)]
pub struct GpuFrame {
    id: u64,
    content: GpuContent,
    palette: Arc<GpuPalette>,
}

fn validate_range(range: [f32; 2]) -> Result<()> {
    ensure!(
        range
            .iter()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v)),
        "GPU palette coordinates must be finite and in 0..=1"
    );
    Ok(())
}

fn validate_bounds(bounds: Bounds<ScaledPixels>) -> Result<()> {
    ensure!(
        bounds.origin.x.0.is_finite()
            && bounds.origin.y.0.is_finite()
            && bounds.size.width.0.is_finite()
            && bounds.size.height.0.is_finite()
            && bounds.size.width.0 >= 0.0
            && bounds.size.height.0 >= 0.0
            && bounds.right().0.is_finite()
            && bounds.bottom().0.is_finite(),
        "GPU bounds must have finite, nonnegative dimensions"
    );
    Ok(())
}

impl GpuFrame {
    fn new(content: GpuContent, palette: Arc<GpuPalette>) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            id: next_id(&NEXT_FRAME_ID)?,
            content,
            palette,
        }))
    }

    /// Validate the complete rectangle publication before copying its data.
    pub fn rectangles(
        rectangles: &[GpuRectangle],
        palette: Arc<GpuPalette>,
        params: GpuRectangleParams,
        limits: GpuLimits,
    ) -> Result<Arc<Self>> {
        limits.palette(&palette)?;
        ensure!(
            rectangles.len() <= limits.max_rectangles && rectangles.len() <= u32::MAX as usize,
            "GPU rectangle capacity exceeded"
        );
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
                rectangle.bounds.iter().all(|v| v.is_finite() && *v >= 0.0)
                    && x + width <= 1.0
                    && y + height <= 1.0,
                "GPU rectangle bounds must be contained in the unit square"
            );
            validate_range(rectangle.palette_range)?;
        }
        Self::new(
            GpuContent::Rectangles {
                rectangles: copy_slice(rectangles)?,
                params,
            },
            palette,
        )
    }

    /// Validate dimensions, checked cell product and mapping before copying cells.
    /// Zero width or height requires an empty slice. Nonfinite cells are accepted
    /// as missing data mapped to the first palette sample before color filtering.
    pub fn field(
        width: u32,
        height: u32,
        values: &[f32],
        palette: Arc<GpuPalette>,
        params: GpuFieldParams,
        limits: GpuLimits,
    ) -> Result<Arc<Self>> {
        limits.palette(&palette)?;
        limits.field(width, height, values.len())?;
        params.mapping.validate()?;
        Self::new(
            GpuContent::Field {
                width,
                height,
                values: copy_slice(values)?,
                params,
            },
            palette,
        )
    }

    /// Retain an existing immutable path without copying its vertices.
    /// Geometry must contain complete triangles with finite xy and st coordinates.
    /// Empty geometry or empty bounds is a valid empty drawing.
    pub fn path(
        path: Arc<Path<Pixels>>,
        palette: Arc<GpuPalette>,
        params: GpuPathParams,
        limits: GpuLimits,
    ) -> Result<Arc<Self>> {
        limits.palette(&palette)?;
        ensure!(
            path.vertices.len() <= limits.max_path_vertices
                && path.vertices.len() <= u32::MAX as usize,
            "GPU path vertex capacity exceeded"
        );
        ensure!(
            path.vertices.len() % 3 == 0,
            "GPU path requires complete triangles"
        );
        validate_bounds(path.bounds.scale(1.0))?;
        validate_range(params.palette_range)?;
        ensure!(
            path.vertices.iter().all(|v| v.xy_position.x.0.is_finite()
                && v.xy_position.y.0.is_finite()
                && v.st_position.x.is_finite()
                && v.st_position.y.is_finite()),
            "GPU path coordinates must be finite"
        );
        Self::new(GpuContent::Path { path, params }, palette)
    }

    /// Validate capacities and all indices before allocating either payload.
    pub fn segments(
        segments: &[GpuSegment],
        layers: &[GpuStrokeLayer],
        palette: Arc<GpuPalette>,
        limits: GpuLimits,
    ) -> Result<Arc<Self>> {
        limits.palette(&palette)?;
        limits.segments(segments.len(), layers.len())?;
        for layer in layers {
            ensure!(
                layer.width.is_finite() && layer.width > 0.0,
                "GPU stroke width must be finite and positive"
            );
        }
        for segment in segments {
            ensure!(
                segment.endpoints.iter().all(|v| v.is_finite()),
                "GPU segment endpoints must be finite"
            );
            for layer in layers {
                ensure!(
                    segment
                        .color
                        .checked_add(layer.palette_offset)
                        .is_some_and(|index| (index as usize) < palette.colors.len()),
                    "GPU segment palette index out of range"
                );
            }
        }
        Self::new(
            GpuContent::Segments {
                segments: copy_slice(segments)?,
                layers: copy_slice(layers)?,
            },
            palette,
        )
    }

    /// Process-unique immutable identity, never recycled after frame release.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Content kind used to enforce schema-stable dynamic publications.
    pub fn schema(&self) -> GpuSchema {
        match &self.content {
            GpuContent::Rectangles { .. } => GpuSchema::Rectangles,
            GpuContent::Field { .. } => GpuSchema::Field,
            GpuContent::Path { .. } => GpuSchema::Path,
            GpuContent::Segments { .. } => GpuSchema::Segments,
        }
    }

    /// Validated immutable geometry and parameters.
    pub fn content(&self) -> &GpuContent {
        &self.content
    }

    /// Independently reusable palette identity and samples.
    pub fn palette(&self) -> &Arc<GpuPalette> {
        &self.palette
    }
}

/// Latest-wins dynamic content, with fixed schema/capacities and no scheduling.
/// Cached scenes retain this slot, never a snapshot of its content.
///
/// A single synchronous owner must serialize publication and `clear`. Snapshots
/// may be captured concurrently. Clearing is not cancellation of a preparing publish:
/// serialize mutations before hiding a view so only an explicit later publication
/// can restore its content. Already captured frames remain valid after clearing.
#[derive(Debug)]
pub struct GpuState {
    current: RwLock<Option<Arc<GpuFrame>>>,
    limits: GpuLimits,
    schema: GpuSchema,
}

impl GpuState {
    /// Attach an immutable frame to a slot with fixed schema and capacities.
    pub fn new(initial: Arc<GpuFrame>, limits: GpuLimits) -> Result<Arc<Self>> {
        limits.palette(initial.palette())?;
        limits.content(initial.content())?;
        Ok(Arc::new(Self {
            schema: initial.schema(),
            current: RwLock::new(Some(initial)),
            limits,
        }))
    }

    /// Validate a rectangle update against this slot before allocating its snapshot.
    /// Failure preserves the current publication; success does not schedule repaint.
    pub fn publish_rectangles(
        &self,
        rectangles: &[GpuRectangle],
        palette: Arc<GpuPalette>,
        params: GpuRectangleParams,
    ) -> Result<()> {
        ensure!(
            self.schema == GpuSchema::Rectangles,
            "GPU publication schema mismatch"
        );
        let frame = GpuFrame::rectangles(rectangles, palette, params, self.limits)?;
        self.replace(frame);
        Ok(())
    }

    /// Validate field extent, count and mapping against this slot before copying cells.
    /// Failure preserves the current publication; success does not schedule repaint.
    pub fn publish_field(
        &self,
        width: u32,
        height: u32,
        values: &[f32],
        palette: Arc<GpuPalette>,
        params: GpuFieldParams,
    ) -> Result<()> {
        ensure!(
            self.schema == GpuSchema::Field,
            "GPU publication schema mismatch"
        );
        let frame = GpuFrame::field(width, height, values, palette, params, self.limits)?;
        self.replace(frame);
        Ok(())
    }

    /// Atomically replace all content after checking this slot's schema/capacities.
    /// Failure preserves the old publication (or the cleared state). Publication
    /// never requests a frame; the caller owns repaint scheduling.
    ///
    /// This attaches an already constructed snapshot, for sharing between slots or
    /// with fixed scenes. Its construction cost belongs to the caller and precedes
    /// this slot's checks. Use `publish_rectangles` or `publish_field` for raw updates
    /// that must be admitted under the slot's limits before payload allocation/copy.
    pub fn publish(&self, frame: Arc<GpuFrame>) -> Result<()> {
        ensure!(
            frame.schema() == self.schema,
            "GPU publication schema mismatch"
        );
        self.limits.palette(frame.palette())?;
        self.limits.content(frame.content())?;
        self.replace(frame);
        Ok(())
    }

    fn replace(&self, frame: Arc<GpuFrame>) {
        let previous = self.current.write().replace(frame);
        drop(previous);
    }

    /// Stable content kind, including while cleared.
    pub fn schema(&self) -> GpuSchema {
        self.schema
    }

    /// Capture a coherent publication. Renderers resolve each distinct slot only
    /// once per prepared frame, including when the captured value is `None`.
    pub fn snapshot(&self) -> Option<Arc<GpuFrame>> {
        self.current.read().clone()
    }

    /// Revoke this slot's content without revoking captured or fixed frames.
    /// Does not schedule repaint. The mutation owner must serialize this call.
    pub fn clear(&self) {
        let previous = self.current.write().take();
        drop(previous);
    }
}

/// Explicit replay policy independent of geometry/material kind.
#[derive(Clone, Debug)]
pub enum GpuSource {
    /// Replay exactly the immutable publication recorded at paint time.
    Fixed(Arc<GpuFrame>),
    /// Resolve the latest complete publication once per rendered frame.
    Dynamic(Arc<GpuState>),
}

/// Content-independent placement captured by the scene.
#[derive(Clone, Debug)]
pub struct GpuDraw {
    /// Scene-computed ordering key.
    pub order: DrawOrder,
    /// Physical-pixel placement; path mapping uses its original full bounds.
    pub bounds: Bounds<ScaledPixels>,
    /// Inherited physical-pixel clipping mask, independent of mapping bounds.
    pub content_mask: ContentMask<ScaledPixels>,
    /// Inherited element opacity, applied exactly once to sampled alpha.
    pub opacity: f32,
    /// Logical-to-physical scale for rectangle segmentation parameters.
    pub scale_factor: f32,
    /// Fixed or dynamically resolved complete content.
    pub source: GpuSource,
}

impl GpuDraw {
    pub(crate) fn from_logical(
        bounds: Bounds<Pixels>,
        content_mask: ContentMask<Pixels>,
        opacity: f32,
        scale_factor: f32,
        source: GpuSource,
    ) -> Result<Self> {
        ensure!(
            !cfg!(any(target_os = "macos", target_os = "ios")),
            "GPU drawings require a WGPU backend"
        );
        ensure!(
            scale_factor.is_finite() && scale_factor > 0.0,
            "invalid GPU scale factor"
        );
        ensure!(
            opacity.is_finite() && (0.0..=1.0).contains(&opacity),
            "invalid GPU opacity"
        );
        // Reject negative logical extents even if scaling would underflow to -0.
        ensure!(
            bounds.size.width.0 >= 0.0
                && bounds.size.height.0 >= 0.0
                && content_mask.bounds.size.width.0 >= 0.0
                && content_mask.bounds.size.height.0 >= 0.0,
            "GPU logical dimensions must be nonnegative"
        );
        let bounds = bounds.scale(scale_factor);
        let content_mask = content_mask.scale(scale_factor);
        validate_bounds(bounds)?;
        validate_bounds(content_mask.bounds)?;
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
impl From<GpuDraw> for Primitive {
    fn from(draw: GpuDraw) -> Self {
        Self::Gpu(draw)
    }
}

#[cfg(test)]
mod tests;
