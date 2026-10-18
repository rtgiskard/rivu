use super::*;
use gpui::{GpuLimits, GpuPalette, GpuRectangle, GpuRectangleParams};

#[test]
fn material_shader_and_byte_abi_match_structurally() {
    use naga::{AddressSpace, ArraySize, ScalarKind, TypeInner, VectorSize};
    let module = naga::front::wgsl::parse_str(SHADER).expect("fixed shader parses");
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::MULTISAMPLED_SHADING,
    )
    .validate(&module)
    .expect("fixed shader validates");
    let find = |name: &str| {
        module
            .types
            .iter()
            .find(|(_, ty)| ty.name.as_deref() == Some(name))
            .unwrap()
    };
    let (_, placement) = find("Placement");
    // These verify kind, width, dimensions, every member offset, struct span,
    // runtime-array stride and binding address spaces, not just total size.
    let check =
        |ty: &naga::Type, span_expected, fields: &[(u32, ScalarKind, Option<VectorSize>)]| {
            let TypeInner::Struct { members, span } = &ty.inner else {
                panic!("expected struct")
            };
            assert_eq!(*span, span_expected);
            assert_eq!(members.len(), fields.len());
            for (member, &(offset, kind, vector_size)) in members.iter().zip(fields) {
                assert_eq!(member.offset, offset);
                match (&module.types[member.ty].inner, vector_size) {
                    (TypeInner::Scalar(scalar), None) => {
                        assert_eq!(scalar.kind, kind);
                        assert_eq!(scalar.width, 4);
                    }
                    (TypeInner::Vector { size, scalar }, Some(expected)) => {
                        assert_eq!(*size, expected);
                        assert_eq!(scalar.kind, kind);
                        assert_eq!(scalar.width, 4);
                    }
                    _ => panic!("unexpected ABI field type"),
                }
            }
        };
    check(
        placement,
        PLACEMENT_SIZE as u32,
        &[
            (0, ScalarKind::Float, Some(VectorSize::Quad)),
            (16, ScalarKind::Float, Some(VectorSize::Quad)),
            (32, ScalarKind::Float, Some(VectorSize::Bi)),
            (40, ScalarKind::Float, None),
            (44, ScalarKind::Float, None),
            (48, ScalarKind::Float, Some(VectorSize::Quad)),
            (64, ScalarKind::Uint, Some(VectorSize::Quad)),
            (80, ScalarKind::Uint, Some(VectorSize::Quad)),
            (96, ScalarKind::Float, Some(VectorSize::Quad)),
        ],
    );
    let globals: Vec<_> = module
        .global_variables
        .iter()
        .map(|(_, variable)| variable)
        .collect();
    assert_eq!(globals.len(), 3);
    for (index, variable) in globals.iter().enumerate() {
        let binding = variable.binding.as_ref().unwrap();
        assert_eq!(
            (binding.group, binding.binding),
            [(0, 0), (1, 0), (1, 1)][index]
        );
        if index == 0 {
            assert_eq!(variable.space, AddressSpace::Uniform);
        } else {
            assert_eq!(
                variable.space,
                AddressSpace::Storage {
                    access: naga::StorageAccess::LOAD
                }
            );
            let TypeInner::Array { base, size, stride } = module.types[variable.ty].inner else {
                panic!("expected runtime array")
            };
            assert_eq!(size, ArraySize::Dynamic);
            if index == 1 {
                assert_eq!(stride, 4);
                assert_eq!(
                    module.types[base].inner,
                    TypeInner::Scalar(naga::Scalar {
                        kind: ScalarKind::Uint,
                        width: 4
                    })
                );
            } else {
                assert_eq!(stride, PALETTE_STRIDE as u32);
                assert_eq!(
                    module.types[base].inner,
                    TypeInner::Vector {
                        size: VectorSize::Quad,
                        scalar: naga::Scalar {
                            kind: ScalarKind::Float,
                            width: 4
                        }
                    }
                );
            }
        }
    }
    assert_eq!(
        buffer_entry(0, true, PLACEMENT_SIZE).ty,
        wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: NonZeroU64::new(PLACEMENT_SIZE)
        }
    );
    assert_eq!(
        buffer_entry(0, false, 4).ty,
        wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: NonZeroU64::new(4)
        }
    );
}

