use super::*;
use gpui::{GpuRectangle, GpuRectangleLimits, GpuRectangleParams};

#[test]
fn fixed_rectangle_shader_and_byte_abi_match_structurally() {
    use naga::{AddressSpace, ArraySize, ScalarKind, TypeInner, VectorSize};
    let module = naga::front::wgsl::parse_str(SHADER).expect("fixed shader parses");
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
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
    let (rectangle_handle, rectangle) = find("Rectangle");
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
        rectangle,
        RECTANGLE_STRIDE as u32,
        &[
            (0, ScalarKind::Float, Some(VectorSize::Quad)),
            (16, ScalarKind::Float, Some(VectorSize::Bi)),
            (24, ScalarKind::Float, Some(VectorSize::Bi)),
        ],
    );
    check(
        placement,
        PLACEMENT_SIZE as u32,
        &[
            (0, ScalarKind::Float, Some(VectorSize::Quad)),
            (16, ScalarKind::Float, Some(VectorSize::Quad)),
            (32, ScalarKind::Float, Some(VectorSize::Bi)),
            (40, ScalarKind::Float, None),
            (44, ScalarKind::Float, None),
            (48, ScalarKind::Float, None),
            (52, ScalarKind::Float, None),
            (56, ScalarKind::Uint, None),
            (60, ScalarKind::Uint, None),
            (64, ScalarKind::Uint, None),
            (68, ScalarKind::Uint, None),
            (72, ScalarKind::Uint, None),
            (76, ScalarKind::Uint, None),
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
                assert_eq!(base, rectangle_handle);
                assert_eq!(stride, RECTANGLE_STRIDE as u32);
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
            min_binding_size: NonZeroU64::new(80)
        }
    );
    assert_eq!(
        buffer_entry(0, false, RECTANGLE_STRIDE).ty,
        wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: NonZeroU64::new(32)
        }
    );
}

#[test]
fn fixed_rectangle_encoder_writes_little_endian_and_zero_padding() {
    let frame = GpuRectangleFrame::new(
        &[GpuRectangle {
            bounds: [0.125, 0.25, 0.5, 0.75],
            palette_range: [0.25, 1.0],
        }],
        &[[0.125, 0.25, 0.5, 1.0]],
        GpuRectangleParams {
            cell_height: 3.0,
            cell_gap: 2.0,
        },
        GpuRectangleLimits {
            max_rectangles: 1,
            max_palette_colors: 1,
        },
    )
    .unwrap();
    let mut encoded = [0; 80];
    encode_frame(&mut wgpu::WriteOnly::from_mut(&mut encoded[..]), &frame, 64);
    for (offset, value) in [
        (0, 0.125_f32),
        (4, 0.25),
        (8, 0.5),
        (12, 0.75),
        (16, 0.25),
        (20, 1.0),
        (64, 0.125),
        (68, 0.25),
        (72, 0.5),
        (76, 1.0),
    ] {
        assert_eq!(&encoded[offset..offset + 4], &value.to_le_bytes());
    }
    assert!(encoded[24..64].iter().all(|byte| *byte == 0));
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
    let batch = GpuRectangleBatch {
        order: 0,
        bounds,
        content_mask: gpui::ContentMask { bounds },
        opacity: 0.5,
        scale_factor: 2.0,
        source: GpuRectangleSource::Fixed(frame.clone()),
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
    assert_eq!(&encoded[56..60], &1_u32.to_le_bytes());
    assert_eq!(&encoded[60..64], &1_u32.to_le_bytes());
    assert!(encoded[64..].iter().all(|byte| *byte == 0));
}

#[cfg(target_os = "linux")]
#[test]
fn rectangle_retirement_charges_unsubmitted_and_inflight_allocations() -> anyhow::Result<()> {
    let (context, format) = crate::WgpuContext::new_headless()?;
    let mut renderer =
        RectangleRenderer::new(&context.device, format, wgpu::CompositeAlphaMode::Opaque);
    let frame = GpuRectangleFrame::new(
        &[GpuRectangle {
            bounds: [0.0, 0.0, 1.0, 1.0],
            palette_range: [0.0, 1.0],
        }],
        &[[1.0; 4]],
        GpuRectangleParams::default(),
        GpuRectangleLimits {
            max_rectangles: 1,
            max_palette_colors: 1,
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
    let batch = GpuRectangleBatch {
        order: 0,
        bounds,
        content_mask: gpui::ContentMask { bounds },
        opacity: 1.0,
        scale_factor: 1.0,
        source: GpuRectangleSource::Fixed(frame.clone()),
    };
    let size = Size {
        width: DevicePixels(8),
        height: DevicePixels(8),
    };
    let prepared = renderer.prepare(&context.device, &[batch.clone()], size);
    assert_eq!(prepared.len(), 1);
    assert!(renderer.preparing);
    let charged = renderer.bytes;
    assert!(charged > PLACEMENT_SIZE);
    renderer.byte_limit = charged;
    assert!(
        !renderer.can_allocate(1),
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
    let completion = Arc::downgrade(&renderer.completed);
    drop(renderer);
    assert!(
        completion.upgrade().is_none(),
        "renderer destruction releases lifecycle ownership"
    );
    Ok(())
}
