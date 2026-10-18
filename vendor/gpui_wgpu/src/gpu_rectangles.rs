//! Removable fixed rectangle program; not a generic application shader API.
//!
//! All allocations belong to one WgpuResources/device generation. Immutable
//! buffers are initialized while mapped, never overwritten with queue writes.
//! Both active and retired storage remain charged until their last submission
//! completes. There is no timer, blocking wait, or unbounded callback queue.
use collections::{FxHashMap, FxHashSet};
use gpui::{DevicePixels, GpuRectangleBatch, GpuRectangleFrame, GpuRectangleSource, Size};
use std::num::NonZeroU64;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
};

const SHADER: &str = include_str!("gpu_rectangles.wgsl");
const RECTANGLE_STRIDE: u64 = 32;
const PALETTE_STRIDE: u64 = 16;
const PLACEMENT_SIZE: u64 = 80;
// Experimental per-renderer limits, not a general GPUI resource policy. Count
// limits also bound metadata, bind groups, dynamic resolution and callbacks.
// Reserve a full initialization staging copy for every live allocation, even
// after upload completion. This conservative charge includes backends that
// implement mapped-at-creation STORAGE/UNIFORM buffers with a pending copy.
const INITIALIZATION_COPIES: u64 = 2;
const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ALLOCATIONS: usize = 8192;
const MAX_DRAWS: usize = 4096;
const MAX_PENDING_SUBMISSIONS: u64 = 8;

struct Upload {
    owner: Weak<GpuRectangleFrame>,
    binding: wgpu::BindGroup,
    _buffer: wgpu::Buffer,
    bytes: u64,
    last_submission: u64,
}
struct PlacementAllocation {
    _buffer: wgpu::Buffer,
    last_submission: u64,
}
struct Program {
    pipeline: wgpu::RenderPipeline,
    placement_layout: wgpu::BindGroupLayout,
    data_layout: wgpu::BindGroupLayout,
}
pub(crate) struct PreparedDraw {
    placement: wgpu::BindGroup,
    data: wgpu::BindGroup,
    instances: u32,
}
pub(crate) type PreparedRectangles = FxHashMap<usize, PreparedDraw>;

pub(crate) struct RectangleRenderer {
    program: Option<Program>,
    uploads: FxHashMap<u64, Upload>,
    placements: Vec<PlacementAllocation>,
    completed: Arc<AtomicU64>,
    submitted: u64,
    preparing: bool,
    bytes: u64,
    byte_limit: u64,
    allocation_limit: usize,
    premultiplied: bool,
    srgb_target: bool,
    target_format: wgpu::TextureFormat,
    program_attempted: bool,
    last_failure_kinds: u8,
    #[cfg(test)]
    upload_count: usize,
    #[cfg(test)]
    resolution_count: usize,
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
            has_dynamic_offset: false,
            min_binding_size: NonZeroU64::new(minimum),
        },
        count: None,
    }
}

impl RectangleRenderer {
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
    pub(crate) fn set_byte_limit(&mut self, limit: u64) {
        self.byte_limit = limit;
    }