#[test]
fn fixed_rectangle_encoder_writes_little_endian_and_zero_padding() {
    let frame = GpuFrame::rectangles(
        &[GpuRectangle {
            bounds: [0.125, 0.25, 0.5, 0.75],
            palette_range: [0.25, 1.0],
        }],
        GpuPalette::new(&[[0.125, 0.25, 0.5, 1.0]], 1).unwrap(),
        GpuRectangleParams {
            cell_height: 3.0,
            cell_gap: 2.0,
        },
        GpuLimits {
            max_rectangles: 1,
            max_palette_colors: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let mut encoded = [0; RECTANGLE_STRIDE as usize];
    encode_frame(&mut wgpu::WriteOnly::from_mut(&mut encoded[..]), &frame);
    for (offset, value) in [
        (0, 0.125_f32),
        (4, 0.25),
        (8, 0.5),
        (12, 0.75),
        (16, 0.25),
        (20, 1.0),
    ] {
        assert_eq!(&encoded[offset..offset + 4], &value.to_le_bytes());
    }
    assert!(encoded[24..].iter().all(|byte| *byte == 0));
    let bounds = gpui::Bounds {
        origin: gpui::Point {
            x: 2.0.into(),
            y: 4.0.into(),
        },
        size: gpui::Size {
            width: 8.0.into(),
            height: 16.0.into(),
        },
    };
    let batch = GpuDraw {
        order: 0,
        bounds,
        content_mask: gpui::ContentMask { bounds },
        opacity: 0.5,
        scale_factor: 2.0,
        source: GpuSource::Fixed(frame.clone()),
    };
    let mut encoded = [0; PLACEMENT_SIZE as usize];
    encode_placement(
        &mut wgpu::WriteOnly::from_mut(&mut encoded[..]),
        &batch,
        &frame,
        Size {
            width: DevicePixels(32),
            height: DevicePixels(64),
        },
        true,
        false,
    );
    for (offset, value) in [
        (0, 2.0_f32),
        (4, 4.0),
        (8, 8.0),
        (12, 16.0),
        (16, 2.0),
        (20, 4.0),
        (24, 8.0),
        (28, 16.0),
        (32, 32.0),
        (36, 64.0),
        (40, 0.5),
        (44, 2.0),
        (48, 3.0),
        (52, 2.0),
    ] {
        assert_eq!(&encoded[offset..offset + 4], &value.to_le_bytes());
    }
    assert_eq!(&encoded[68..72], &1_u32.to_le_bytes());
    assert_eq!(&encoded[72..76], &1_u32.to_le_bytes());
    assert!(encoded[76..].iter().all(|byte| *byte == 0));
}

#[cfg(target_os = "linux")]
#[test]
fn rectangle_retirement_charges_unsubmitted_and_inflight_allocations() -> anyhow::Result<()> {
    let (context, format) = crate::WgpuContext::new_headless()?;
    let mut renderer =
        MaterialRenderer::new(&context.device, format, wgpu::CompositeAlphaMode::Opaque, 1);
    let frame = GpuFrame::rectangles(
        &[GpuRectangle {
            bounds: [0.0, 0.0, 1.0, 1.0],
            palette_range: [0.0, 1.0],
        }],
        GpuPalette::new(&[[1.0; 4]], 1)?,
        GpuRectangleParams::default(),
        GpuLimits {
            max_rectangles: 1,
            max_palette_colors: 1,
            ..Default::default()
        },
    )?;
    let bounds = gpui::Bounds {
        origin: gpui::Point {
            x: 0.0.into(),
            y: 0.0.into(),
        },
        size: gpui::Size {
            width: 8.0.into(),
            height: 8.0.into(),
        },
    };
    let batch = GpuDraw {
        order: 0,
        bounds,
        content_mask: gpui::ContentMask { bounds },
        opacity: 1.0,
        scale_factor: 1.0,
        source: GpuSource::Fixed(frame.clone()),
    };
    let size = Size {
        width: DevicePixels(8),
        height: DevicePixels(8),
    };
    renderer.allocation_limit = 3 * INITIALIZATION_COPIES as usize;
    let prepared = renderer.prepare(&context.device, &[batch.clone(), batch.clone()], size);
    assert_eq!(prepared.len(), 1);
    assert_eq!(renderer.pipeline_counts, [1, 0, 0, 0]);
    assert_eq!(
        renderer.last_failure_kinds,
        1 << 3,
        "second placement must respect allocation admission"
    );
    assert_eq!(
        renderer.resolve(&vec![batch.clone(); MAX_DRAWS + 1]).len(),
        MAX_DRAWS
    );
    renderer.allocation_limit = MAX_ALLOCATIONS;
    assert!(renderer.preparing);
    let charged = renderer.bytes;
    assert!(charged > PLACEMENT_SIZE);
    renderer.byte_limit = charged;
    assert!(
        !renderer.can_allocate(1, 1),
        "unsubmitted allocations remain charged"
    );
    drop(prepared);
    // Deterministic delayed-completion case: advance the submission watermark
    // without issuing a callback. No timing assumptions about the test adapter.
    renderer.preparing = false;
    renderer.submitted = MAX_PENDING_SUBMISSIONS;
    renderer.byte_limit = MAX_BYTES;
    assert!(renderer.prepare(&context.device, &[batch], size).is_empty());
    assert_eq!(
        renderer.upload_count, 1,
        "flight pressure cannot enqueue more uploads or callbacks"
    );
    drop(frame);
    assert_eq!(
        renderer
            .uploads
            .values()
            .next()
            .unwrap()
            .owner
            .strong_count(),
        0
    );
    assert_eq!(
        renderer.bytes, charged,
        "dropping CPU owners cannot free pending storage"
    );
    // Flush real mapped initialization before simulating completion delivery.
    let submission = context.queue.submit(std::iter::empty());
    context.device.poll(wgpu::PollType::Wait {
        submission_index: Some(submission),
        timeout: Some(std::time::Duration::from_secs(30)),
    })?;
    renderer
        .completed
        .store(MAX_PENDING_SUBMISSIONS, Ordering::Release);
    renderer.prepare(&context.device, &[], size);
    assert_eq!(renderer.bytes, 0);
    assert!(renderer.uploads.is_empty());
    assert!(renderer.placements.is_empty());
    assert!(renderer.palettes.is_empty());
    let completion = Arc::downgrade(&renderer.completed);
    drop(renderer);
    assert!(
        completion.upgrade().is_none(),
        "renderer destruction releases lifecycle ownership"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn pipelines_follow_admitted_variants_and_reuse_on_replay() -> anyhow::Result<()> {
    let (context, format) = crate::WgpuContext::new_headless()?;
    let device = &context.device;
    let mut renderer = MaterialRenderer::new(device, format, wgpu::CompositeAlphaMode::Opaque, 1);
    let limits = GpuLimits {
        max_field_width: 1,
        max_field_height: 1,
        max_field_cells: 1,
        max_path_vertices: 3,
        ..Default::default()
    };
    let palette = GpuPalette::new(&[[1.0; 4]], 1)?;
    let field = GpuFrame::field(
        1,
        1,
        &[0.0],
        palette.clone(),
        gpui::GpuFieldParams {
            mapping: GpuValueMapping::Linear { min: 0.0, max: 1.0 },
        },
        limits,
    )?;
    let state = gpui::GpuState::new(field.clone(), limits)?;
    state.clear();
    let empty = GpuFrame::rectangles(&[], palette.clone(), GpuRectangleParams::default(), limits)?;
    let point = |x, y| gpui::point(gpui::px(x), gpui::px(y));
    let mut path = gpui::Path::new(point(2.0, 3.0));
    path.curve_to(point(6.0, 7.0), point(6.0, 3.0));
    let path = GpuFrame::path(
        Arc::new(path),
        palette,
        gpui::GpuPathParams {
            palette_range: [0.0, 1.0],
        },
        limits,
    )?;
    let bounds = gpui::Bounds {
        origin: gpui::point(0.0.into(), 0.0.into()),
        size: gpui::size(8.0.into(), 8.0.into()),
    };
    let field_batch = GpuDraw {
        order: 0,
        bounds,
        content_mask: gpui::ContentMask { bounds },
        opacity: 1.0,
        scale_factor: 1.0,
        source: GpuSource::Fixed(field),
    };
    let path_batch = GpuDraw {
        source: GpuSource::Fixed(path),
        ..field_batch.clone()
    };
    let size = Size {
        width: DevicePixels(8),
        height: DevicePixels(8),
    };
    assert!(renderer.prepare(device, &[], size).is_empty());
    let cleared = GpuDraw {
        source: GpuSource::Dynamic(state),
        ..field_batch.clone()
    };
    let empty = GpuDraw {
        source: GpuSource::Fixed(empty),
        ..field_batch.clone()
    };
    assert!(renderer.prepare(device, &[cleared, empty], size).is_empty());
    assert_eq!(renderer.pipeline_counts, [0, 0, 0, 0]);
    let invalid = GpuDraw {
        scale_factor: f32::NAN,
        ..field_batch.clone()
    };
    assert!(renderer.prepare(device, &[invalid], size).is_empty());
    renderer.byte_limit = 0;
    assert!(
        renderer
            .prepare(device, &[field_batch.clone(), path_batch.clone()], size)
            .is_empty()
    );
    assert_eq!(renderer.pipeline_counts, [0, 0, 0, 0]);
    assert_eq!(renderer.upload_count, 0);
    renderer.byte_limit = MAX_BYTES;
    assert_eq!(
        renderer.prepare(device, &[path_batch.clone()], size).len(),
        1
    );
    assert_eq!(
        renderer.pipeline_counts,
        [0, 0, 1, 0],
        "path demand must not compile ordinary pipeline"
    );
    context.queue.submit(std::iter::empty());
    renderer.submitted(&context.queue);
    // A rejected ordinary record cannot affect the already usable path variant.
    renderer.byte_limit = 0;
    assert!(
        renderer
            .prepare(device, &[field_batch.clone()], size)
            .is_empty()
    );
    assert_eq!(renderer.pipeline_counts, [0, 0, 1, 0]);
    renderer.byte_limit = MAX_BYTES;
    let both = [path_batch, field_batch];
    assert_eq!(renderer.prepare(device, &both, size).len(), 2);
    assert_eq!(renderer.pipeline_counts, [0, 1, 1, 0]);
    context.queue.submit(std::iter::empty());
    renderer.submitted(&context.queue);
    assert_eq!(renderer.prepare(device, &both, size).len(), 2);
    assert_eq!(
        renderer.pipeline_counts,
        [0, 1, 1, 0],
        "replay must reuse both variants"
    );
    context.queue.submit(std::iter::empty());
    renderer.submitted(&context.queue);
    renderer.disable_program();
    assert!(renderer.prepare(device, &both, size).is_empty());
    assert_eq!(
        renderer.pipeline_counts,
        [0, 1, 1, 0],
        "disabled programs must not be recreated"
    );
    Ok(())
}

#[test]
fn field_and_path_encoders_preserve_bits_dimensions_mapping_and_geometry() -> anyhow::Result<()> {
    use gpui::{GpuFieldParams, GpuPathParams};
    let limits = GpuLimits {
        max_field_width: 2,
        max_field_height: 1,
        max_field_cells: 2,
        max_path_vertices: 3,
        max_palette_colors: 1,
        ..Default::default()
    };
    let palette = GpuPalette::new(&[[1.0; 4]], 1)?;
    let values = [f32::from_bits(0x7fc01234), -3.0];
    let field = GpuFrame::field(
        2,
        1,
        &values,
        palette.clone(),
        GpuFieldParams {
            mapping: GpuValueMapping::Linear {
                min: -4.0,
                max: 4.0,
            },
        },
        limits,
    )?;
    let mut data = [0; 8];
    encode_frame(&mut wgpu::WriteOnly::from_mut(&mut data[..]), &field);
    assert_eq!(&data[..4], &0x7fc01234_u32.to_le_bytes());
    assert_eq!(&data[4..], &(-3.0_f32).to_le_bytes());
    let bounds = gpui::Bounds {
        origin: gpui::point(0.0.into(), 0.0.into()),
        size: gpui::size(16.0.into(), 8.0.into()),
    };
    let batch = GpuDraw {
        order: 0,
        bounds,
        content_mask: gpui::ContentMask { bounds },
        opacity: 0.5,
        scale_factor: 1.0,
        source: GpuSource::Fixed(field.clone()),
    };
    let mut placement = [0; PLACEMENT_SIZE as usize];
    encode_placement(
        &mut wgpu::WriteOnly::from_mut(&mut placement[..]),
        &batch,
        &field,
        Size {
            width: DevicePixels(16),
            height: DevicePixels(8),
        },
        false,
        true,
    );
    for (offset, value) in [(48, -4.0_f32), (52, 8.0)] {
        assert_eq!(&placement[offset..offset + 4], &value.to_le_bytes());
    }
    for (offset, value) in [
        (64, 1_u32),
        (68, 1),
        (72, 0),
        (76, 1),
        (80, 2),
        (84, 1),
        (88, 0),
        (92, 0),
    ] {
        assert_eq!(&placement[offset..offset + 4], &value.to_le_bytes());
    }
    assert!(placement[96..].iter().all(|byte| *byte == 0));
    let point = |x, y| gpui::point(gpui::px(x), gpui::px(y));
    let mut path = gpui::Path::new(point(2.0, 3.0));
    path.curve_to(point(6.0, 7.0), point(6.0, 3.0));
    let path = GpuFrame::path(
        Arc::new(path),
        palette,
        GpuPathParams {
            palette_range: [0.25, 0.75],
        },
        limits,
    )?;
    let mut data = [0; 48];
    encode_frame(&mut wgpu::WriteOnly::from_mut(&mut data[..]), &path);
    for (index, expected) in [
        2.0_f32, 3.0, 0.0, 0.0, 6.0, 3.0, 0.5, 0.0, 6.0, 7.0, 1.0, 1.0,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(&data[index * 4..index * 4 + 4], &expected.to_le_bytes());
    }
    placement.fill(0);
    encode_placement(
        &mut wgpu::WriteOnly::from_mut(&mut placement[..]),
        &batch,
        &path,
        Size {
            width: DevicePixels(16),
            height: DevicePixels(8),
        },
        false,
        false,
    );
    assert_eq!(&placement[64..68], &2_u32.to_le_bytes());
    for (offset, value) in [
        (48, 0.25_f32),
        (52, 0.75),
        (96, 2.0),
        (100, 3.0),
        (104, 4.0),
        (108, 4.0),
    ] {
        assert_eq!(&placement[offset..offset + 4], &value.to_le_bytes());
    }
    assert_eq!(draw_counts(&path), (1, 3, MaterialKind::Path));
    Ok(())
}
