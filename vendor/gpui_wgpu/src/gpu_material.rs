//! Closed numerical palette materials, owned by one WgpuResources generation.
//! Immutable mapped uploads and placements remain charged until completion;
//! weak CPU owners, bounded metadata and completion callbacks prevent retention.
use collections::{FxHashMap, FxHashSet};
use gpui::{
    DevicePixels, GpuContent, GpuDraw, GpuFrame, GpuPalette, GpuSource, GpuValueMapping, Size,
};
use std::num::NonZeroU64;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
};

const SHADER: &str = concat!(
    include_str!("gpu_material.wgsl"),
    include_str!("path_coverage.wgsl")
);
const RECTANGLE_STRIDE: u64 = 32;
const WAVEFORM_STRIDE: u64 = 16;
const PATH_STRIDE: u64 = 16;
const PALETTE_STRIDE: u64 = 16;
const PLACEMENT_SIZE: u64 = 128;
// Charge an initialization staging copy even after upload completion.
const INITIALIZATION_COPIES: u64 = 2;
const MAX_BYTES: u64 = 64 * 1024 * 1024;

fn material_supported(limits: &wgpu::Limits, flags: wgpu::DownlevelFlags) -> bool {
    // The composed module includes sample-interpolated path coverage even
    // when the first requested material itself is single-sampled.
    flags.contains(wgpu::DownlevelFlags::MULTISAMPLED_SHADING)
        && limits.max_storage_buffers_per_shader_stage >= 2
        && limits.max_storage_buffer_binding_size >= RECTANGLE_STRIDE
}

fn material_bytes_admitted(used: u64, additional: u64, limit: u64) -> bool {
    additional
        .checked_mul(INITIALIZATION_COPIES)
        .and_then(|charge| used.checked_add(charge))
        .is_some_and(|total| total <= limit)
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    #[test]
    fn initialized_uploads_account_for_resident_generations_and_overflow() {
        let full_resolution = 3600 * (4096 * 4 + 8);
        assert!(!material_bytes_admitted(
            0,
            full_resolution,
            64 * 1024 * 1024
        ));
        let sampled = 3600 * (580 * 4 + 8);
        assert!(material_bytes_admitted(0, sampled, 16 * 1024 * 1024));
        assert!(material_bytes_admitted(
            sampled * 2,
            sampled,
            32 * 1024 * 1024
        ));
        assert!(material_bytes_admitted(
            32 * 1024 * 1024,
            sampled * 2,
            64 * 1024 * 1024
        ));
        assert!(material_bytes_admitted(60, 2, 64));
        assert!(!material_bytes_admitted(61, 2, 64));
        assert!(!material_bytes_admitted(65, 0, 64));
        assert!(!material_bytes_admitted(0, u64::MAX, u64::MAX));
        assert!(!material_bytes_admitted(u64::MAX, 1, u64::MAX));
    }
}
const MAX_ALLOCATIONS: usize = 8192;
const MAX_DRAWS: usize = 4096;
const MAX_PENDING_SUBMISSIONS: u64 = 8;