    #[cfg(test)]
    pub(crate) fn disable_program(&mut self) {
        self.program = None;
        self.program_attempted = true;
    }
    fn create_program(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        alpha: wgpu::CompositeAlphaMode,
    ) -> Option<Program> {
        let premultiplied = alpha == wgpu::CompositeAlphaMode::PreMultiplied;
        let limits = device.limits();
        if limits.max_storage_buffers_per_shader_stage < 2
            || limits.max_storage_buffer_binding_size < RECTANGLE_STRIDE
        {
            None
        } else {
            let placement_layout =
                device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("rectangle placement layout"),
                    entries: &[buffer_entry(0, true, PLACEMENT_SIZE)],
                });
            let data_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("rectangle immutable data layout"),
                entries: &[
                    buffer_entry(0, false, RECTANGLE_STRIDE),
                    buffer_entry(1, false, PALETTE_STRIDE),
                ],
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("rectangle program layout"),
                bind_group_layouts: &[Some(&placement_layout), Some(&data_layout)],
                immediate_size: 0,
            });
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("fixed rectangle program"),
                source: wgpu::ShaderSource::Wgsl(SHADER.into()),
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("fixed rectangle program"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("rectangle_vertex"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("rectangle_fragment"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(if premultiplied {
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
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            });
            Some(Program {
                pipeline,
                placement_layout,
                data_layout,
            })
        }
    }

    pub(crate) fn new(
        _device: &wgpu::Device,
        format: wgpu::TextureFormat,
        alpha: wgpu::CompositeAlphaMode,
    ) -> Self {
        Self {
            program: None,
            uploads: FxHashMap::default(),
            placements: Vec::new(),
            completed: Arc::new(AtomicU64::new(0)),
            submitted: 0,
            preparing: false,
            bytes: 0,
            byte_limit: MAX_BYTES,
            allocation_limit: MAX_ALLOCATIONS,
            premultiplied: alpha == wgpu::CompositeAlphaMode::PreMultiplied,
            srgb_target: format.is_srgb(),
            target_format: format,
            program_attempted: false,
            last_failure_kinds: 0,
            #[cfg(test)]
            upload_count: 0,
            #[cfg(test)]
            resolution_count: 0,
        }
    }

    fn can_allocate(&self, bytes: u64) -> bool {
        bytes <= self.byte_limit.saturating_sub(self.bytes) / INITIALIZATION_COPIES
            && (self.uploads.len() + self.placements.len() + 1) * INITIALIZATION_COPIES as usize
                <= self.allocation_limit
    }

    /// Each dynamic slot (including a revoked slot) is read exactly once. This
    /// temporary table is the only renderer-owned strong CPU frame reference.
    fn resolve(&mut self, batches: &[GpuRectangleBatch]) -> Vec<Option<Arc<GpuRectangleFrame>>> {
        let mut states = FxHashMap::default();
        batches
            .iter()
            .take(MAX_DRAWS)
            .map(|batch| match &batch.source {
                GpuRectangleSource::Fixed(frame) => Some(frame.clone()),
                GpuRectangleSource::Dynamic(state) => states
                    .entry(Arc::as_ptr(state) as usize)
                    .or_insert_with(|| {
                        #[cfg(test)]
                        {
                            self.resolution_count += 1;
                        }
                        state.snapshot()
                    })
                    .clone(),
            })
            .collect()
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
                "GPU rectangles: skipped records (draw limit {}, invalid placement {}, upload {}, placement capacity {}, in-flight {}); first upload error: {first_upload_error:#?}",
                counts[0],
                counts[1],
                counts[2],
                counts[3],
                counts[4]
            );
        }
        // Report a new episode after recovery, not every draw of a persistent
        // failure. Counts describe the first frame of each category transition.
        self.last_failure_kinds = kinds;
    }

    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        batches: &[GpuRectangleBatch],
        size: Size<DevicePixels>,
    ) -> PreparedRectangles {
        debug_assert!(!self.preparing);
        if batches.is_empty() && self.uploads.is_empty() && self.placements.is_empty() {
            self.last_failure_kinds = 0;
            return PreparedRectangles::default();
        }
        // Poll only as part of a real draw, never on a hidden-window timer.
        let _ = device.poll(wgpu::PollType::Poll);
        let completed = self.completed.load(Ordering::Acquire);
        self.placements.retain(|allocation| {
            if allocation.last_submission <= completed {
                self.bytes -= PLACEMENT_SIZE * INITIALIZATION_COPIES;
                false
            } else {
                true
            }
        });
        let resolved = self.resolve(batches);
        let wanted: FxHashSet<_> = resolved.iter().flatten().map(|frame| frame.id()).collect();
        self.uploads.retain(|key, upload| {
            if upload.last_submission <= completed
                && (!wanted.contains(key) || upload.owner.strong_count() == 0)
            {
                self.bytes -= upload.bytes;
                false
            } else {
                true
            }
        });
        let mut prepared = PreparedRectangles::default();
        let mut skipped = [batches.len().saturating_sub(MAX_DRAWS), 0, 0, 0, 0];
        let mut first_upload_error = None;
        if !resolved
            .iter()
            .flatten()
            .any(|frame| !frame.rectangles().is_empty())
        {
            self.report_skipped(skipped, None);
            return prepared;
        }
        if !self.program_attempted {
            self.program = Self::create_program(
                device,
                self.target_format,
                if self.premultiplied {
                    wgpu::CompositeAlphaMode::PreMultiplied
                } else {
                    wgpu::CompositeAlphaMode::Opaque
                },
            );
            self.program_attempted = true;
            if self.program.is_none() {
                log::warn!(
                    "GPU rectangles: storage-buffer capability unavailable; skipping rectangle draws"
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
        for (index, (batch, frame)) in batches.iter().zip(resolved).enumerate() {
            let Some(frame) = frame else {
                continue;
            };
            if frame.rectangles().is_empty() {
                continue;
            }
            let params = frame.params();
            let period = (params.cell_height + params.cell_gap) * batch.scale_factor;
            if !valid_placement(batch) || !period.is_finite() {
                skipped[1] += 1;
                continue;
            }
            let key = frame.id();
            if !self.uploads.contains_key(&key) {
                if let Err(error) = self.upload(device, &frame, serial) {
                    skipped[2] += 1;
                    if first_upload_error.is_none() {
                        first_upload_error = Some(error);
                    }
                    continue;
                }
            }
            if !self.can_allocate(PLACEMENT_SIZE) {
                skipped[3] += 1;
                continue;
            }
            let program = self.program.as_ref().unwrap();
            let buffer = mapped_buffer(
                device,
                PLACEMENT_SIZE,
                wgpu::BufferUsages::UNIFORM,
                "rectangle placement",
                |bytes| {
                    encode_placement(
                        bytes,
                        batch,
                        &frame,
                        size,
                        self.premultiplied,
                        self.srgb_target,
                    );
                },
            );
            let placement = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rectangle placement"),
                layout: &program.placement_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                }],
            });
            let upload = self.uploads.get_mut(&key).unwrap();
            debug_assert!(upload.owner.ptr_eq(&Arc::downgrade(&frame)));
            upload.last_submission = serial;
            prepared.insert(
                index,
                PreparedDraw {
                    placement,
                    data: upload.binding.clone(),
                    instances: frame.rectangles().len() as u32,
                },
            );
            self.placements.push(PlacementAllocation {
                _buffer: buffer,
                last_submission: serial,
            });
            self.bytes += PLACEMENT_SIZE * INITIALIZATION_COPIES;
            self.preparing = true;
        }
        self.report_skipped(skipped, first_upload_error.as_ref());
        prepared
    }

    fn upload(
        &mut self,
        device: &wgpu::Device,
        frame: &Arc<GpuRectangleFrame>,
        serial: u64,
    ) -> anyhow::Result<()> {
        let limits = device.limits();
        let rectangles_size = (frame.rectangles().len() as u64)
            .checked_mul(RECTANGLE_STRIDE)
            .ok_or_else(|| anyhow::anyhow!("rectangle byte count overflow"))?;
        let palette_size = (frame.palette().len() as u64)
            .checked_mul(PALETTE_STRIDE)
            .ok_or_else(|| anyhow::anyhow!("palette byte count overflow"))?;
        anyhow::ensure!(
            frame.rectangles().len() <= u32::MAX as usize
                && frame.palette().len() <= u32::MAX as usize,
            "shader count limit exceeded"
        );
        anyhow::ensure!(
            rectangles_size <= limits.max_storage_buffer_binding_size as u64
                && palette_size <= limits.max_storage_buffer_binding_size as u64,
            "storage binding size limit exceeded"
        );
        let alignment = u64::from(limits.min_storage_buffer_offset_alignment);
        let palette_offset = rectangles_size.div_ceil(alignment) * alignment;
        let bytes = palette_offset
            .checked_add(palette_size)
            .ok_or_else(|| anyhow::anyhow!("frame byte count overflow"))?;
        // Reserve room for the first placement as part of admission; a failed
        // draw does not fill the cache with otherwise unusable uploads.
        anyhow::ensure!(
            bytes <= limits.max_buffer_size
                && self.can_allocate(bytes + PLACEMENT_SIZE)
                && (self.uploads.len() + self.placements.len() + 2)
                    * INITIALIZATION_COPIES as usize
                    <= self.allocation_limit,
            "experimental aggregate rectangle resource limit exceeded"
        );
        let buffer = mapped_buffer(
            device,
            bytes,
            wgpu::BufferUsages::STORAGE,
            "immutable rectangle frame",
            |output| encode_frame(output, frame, palette_offset as usize),
        );
        let binding = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("immutable rectangle frame"),
            layout: &self.program.as_ref().unwrap().data_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &buffer,
                        offset: 0,
                        size: NonZeroU64::new(rectangles_size),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &buffer,
                        offset: palette_offset,
                        size: NonZeroU64::new(palette_size),
                    }),
                },
            ],
        });
        let charge = bytes * INITIALIZATION_COPIES;
        self.uploads.insert(
            frame.id(),
            Upload {
                owner: Arc::downgrade(frame),
                binding,
                _buffer: buffer,
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

    pub(crate) fn draw(&self, draw: &PreparedDraw, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.program.as_ref().unwrap().pipeline);
        pass.set_bind_group(0, &draw.placement, &[]);
        pass.set_bind_group(1, &draw.data, &[]);
        pass.draw(0..6, 0..draw.instances);
    }

    /// Called after either the real submission or the error-path flush. An
    /// unsubmitted recording never escapes accounting. At most eight callbacks
    /// can be outstanding, each holding only an atomic completion watermark.
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
        bytes.fill(0);
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
fn encode_frame(
    bytes: &mut wgpu::WriteOnly<'_, [u8]>,
    frame: &GpuRectangleFrame,
    palette_offset: usize,
) {
    for (index, rectangle) in frame.rectangles().iter().enumerate() {
        let offset = index * RECTANGLE_STRIDE as usize;
        put_floats(bytes, offset, &rectangle.bounds);
        put_floats(bytes, offset + 16, &rectangle.palette_range);
    }
    for (index, color) in frame.palette().iter().enumerate() {
        put_floats(
            bytes,
            palette_offset + index * PALETTE_STRIDE as usize,
            color,
        );
    }
}
fn encode_placement(
    bytes: &mut wgpu::WriteOnly<'_, [u8]>,
    batch: &GpuRectangleBatch,
    frame: &GpuRectangleFrame,
    size: Size<DevicePixels>,
    premultiplied: bool,
    srgb: bool,
) {
    let bounds = batch.bounds;
    let mask = batch.content_mask.bounds;
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
    put_floats(
        bytes,
        48,
        &[frame.params().cell_height, frame.params().cell_gap],
    );
    put_u32(bytes, 56, frame.palette().len() as u32);
    put_u32(bytes, 60, premultiplied as u32);
    put_u32(bytes, 64, srgb as u32);
}
fn valid_placement(batch: &GpuRectangleBatch) -> bool {
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
#[path = "gpu_rectangles_abi_tests.rs"]
mod tests;