struct Upload {
    owner: Weak<GpuFrame>,
    binding: wgpu::BindGroup,
    _buffer: wgpu::Buffer,
    palette_id: u64,
    bytes: u64,
    last_submission: u64,
}
struct PaletteUpload {
    owner: Weak<GpuPalette>,
    buffer: wgpu::Buffer,
    bytes: u64,
    last_submission: u64,
}
struct PlacementAllocation {
    _buffer: wgpu::Buffer,
    bytes: u64,
    last_submission: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MaterialKind {
    Rectangles,
    Field,
    Path,
    Segments,
    Waveform,
}
impl MaterialKind {
    fn multisampled(self) -> bool {
        self != Self::Field
    }
}
struct Program {
    pipelines: [Option<wgpu::RenderPipeline>; 5],
    layout: wgpu::PipelineLayout,
    shader: wgpu::ShaderModule,
    placement_layout: wgpu::BindGroupLayout,
    data_layout: wgpu::BindGroupLayout,
}
pub(crate) struct PreparedDraw {
    placement_offset: u32,
    data: wgpu::BindGroup,
    instances: u32,
    vertices: u32,
    kind: MaterialKind,
    raster_bounds: Option<gpui::Bounds<gpui::ScaledPixels>>,
    pub(crate) visible_bounds: gpui::Bounds<gpui::ScaledPixels>,
}
impl PreparedDraw {
    pub(crate) fn multisampled(&self) -> bool {
        self.kind.multisampled()
    }
}
pub(crate) struct MsaaGroup {
    pub(crate) range: std::ops::Range<usize>,
    pub(crate) bounds: gpui::Bounds<gpui::ScaledPixels>,
    pub(crate) raster_bounds: gpui::Bounds<gpui::ScaledPixels>,
}
#[derive(Default)]
pub(crate) struct PreparedMaterials {
    pub(crate) draws: FxHashMap<usize, PreparedDraw>,
    placement: Option<wgpu::BindGroup>,
    pub(crate) msaa_groups: FxHashMap<usize, MsaaGroup>,
    pub(crate) msaa_extent: (u32, u32),
}

/// Round storage only, never the composite geometry or the fragment mask.
pub(crate) fn raster_bounds(
    bounds: gpui::Bounds<gpui::ScaledPixels>,
    size: Size<DevicePixels>,
) -> Option<gpui::Bounds<gpui::ScaledPixels>> {
    use gpui::{Bounds, Point, ScaledPixels};
    let clipped = bounds.intersect(&Bounds {
        origin: Point::default(),
        size: size.map(ScaledPixels::from),
    });
    if clipped.is_empty() {
        return None;
    }
    let origin = Point {
        x: ScaledPixels(clipped.origin.x.0.floor()),
        y: ScaledPixels(clipped.origin.y.0.floor()),
    };
    let end = clipped.bottom_right();
    Some(Bounds {
        origin,
        size: gpui::Size {
            width: ScaledPixels(end.x.0.ceil() - origin.x.0),
            height: ScaledPixels(end.y.0.ceil() - origin.y.0),
        },
    })
}

impl PreparedMaterials {
    fn plan_msaa(&mut self, ranges: &[std::ops::Range<usize>], size: Size<DevicePixels>) {
        use gpui::{Bounds, ScaledPixels};
        let viewport = Bounds {
            origin: gpui::Point::default(),
            size: size.map(ScaledPixels::from),
        };
        let integer = |bounds: Bounds<ScaledPixels>| {
            let end = bounds.bottom_right();
            bounds.origin.x.0.fract() == 0.0
                && bounds.origin.y.0.fract() == 0.0
                && end.x.0.fract() == 0.0
                && end.y.0.fract() == 0.0
        };
        let area = |bounds: Bounds<ScaledPixels>| {
            let clipped = bounds.intersect(&viewport);
            if clipped.is_empty() {
                return 0.0;
            }
            f64::from(clipped.size.width.0) * f64::from(clipped.size.height.0)
        };
        for range in ranges {
            let mut indices = range.clone().peekable();
            while let Some(index) = indices.next() {
                let Some(draw) = self.draws.get(&index).filter(|draw| draw.multisampled()) else {
                    continue;
                };
                let mut bounds = draw.visible_bounds;
                let mut covered_area = area(bounds);
                let mut group_range = index..index + 1;
                if integer(bounds) {
                    while let Some(&next_index) = indices.peek() {
                        let Some(next) = self
                            .draws
                            .get(&next_index)
                            .filter(|draw| draw.multisampled())
                        else {
                            break;
                        };
                        let next_bounds = next.visible_bounds;
                        let union = bounds.union(&next_bounds);
                        // Disjoint integer composites can share a resolve. Do not
                        // turn distant small draws into a mostly empty attachment.
                        if !integer(next_bounds)
                            || bounds.intersects(&next_bounds)
                            || area(union) > 2.0 * (covered_area + area(next_bounds))
                        {
                            break;
                        }
                        covered_area += area(next_bounds);
                        bounds = union;
                        group_range.end = next_index + 1;
                        indices.next();
                    }
                }
                let Some(raster_bounds) = raster_bounds(bounds, size) else {
                    continue;
                };
                self.msaa_extent.0 = self.msaa_extent.0.max(raster_bounds.size.width.0 as u32);
                self.msaa_extent.1 = self.msaa_extent.1.max(raster_bounds.size.height.0 as u32);
                for draw_index in group_range.clone() {
                    self.draws.get_mut(&draw_index).unwrap().raster_bounds = Some(raster_bounds);
                }
                self.msaa_groups.insert(
                    index,
                    MsaaGroup {
                        range: group_range,
                        bounds,
                        raster_bounds,
                    },
                );
            }
        }
    }
}
pub(crate) struct MaterialRenderer {
    downlevel_flags: wgpu::DownlevelFlags,
    program: Option<Program>,
    uploads: FxHashMap<u64, Upload>,
    palettes: FxHashMap<u64, PaletteUpload>,
    placements: Vec<PlacementAllocation>,
    // Private bounded resolver scratch; cleared after each preparation so it
    // never extends publication lifetimes while the renderer is idle.
    resolved: Vec<Option<Arc<GpuFrame>>>,
    states: FxHashMap<usize, Option<Arc<GpuFrame>>>,
    wanted: FxHashSet<u64>,
    wanted_palettes: FxHashSet<u64>,
    live_palettes: FxHashSet<u64>,
    completed: Arc<AtomicU64>,
    submitted: u64,
    collected: u64,
    preparing: bool,
    bytes: u64,
    byte_limit: u64,
    allocation_limit: usize,
    premultiplied: bool,
    srgb_target: bool,
    target_format: wgpu::TextureFormat,
    path_samples: u32,
    program_attempted: bool,
    last_failure_kinds: u8,
    #[cfg(test)]
    upload_count: usize,
    #[cfg(test)]
    palette_upload_count: usize,
    #[cfg(test)]
    resolution_count: usize,
    #[cfg(test)]
    pipeline_counts: [usize; 5],
}
fn buffer_entry(binding: u32, uniform: bool, minimum: u64) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: if uniform {
                wgpu::BufferBindingType::Uniform
            } else {
                wgpu::BufferBindingType::Storage { read_only: true }
            },
            has_dynamic_offset: uniform,
            min_binding_size: NonZeroU64::new(minimum),
        },
        count: None,
    }
}
impl MaterialRenderer {
    #[cfg(test)]
    pub(crate) fn stats(&self) -> (usize, usize, u64, usize, usize) {
        (
            self.uploads.len(),
            self.placements.len(),
            self.bytes,
            self.upload_count,
            self.resolution_count,
        )
    }
    #[cfg(test)]
    pub(crate) fn palette_stats(&self) -> (usize, usize) {
        (self.palettes.len(), self.palette_upload_count)
    }
    #[cfg(test)]
    pub(crate) fn set_byte_limit(&mut self, limit: u64) {
        self.byte_limit = limit;
    }
    #[cfg(test)]
    pub(crate) fn set_downlevel_flags(&mut self, flags: wgpu::DownlevelFlags) {
        self.downlevel_flags = flags;
        self.program = None;
        self.program_attempted = false;
    }
    fn create_program(
        device: &wgpu::Device,
        downlevel_flags: wgpu::DownlevelFlags,
    ) -> Option<Program> {
        if !material_supported(&device.limits(), downlevel_flags) {
            return None;
        }
        let placement_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("GPU material placement layout"),
            entries: &[buffer_entry(0, true, PLACEMENT_SIZE)],
        });
        let data_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("GPU material immutable data layout"),
            entries: &[
                buffer_entry(0, false, 4),
                buffer_entry(1, false, PALETTE_STRIDE),
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("GPU material layout"),
            bind_group_layouts: &[Some(&placement_layout), Some(&data_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("GPU palette material"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        Some(Program {
            pipelines: [None, None, None, None, None],
            layout,
            shader,
            placement_layout,
            data_layout,
        })
    }
    fn ensure_pipeline(&mut self, device: &wgpu::Device, kind: MaterialKind) {
        let program = self.program.as_mut().unwrap();
        let pipeline = &mut program.pipelines[kind as usize];
        if pipeline.is_some() {
            return;
        }
        *pipeline = Some(
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(match kind {
                    MaterialKind::Rectangles => "GPU palette rectangles",
                    MaterialKind::Field => "GPU palette field",
                    MaterialKind::Path => "GPU palette path",
                    MaterialKind::Segments => "GPU palette segments",
                    MaterialKind::Waveform => "GPU palette waveform",
                }),
                layout: Some(&program.layout),
                vertex: wgpu::VertexState {
                    module: &program.shader,
                    entry_point: Some(match kind {
                        MaterialKind::Rectangles => "rectangle_vertex",
                        MaterialKind::Field => "material_vertex",
                        MaterialKind::Path => "path_vertex",
                        MaterialKind::Segments => "segment_vertex",
                        MaterialKind::Waveform => "waveform_vertex",
                    }),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &program.shader,
                    entry_point: Some(match kind {
                        MaterialKind::Rectangles => "rectangle_fragment",
                        MaterialKind::Field => "material_fragment",
                        MaterialKind::Path => "path_fragment",
                        MaterialKind::Segments => "segment_fragment",
                        MaterialKind::Waveform => "waveform_fragment",
                    }),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: self.target_format,
                        blend: Some(if kind.multisampled() || self.premultiplied {
                            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING
                        } else {
                            wgpu::BlendState::ALPHA_BLENDING
                        }),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState {
                    count: if kind.multisampled() {
                        self.path_samples
                    } else {
                        1
                    },
                    ..Default::default()
                },
                multiview_mask: None,
                cache: None,
            }),
        );
        #[cfg(test)]
        {
            self.pipeline_counts[kind as usize] += 1;
        }
    }
    pub(crate) fn new(
        _device: &wgpu::Device,
        format: wgpu::TextureFormat,
        alpha: wgpu::CompositeAlphaMode,
        path_samples: u32,
        downlevel_flags: wgpu::DownlevelFlags,
    ) -> Self {
        Self {
            downlevel_flags,
            program: None,
            uploads: FxHashMap::default(),
            palettes: FxHashMap::default(),
            placements: Vec::new(),
            resolved: Vec::new(),
            states: FxHashMap::default(),
            wanted: FxHashSet::default(),
            wanted_palettes: FxHashSet::default(),
            live_palettes: FxHashSet::default(),
            completed: Arc::new(AtomicU64::new(0)),
            submitted: 0,
            collected: 0,
            preparing: false,
            bytes: 0,
            byte_limit: MAX_BYTES,
            allocation_limit: MAX_ALLOCATIONS,
            premultiplied: alpha == wgpu::CompositeAlphaMode::PreMultiplied,
            srgb_target: format.is_srgb(),
            target_format: format,
            path_samples,
            program_attempted: false,
            last_failure_kinds: 0,
            #[cfg(test)]
            upload_count: 0,
            #[cfg(test)]
            palette_upload_count: 0,
            #[cfg(test)]
            resolution_count: 0,
            #[cfg(test)]
            pipeline_counts: [0; 5],
        }
    }
    pub(crate) fn configure_budget(&mut self, bytes: Option<u64>) {
        if let Some(bytes) = bytes {
            self.byte_limit = bytes;
        }
    }

    fn can_allocate(&self, bytes: u64, count: usize) -> bool {
        material_bytes_admitted(self.bytes, bytes, self.byte_limit)
            && (self.uploads.len() + self.palettes.len() + self.placements.len() + count)
                * INITIALIZATION_COPIES as usize
                <= self.allocation_limit
    }
    /// Resolve each slot, including a cleared slot, once per recorded frame.
    fn resolve(&mut self, batches: &[GpuDraw]) -> Vec<Option<Arc<GpuFrame>>> {
        let mut resolved = std::mem::take(&mut self.resolved);
        resolved.extend(batches.iter().take(MAX_DRAWS).map(|batch| {
            match &batch.source {
                GpuSource::Fixed(frame) => Some(frame.clone()),
                GpuSource::Dynamic(state) => self
                    .states
                    .entry(Arc::as_ptr(state) as usize)
                    .or_insert_with(|| {
                        #[cfg(test)]
                        {
                            self.resolution_count += 1;
                        }
                        state.snapshot()
                    })
                    .clone(),
            }
        }));
        self.states.clear();
        resolved
    }
    fn report_skipped(&mut self, counts: [usize; 5], first_upload_error: Option<&anyhow::Error>) {
        let kinds = counts
            .iter()
            .enumerate()
            .fold(0_u8, |mask, (index, count)| {
                mask | (u8::from(*count != 0) << index)
            });
        if kinds != 0 && kinds != self.last_failure_kinds {
            log::warn!(
                "GPU materials: skipped records (draw limit {}, invalid placement {}, upload {}, placement capacity {}, in-flight {}); first upload error: {first_upload_error:#?}",
                counts[0],
                counts[1],
                counts[2],
                counts[3],
                counts[4]
            );
        }
        self.last_failure_kinds = kinds;
    }
    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        batches: &[GpuDraw],
        size: Size<DevicePixels>,
        msaa_ranges: Option<&[std::ops::Range<usize>]>,
    ) -> PreparedMaterials {
        debug_assert!(!self.preparing);
        if batches.is_empty()
            && self.uploads.is_empty()
            && self.palettes.is_empty()
            && self.placements.is_empty()
        {
            self.last_failure_kinds = 0;
            return PreparedMaterials::default();
        }
        if self.submitted > self.completed.load(Ordering::Acquire) {
            let _ = device.poll(wgpu::PollType::Poll);
        }
        let mut resolved = self.resolve(batches);
        self.wanted.clear();
        self.wanted
            .extend(resolved.iter().flatten().map(|frame| frame.id()));
        self.wanted_palettes.clear();
        self.wanted_palettes
            .extend(resolved.iter().flatten().map(|frame| frame.palette().id()));
        self.collect_completed();
        let prepared = self.prepare_resolved(device, batches, size, msaa_ranges, &resolved);
        resolved.clear();
        self.resolved = resolved;
        prepared
    }

    /// No presentation, uploads or waits. Hidden scenes discard their wanted
    /// set, but in-flight storage stays charged until its completion callback.
    pub(crate) fn maintain(&mut self, device: &wgpu::Device, discard: bool) -> bool {
        debug_assert!(!self.preparing);
        let changed = discard && (!self.wanted.is_empty() || !self.wanted_palettes.is_empty());
        if discard {
            self.wanted.clear();
            self.wanted_palettes.clear();
        }
        if self.submitted > self.completed.load(Ordering::Acquire) {
            let _ = device.poll(wgpu::PollType::Poll);
        }
        if changed || self.completed.load(Ordering::Acquire) > self.collected {
            self.collect_completed();
        }
        // Another thread may deliver completion just after collection. Stop
        // only at the watermark whose resources we actually reclaimed, never
        // at a newer atomic value that would strand the final allocations.
        self.submitted > self.collected
    }

    fn collect_completed(&mut self) {
        let completed = self.completed.load(Ordering::Acquire);
        self.collected = completed;
        self.placements.retain(|allocation| {
            if allocation.last_submission <= completed {
                self.bytes -= allocation.bytes;
                false
            } else {
                true
            }
        });
        self.uploads.retain(|key, upload| {
            if upload.last_submission <= completed
                && (!self.wanted.contains(key) || upload.owner.strong_count() == 0)
            {
                self.bytes -= upload.bytes;
                false
            } else {
                true
            }
        });
        self.live_palettes.clear();
        self.live_palettes
            .extend(self.wanted_palettes.iter().copied());
        self.live_palettes
            .extend(self.uploads.values().map(|upload| upload.palette_id));
        self.palettes.retain(|key, upload| {
            if upload.last_submission <= completed
                && (!self.live_palettes.contains(key) || upload.owner.strong_count() == 0)
            {
                self.bytes -= upload.bytes;
                false
            } else {
                true
            }
        });
    }

    fn prepare_resolved(
        &mut self,
        device: &wgpu::Device,
        batches: &[GpuDraw],
        size: Size<DevicePixels>,
        msaa_ranges: Option<&[std::ops::Range<usize>]>,
        resolved: &[Option<Arc<GpuFrame>>],
    ) -> PreparedMaterials {
        let completed = self.completed.load(Ordering::Acquire);
        let mut prepared = PreparedMaterials::default();
        let mut skipped = [batches.len().saturating_sub(MAX_DRAWS), 0, 0, 0, 0];
        let mut first_upload_error = None;
        if !resolved
            .iter()
            .flatten()
            .any(|frame| draw_counts(frame).0 != 0)
        {
            self.report_skipped(skipped, None);
            return prepared;
        }
        if !self.program_attempted {
            self.program = Self::create_program(device, self.downlevel_flags);
            self.program_attempted = true;
            if self.program.is_none() {
                log::warn!(
                    "GPU materials: storage-buffer or multisampled-shading capability unavailable; skipping material draws"
                );
            }
        }
        if self.program.is_none() {
            return prepared;
        }
        if self.submitted - completed >= MAX_PENDING_SUBMISSIONS {
            skipped[4] = resolved.len();
            self.report_skipped(skipped, None);
            return prepared;
        }
        let serial = self.submitted + 1;
        let limits = device.limits();
        let placement_stride =
            PLACEMENT_SIZE.next_multiple_of(u64::from(limits.min_uniform_buffer_offset_alignment));
        for (index, (batch, frame)) in batches.iter().zip(resolved).enumerate() {
            let Some(frame) = frame else {
                continue;
            };
            let (instances, vertices, kind) = draw_counts(&frame);
            if instances == 0 {
                continue;
            }
            let period_valid = match frame.content() {
                GpuContent::Waveform { .. } => {
                    (batch.bounds.size.width.0 / batch.scale_factor).is_finite()
                        && (batch.bounds.size.height.0 / batch.scale_factor).is_finite()
                }
                GpuContent::Rectangles { params, .. } => {
                    ((params.cell_height + params.cell_gap) * batch.scale_factor).is_finite()
                }
                GpuContent::Segments { layers, .. } => layers
                    .iter()
                    .all(|layer| (layer.width * batch.scale_factor).is_finite()),
                _ => true,
            };
            if !valid_placement(batch) || !period_valid {
                skipped[1] += 1;
                continue;
            }
            let visible_bounds = batch.visible_bounds_for(frame);
            if visible_bounds.is_empty() {
                continue;
            }
            let placement_offset = prepared.draws.len() as u64 * placement_stride;
            let placement_bytes = placement_offset + PLACEMENT_SIZE;
            if placement_offset > u64::from(u32::MAX)
                || placement_bytes > limits.max_buffer_size
                || !self.can_allocate(placement_bytes, 1)
            {
                skipped[3] += 1;
                continue;
            }
            let key = frame.id();
            if !self.uploads.contains_key(&key) {
                if let Err(error) = self.upload(device, frame, serial, placement_bytes) {
                    skipped[2] += 1;
                    if first_upload_error.is_none() {
                        first_upload_error = Some(error);
                    }
                    continue;
                }
            }
            // Only admitted draws demand a pipeline; replay retains each variant.
            self.ensure_pipeline(device, kind);
            let upload = self.uploads.get_mut(&key).unwrap();
            debug_assert!(upload.owner.ptr_eq(&Arc::downgrade(frame)));
            upload.last_submission = serial;
            self.palettes
                .get_mut(&upload.palette_id)
                .unwrap()
                .last_submission = serial;
            prepared.draws.insert(
                index,
                PreparedDraw {
                    placement_offset: placement_offset as u32,
                    data: upload.binding.clone(),
                    instances,
                    vertices,
                    kind,
                    raster_bounds: None,
                    visible_bounds,
                },
            );
        }
        if let Some(ranges) = msaa_ranges {
            prepared.plan_msaa(ranges, size);
        }
        if !prepared.draws.is_empty() {
            let bytes = (prepared.draws.len() as u64 - 1) * placement_stride + PLACEMENT_SIZE;
            // One exact-size immutable slab per frame, not a retained high-water pool.
            // Offsets satisfy the device alignment; padding and initialization stay charged.
            let buffer = mapped_buffer(
                device,
                bytes,
                wgpu::BufferUsages::UNIFORM,
                "GPU frame placements",
                |mapped| {
                    for (&index, draw) in &prepared.draws {
                        let offset = draw.placement_offset as usize;
                        encode_placement(
                            &mut mapped.slice(offset..offset + PLACEMENT_SIZE as usize),
                            &batches[index],
                            resolved[index].as_ref().unwrap(),
                            size,
                            self.premultiplied,
                            self.srgb_target,
                        );
                        if let Some(raster) = draw.raster_bounds {
                            put_floats(
                                &mut mapped.slice(offset..offset + PLACEMENT_SIZE as usize),
                                112,
                                &[raster.origin.x.0, raster.origin.y.0],
                            );
                        }
                    }
                },
            );
            prepared.placement = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("GPU frame placements"),
                layout: &self.program.as_ref().unwrap().placement_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &buffer,
                        offset: 0,
                        size: NonZeroU64::new(PLACEMENT_SIZE),
                    }),
                }],
            }));
            let charge = bytes * INITIALIZATION_COPIES;
            self.placements.push(PlacementAllocation {
                _buffer: buffer,
                bytes: charge,
                last_submission: serial,
            });
            self.bytes += charge;
            self.preparing = true;
        }
        self.report_skipped(skipped, first_upload_error.as_ref());
        prepared
    }
    fn upload(
        &mut self,
        device: &wgpu::Device,
        frame: &Arc<GpuFrame>,
        serial: u64,
        placement_bytes: u64,
    ) -> anyhow::Result<()> {
        let limits = device.limits();
        let data_size = match frame.content() {
            GpuContent::Waveform { columns, .. } => {
                (columns.len() as u64).checked_mul(WAVEFORM_STRIDE)
            }
            GpuContent::Rectangles { rectangles, .. } => {
                (rectangles.len() as u64).checked_mul(RECTANGLE_STRIDE)
            }
            GpuContent::Field {
                values, intervals, ..
            } => (values.len() as u64)
                .checked_mul(4)
                .and_then(|n| n.checked_add(intervals.as_ref().map_or(0, |v| v.len() as u64 * 8))),
            GpuContent::Path { path, .. } => (path.vertices.len() as u64).checked_mul(PATH_STRIDE),
            GpuContent::Segments { segments, layers } => {
                (segments.len() as u64).checked_mul(32).and_then(|n| {
                    (layers.len() as u64)
                        .checked_mul(16)
                        .and_then(|l| n.checked_add(l))
                })
            }
        }
        .ok_or_else(|| anyhow::anyhow!("material byte count overflow"))?
        .max(4);
        let palette_size = (frame.palette().colors().len() as u64)
            .checked_mul(PALETTE_STRIDE)
            .ok_or_else(|| anyhow::anyhow!("palette byte count overflow"))?;
        anyhow::ensure!(
            data_size <= limits.max_storage_buffer_binding_size as u64
                && palette_size <= limits.max_storage_buffer_binding_size as u64
                && data_size <= limits.max_buffer_size
                && palette_size <= limits.max_buffer_size,
            "storage binding size limit exceeded"
        );
        let palette_id = frame.palette().id();
        let new_palette = !self.palettes.contains_key(&palette_id);
        let bytes = data_size + if new_palette { palette_size } else { 0 };
        // Admission reserves the complete aligned placement slab, including earlier draws.
        anyhow::ensure!(
            self.can_allocate(bytes + placement_bytes, 2 + usize::from(new_palette)),
            "aggregate GPU material resource limit exceeded"
        );
        if new_palette {
            let buffer = mapped_buffer(
                device,
                palette_size,
                wgpu::BufferUsages::STORAGE,
                "immutable GPU palette",
                |bytes| {
                    for (index, color) in frame.palette().colors().iter().enumerate() {
                        put_floats(bytes, index * PALETTE_STRIDE as usize, color);
                    }
                },
            );
            let charge = palette_size * INITIALIZATION_COPIES;
            self.palettes.insert(
                palette_id,
                PaletteUpload {
                    owner: Arc::downgrade(frame.palette()),
                    buffer,
                    bytes: charge,
                    last_submission: serial,
                },
            );
            self.bytes += charge;
            #[cfg(test)]
            {
                self.palette_upload_count += 1;
            }
        }
        let buffer = mapped_buffer(
            device,
            data_size,
            wgpu::BufferUsages::STORAGE,
            "immutable GPU material data",
            |bytes| encode_frame(bytes, frame),
        );
        let binding = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("immutable GPU material"),
            layout: &self.program.as_ref().unwrap().data_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.palettes[&palette_id].buffer.as_entire_binding(),
                },
            ],
        });
        let charge = data_size * INITIALIZATION_COPIES;
        self.uploads.insert(
            frame.id(),
            Upload {
                owner: Arc::downgrade(frame),
                binding,
                _buffer: buffer,
                palette_id,
                bytes: charge,
                last_submission: serial,
            },
        );
        self.bytes += charge;
        self.preparing = true;
        #[cfg(test)]
        {
            self.upload_count += 1;
        }
        Ok(())
    }
    pub(crate) fn draw(
        &self,
        materials: &PreparedMaterials,
        draw: &PreparedDraw,
        pass: &mut wgpu::RenderPass<'_>,
    ) {
        let program = self.program.as_ref().unwrap();
        pass.set_pipeline(program.pipelines[draw.kind as usize].as_ref().unwrap());
        pass.set_bind_group(
            0,
            materials.placement.as_ref().unwrap(),
            &[draw.placement_offset],
        );
        pass.set_bind_group(1, &draw.data, &[]);
        pass.draw(0..draw.vertices, 0..draw.instances);
    }
    /// record_frame calls this after submission or its error-path flush.
    /// No unsubmitted recording escapes accounting; at most eight callbacks.
    pub(crate) fn submitted(&mut self, queue: &wgpu::Queue) {
        if !self.preparing {
            return;
        }
        self.preparing = false;
        self.submitted += 1;
        let serial = self.submitted;
        let completed = self.completed.clone();
        queue.on_submitted_work_done(move || {
            completed.fetch_max(serial, Ordering::Release);
        });
    }
}
fn draw_counts(frame: &GpuFrame) -> (u32, u32, MaterialKind) {
    match frame.content() {
        GpuContent::Waveform { columns, .. } => {
            ((columns.len() * 4) as u32, 6, MaterialKind::Waveform)
        }
        GpuContent::Rectangles { rectangles, .. } => {
            (rectangles.len() as u32, 6, MaterialKind::Rectangles)
        }
        GpuContent::Field {
            values,
            width,
            intervals,
            ..
        } => (
            if intervals.is_some() && !values.is_empty() {
                *width
            } else {
                u32::from(!values.is_empty())
            },
            6,
            MaterialKind::Field,
        ),
        GpuContent::Segments { segments, layers } => (
            (segments.len() * layers.len()) as u32,
            6,
            MaterialKind::Segments,
        ),
        GpuContent::Path { path, .. } => (
            u32::from(
                !path.vertices.is_empty()
                    && f32::from(path.bounds.size.width) > 0.0
                    && f32::from(path.bounds.size.height) > 0.0,
            ),
            path.vertices.len() as u32,
            MaterialKind::Path,
        ),
    }
}
fn mapped_buffer(
    device: &wgpu::Device,
    size: u64,
    usage: wgpu::BufferUsages,
    label: &str,
    encode: impl FnOnce(&mut wgpu::WriteOnly<'_, [u8]>),
) -> wgpu::Buffer {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage,
        mapped_at_creation: true,
    });
    {
        let mut mapped = buffer.slice(..).get_mapped_range_mut();
        let mut bytes = mapped.slice(..);
        // wgpu 29 guarantees every new Buffer starts zeroed, including its
        // mapped-at-creation view. Preserve untouched ABI padding without a
        // second full upload-sized CPU zero pass.
        encode(&mut bytes);
    }
    buffer.unmap();
    buffer
}
fn put_u32(bytes: &mut wgpu::WriteOnly<'_, [u8]>, offset: usize, value: u32) {
    bytes
        .slice(offset..offset + 4)
        .copy_from_slice(&value.to_le_bytes());
}
fn put_f32(bytes: &mut wgpu::WriteOnly<'_, [u8]>, offset: usize, value: f32) {
    bytes
        .slice(offset..offset + 4)
        .copy_from_slice(&value.to_le_bytes());
}
fn put_floats(bytes: &mut wgpu::WriteOnly<'_, [u8]>, offset: usize, values: &[f32]) {
    for (index, value) in values.iter().enumerate() {
        put_f32(bytes, offset + index * 4, *value);
    }
}
fn encode_frame(bytes: &mut wgpu::WriteOnly<'_, [u8]>, frame: &GpuFrame) {
    match frame.content() {
        GpuContent::Waveform { columns, .. } => {
            for (index, column) in columns.iter().enumerate() {
                put_floats(bytes, index * WAVEFORM_STRIDE as usize, &column.levels);
            }
        }
        GpuContent::Rectangles { rectangles, .. } => {
            for (index, rectangle) in rectangles.iter().enumerate() {
                let offset = index * RECTANGLE_STRIDE as usize;
                put_floats(bytes, offset, &rectangle.bounds);
                put_floats(bytes, offset + 16, &rectangle.palette_range);
            }
        }
        GpuContent::Segments { segments, layers } => {
            for (index, layer) in layers.iter().enumerate() {
                put_floats(bytes, index * 16, &[layer.width]);
                put_u32(bytes, index * 16 + 4, layer.palette_offset);
            }
            for (index, segment) in segments.iter().enumerate() {
                let offset = layers.len() * 16 + index * 32;
                put_floats(bytes, offset, &segment.endpoints);
                put_u32(bytes, offset + 16, segment.color);
            }
        }
        GpuContent::Field {
            values, intervals, ..
        } => {
            let mut offset = 0;
            if let Some(intervals) = intervals {
                for interval in intervals.iter() {
                    put_floats(bytes, offset, interval);
                    offset += 8;
                }
            }
            put_floats(bytes, offset, values);
        }
        GpuContent::Path { path, .. } => {
            for (index, vertex) in path.vertices.iter().enumerate() {
                put_floats(
                    bytes,
                    index * PATH_STRIDE as usize,
                    &[
                        f32::from(vertex.xy_position.x),
                        f32::from(vertex.xy_position.y),
                        vertex.st_position.x,
                        vertex.st_position.y,
                    ],
                );
            }
        }
    }
}
fn encode_placement(
    bytes: &mut wgpu::WriteOnly<'_, [u8]>,
    batch: &GpuDraw,
    frame: &GpuFrame,
    size: Size<DevicePixels>,
    premultiplied: bool,
    srgb: bool,
) {
    let bounds = batch.bounds;
    let mask = batch.content_mask.bounds;
    put_floats(bytes, 112, &[0.0, 0.0]);
    put_floats(
        bytes,
        0,
        &[
            bounds.origin.x.0,
            bounds.origin.y.0,
            bounds.size.width.0,
            bounds.size.height.0,
        ],
    );
    put_floats(
        bytes,
        16,
        &[
            mask.origin.x.0,
            mask.origin.y.0,
            mask.size.width.0,
            mask.size.height.0,
        ],
    );
    put_floats(
        bytes,
        32,
        &[
            size.width.0 as f32,
            size.height.0 as f32,
            batch.opacity,
            batch.scale_factor,
        ],
    );
    let schema = match frame.content() {
        GpuContent::Waveform { columns, params } => {
            put_floats(
                bytes,
                48,
                &[
                    params.palette_start,
                    (bounds.size.width.0 / batch.scale_factor).ceil(),
                    (bounds.size.height.0 / batch.scale_factor).ceil(),
                ],
            );
            put_u32(bytes, 80, columns.len() as u32);
            4
        }
        GpuContent::Rectangles { params, .. } => {
            put_floats(bytes, 48, &[params.cell_height, params.cell_gap]);
            0
        }
        GpuContent::Field {
            width,
            height,
            params,
            intervals,
            ..
        } => {
            let (min, range, logarithmic) = match params.mapping {
                GpuValueMapping::Linear { min, max } => (min, max - min, false),
                GpuValueMapping::Logarithmic { db_range } => (0.0, db_range, true),
            };
            put_floats(bytes, 48, &[min, range]);
            put_u32(bytes, 80, *width);
            put_u32(bytes, 84, *height);
            put_u32(bytes, 88, logarithmic as u32);
            put_u32(bytes, 92, intervals.is_some() as u32);
            1
        }
        GpuContent::Segments { segments, layers } => {
            put_u32(bytes, 80, segments.len() as u32);
            put_u32(bytes, 84, layers.len() as u32);
            3
        }
        GpuContent::Path { path, params } => {
            put_floats(bytes, 48, &params.palette_range);
            put_floats(
                bytes,
                96,
                &[
                    f32::from(path.bounds.origin.x),
                    f32::from(path.bounds.origin.y),
                    f32::from(path.bounds.size.width),
                    f32::from(path.bounds.size.height),
                ],
            );
            2
        }
    };
    put_u32(bytes, 64, schema);
    put_u32(bytes, 68, frame.palette().colors().len() as u32);
    put_u32(bytes, 72, premultiplied as u32);
    put_u32(bytes, 76, srgb as u32);
}
fn valid_placement(batch: &GpuDraw) -> bool {
    let bounds = batch.bounds;
    let mask = batch.content_mask.bounds;
    [
        bounds.origin.x.0,
        bounds.origin.y.0,
        bounds.size.width.0,
        bounds.size.height.0,
        mask.origin.x.0,
        mask.origin.y.0,
        mask.size.width.0,
        mask.size.height.0,
        batch.opacity,
        batch.scale_factor,
    ]
    .iter()
    .all(|value| value.is_finite())
        && (bounds.origin.x.0 + bounds.size.width.0).is_finite()
        && (bounds.origin.y.0 + bounds.size.height.0).is_finite()
        && (mask.origin.x.0 + mask.size.width.0).is_finite()
        && (mask.origin.y.0 + mask.size.height.0).is_finite()
        && bounds.size.width.0 > 0.0
        && bounds.size.height.0 > 0.0
        && mask.size.width.0 >= 0.0
        && mask.size.height.0 >= 0.0
        && (0.0..=1.0).contains(&batch.opacity)
        && batch.scale_factor > 0.0
}
#[cfg(test)]
#[path = "gpu_material_abi_tests.rs"]
mod tests;
