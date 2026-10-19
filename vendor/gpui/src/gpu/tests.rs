use super::*;
use crate::{point, size};

const LIMITS: GpuLimits = GpuLimits {
    max_rectangles: 2,
    max_waveform_columns: 2,
    max_field_width: 2,
    max_field_height: 2,
    max_field_cells: 4,
    max_path_vertices: 6,
    max_segments: 2,
    max_stroke_layers: 2,
    max_palette_colors: 2,
};
const RECTANGLE: GpuRectangle = GpuRectangle {
    bounds: [0.0, 0.0, 1.0, 1.0],
    palette_range: [0.0, 1.0],
};
const FIELD_PARAMS: GpuFieldParams = GpuFieldParams {
    mapping: GpuValueMapping::Linear { min: 0.0, max: 1.0 },
};

fn palette() -> Arc<GpuPalette> {
    GpuPalette::new(&[[0.0, 0.5, 1.0, 0.0], [1.0; 4]], 2).unwrap()
}

fn frame() -> Arc<GpuFrame> {
    GpuFrame::rectangles(
        &[RECTANGLE],
        palette(),
        GpuRectangleParams::default(),
        LIMITS,
    )
    .unwrap()
}

fn rectangle_content(frame: &GpuFrame) -> (&[GpuRectangle], GpuRectangleParams) {
    let GpuContent::Rectangles { rectangles, params } = frame.content() else {
        panic!("expected rectangle publication")
    };
    (rectangles, *params)
}

fn path() -> Arc<Path<Pixels>> {
    let mut path = Path::new(point(Pixels(0.0), Pixels(0.0)));
    path.push_triangle(
        (
            point(Pixels(0.0), Pixels(0.0)),
            point(Pixels(20.0), Pixels(0.0)),
            point(Pixels(0.0), Pixels(30.0)),
        ),
        (point(0.0, 0.0), point(0.5, 0.0), point(1.0, 1.0)),
    );
    Arc::new(path)
}

fn placement(source: GpuSource) -> Result<GpuDraw> {
    let bounds = Bounds::new(
        point(Pixels(3.0), Pixels(4.0)),
        size(Pixels(20.0), Pixels(30.0)),
    );
    let mask = ContentMask {
        bounds: Bounds::new(bounds.origin, size(Pixels(10.0), Pixels(15.0))),
    };
    GpuDraw::from_logical(bounds, mask, 0.375, 2.0, source)
}

#[test]
fn identities_are_unique_and_checked_without_recycling() {
    let first = frame();
    let first_id = first.id();
    let clone = first.clone();
    assert_eq!(clone.id(), first_id);
    drop(first);
    drop(clone);
    assert!(frame().id() > first_id);
    let colors = palette();
    assert_eq!(colors.clone().id(), colors.id());
    assert!(palette().id() > colors.id());
    let exhausted = AtomicU64::new(u64::MAX - 1);
    assert_eq!(next_id(&exhausted).unwrap(), u64::MAX - 1);
    assert!(next_id(&exhausted).is_err());
    assert!(next_id(&exhausted).is_err());
    assert_eq!(exhausted.load(Ordering::Relaxed), u64::MAX);
}

#[test]
fn palette_is_shared_across_schemas_and_parameter_only_publications() {
    let palette = palette();
    let rectangles = GpuFrame::rectangles(
        &[RECTANGLE],
        palette.clone(),
        GpuRectangleParams::default(),
        LIMITS,
    )
    .unwrap();
    let field = GpuFrame::field(1, 1, &[0.5], palette.clone(), FIELD_PARAMS, LIMITS).unwrap();
    let geometry = path();
    let path_frame = GpuFrame::path(
        geometry.clone(),
        palette.clone(),
        GpuPathParams::default(),
        LIMITS,
    )
    .unwrap();
    let updated = GpuFrame::path(
        geometry.clone(),
        palette.clone(),
        GpuPathParams {
            palette_range: [1.0, 0.0],
        },
        LIMITS,
    )
    .unwrap();
    for frame in [&rectangles, &field, &path_frame, &updated] {
        assert!(Arc::ptr_eq(frame.palette(), &palette));
    }
    assert_ne!(path_frame.id(), updated.id());
    for frame in [&path_frame, &updated] {
        let GpuContent::Path { path, .. } = frame.content() else {
            panic!("expected path")
        };
        assert!(Arc::ptr_eq(path, &geometry));
        assert!(Arc::ptr_eq(&path.vertices, &geometry.vertices));
    }
    assert_eq!(palette.colors()[0][3], 0.0);
}

#[test]
fn validation_rejects_invalid_palette_geometry_and_segmentation() {
    assert!(GpuPalette::new(&[], 2).is_err());
    assert!(GpuPalette::new(&[[0.0; 4]], 0).is_err());
    assert!(GpuPalette::new(&[[0.0; 4]; 3], 2).is_err());
    for value in [-0.1, 1.1, f32::NAN, f32::INFINITY] {
        assert!(GpuPalette::new(&[[value; 4]], 1).is_err());
        assert!(
            GpuFrame::rectangles(
                &[GpuRectangle {
                    palette_range: [value, 1.0],
                    ..RECTANGLE
                }],
                palette(),
                GpuRectangleParams::default(),
                LIMITS
            )
            .is_err()
        );
        assert!(
            GpuFrame::path(
                path(),
                palette(),
                GpuPathParams {
                    palette_range: [value, 1.0]
                },
                LIMITS
            )
            .is_err()
        );
    }
    for bounds in [
        [-0.1, 0.0, 1.0, 1.0],
        [0.0, 0.0, -0.1, 1.0],
        [0.5, 0.0, 0.6, 1.0],
        [0.0, 0.5, 1.0, 0.6],
        [0.0, 0.0, f32::INFINITY, 1.0],
        [f32::NAN, 0.0, 1.0, 1.0],
    ] {
        assert!(
            GpuFrame::rectangles(
                &[GpuRectangle {
                    bounds,
                    ..RECTANGLE
                }],
                palette(),
                GpuRectangleParams::default(),
                LIMITS
            )
            .is_err()
        );
    }
    for (cell_height, cell_gap) in [
        (-1.0, 0.0),
        (1.0, -1.0),
        (0.0, 1.0),
        (f32::NAN, 0.0),
        (1.0, f32::INFINITY),
        (f32::MAX, f32::MAX),
    ] {
        assert!(
            GpuFrame::rectangles(
                &[],
                palette(),
                GpuRectangleParams {
                    cell_height,
                    cell_gap
                },
                LIMITS
            )
            .is_err()
        );
    }
    assert!(
        GpuFrame::rectangles(
            &[RECTANGLE; 3],
            palette(),
            GpuRectangleParams::default(),
            LIMITS
        )
        .is_err()
    );
    assert!(
        GpuFrame::rectangles(
            &[],
            palette(),
            GpuRectangleParams::default(),
            GpuLimits {
                max_rectangles: 0,
                ..LIMITS
            }
        )
        .is_ok()
    );
    assert!(
        GpuFrame::rectangles(
            &[],
            palette(),
            GpuRectangleParams::default(),
            GpuLimits {
                max_palette_colors: 0,
                ..LIMITS
            }
        )
        .is_err()
    );
}

#[test]
fn field_dimensions_capacity_and_mapping_are_checked_as_one_publication() {
    for (width, height, values) in [
        (0, 2, &[][..]),
        (2, 0, &[][..]),
        (0, 0, &[][..]),
        (1, 2, &[0.0, 1.0][..]),
        (2, 1, &[0.0, 1.0][..]),
    ] {
        let frame =
            GpuFrame::field(width, height, values, palette(), FIELD_PARAMS, LIMITS).unwrap();
        let GpuContent::Field {
            width: w,
            height: h,
            values: cells,
            ..
        } = frame.content()
        else {
            panic!("expected field")
        };
        assert_eq!((*w, *h, cells.as_ref()), (width, height, values));
    }
    for (width, height, values) in [
        (0, 2, &[0.0][..]),
        (2, 2, &[0.0][..]),
        (3, 0, &[][..]),
        (1, 3, &[0.0; 3][..]),
    ] {
        assert!(GpuFrame::field(width, height, values, palette(), FIELD_PARAMS, LIMITS).is_err());
    }
    let enormous = GpuLimits {
        max_field_width: u32::MAX,
        max_field_height: u32::MAX,
        max_field_cells: usize::MAX,
        ..LIMITS
    };
    assert!(GpuFrame::field(u32::MAX, u32::MAX, &[], palette(), FIELD_PARAMS, enormous).is_err());
    assert!(
        GpuFrame::field(
            2,
            2,
            &[0.0; 4],
            palette(),
            FIELD_PARAMS,
            GpuLimits {
                max_field_cells: 3,
                ..LIMITS
            }
        )
        .is_err()
    );
    for mapping in [
        GpuValueMapping::Linear { min: 1.0, max: 1.0 },
        GpuValueMapping::Linear { min: 1.0, max: 0.0 },
        GpuValueMapping::Linear {
            min: f32::NAN,
            max: 1.0,
        },
        GpuValueMapping::Linear {
            min: 0.0,
            max: f32::INFINITY,
        },
        GpuValueMapping::Linear {
            min: -f32::MAX,
            max: f32::MAX,
        },
        GpuValueMapping::Logarithmic { db_range: 0.0 },
        GpuValueMapping::Logarithmic { db_range: -1.0 },
        GpuValueMapping::Logarithmic {
            db_range: f32::INFINITY,
        },
        GpuValueMapping::Logarithmic { db_range: f32::NAN },
    ] {
        assert!(GpuFrame::field(0, 0, &[], palette(), GpuFieldParams { mapping }, LIMITS).is_err());
    }
}

#[test]
fn path_geometry_is_an_immutable_snapshot_with_existing_coverage() {
    let mut geometry = path();
    let frame = GpuFrame::path(
        geometry.clone(),
        palette(),
        GpuPathParams::default(),
        LIMITS,
    )
    .unwrap();
    let GpuContent::Path { path: retained, .. } = frame.content() else {
        panic!("expected path")
    };
    assert!(Arc::ptr_eq(retained, &geometry));
    assert_eq!(retained.vertices[1].st_position, point(0.5, 0.0));
    Arc::make_mut(&mut Arc::make_mut(&mut geometry).vertices)[1].st_position = point(0.0, 1.0);
    assert_eq!(retained.vertices[1].st_position, point(0.5, 0.0));
    assert!(!Arc::ptr_eq(&retained.vertices, &geometry.vertices));
    let mut invalid = path();
    Arc::make_mut(&mut Arc::make_mut(&mut invalid).vertices).pop();
    assert!(GpuFrame::path(invalid, palette(), GpuPathParams::default(), LIMITS).is_err());
    let mut invalid = path();
    Arc::make_mut(&mut Arc::make_mut(&mut invalid).vertices)[0]
        .xy_position
        .x = Pixels(f32::NAN);
    assert!(GpuFrame::path(invalid, palette(), GpuPathParams::default(), LIMITS).is_err());
    let mut invalid = path();
    Arc::make_mut(&mut Arc::make_mut(&mut invalid).vertices)[0]
        .st_position
        .x = f32::INFINITY;
    assert!(GpuFrame::path(invalid, palette(), GpuPathParams::default(), LIMITS).is_err());
    assert!(
        GpuFrame::path(
            path(),
            palette(),
            GpuPathParams::default(),
            GpuLimits {
                max_path_vertices: 2,
                ..LIMITS
            }
        )
        .is_err()
    );
    let empty = Arc::new(Path::new(point(Pixels(0.0), Pixels(0.0))));
    assert!(GpuFrame::path(empty, palette(), GpuPathParams::default(), LIMITS).is_ok());
}

#[test]
fn dynamic_schema_and_capacities_survive_clear_and_rejected_publications() {
    let initial = frame();
    let state = GpuState::new(initial.clone(), LIMITS).unwrap();
    let field = GpuFrame::field(1, 1, &[0.0], palette(), FIELD_PARAMS, LIMITS).unwrap();
    let oversized = GpuFrame::rectangles(
        &[RECTANGLE; 3],
        palette(),
        GpuRectangleParams::default(),
        GpuLimits {
            max_rectangles: 3,
            ..LIMITS
        },
    )
    .unwrap();
    let oversized_palette = GpuFrame::rectangles(
        &[],
        GpuPalette::new(&[[1.0; 4]; 3], 3).unwrap(),
        GpuRectangleParams::default(),
        GpuLimits {
            max_palette_colors: 3,
            ..LIMITS
        },
    )
    .unwrap();
    for rejected in [&field, &oversized, &oversized_palette] {
        assert!(state.publish(rejected.clone()).is_err());
        assert!(Arc::ptr_eq(&initial, &state.snapshot().unwrap()));
    }
    assert!(
        GpuState::new(
            initial.clone(),
            GpuLimits {
                max_rectangles: 0,
                ..LIMITS
            }
        )
        .is_err()
    );
    state.clear();
    assert_eq!(state.schema(), GpuSchema::Rectangles);
    assert!(state.publish(field).is_err());
    assert!(state.snapshot().is_none());
    state.publish(initial.clone()).unwrap();
    assert!(Arc::ptr_eq(&initial, &state.snapshot().unwrap()));
    let field = GpuFrame::field(1, 1, &[0.0], palette(), FIELD_PARAMS, LIMITS).unwrap();
    let field_state = GpuState::new(
        field.clone(),
        GpuLimits {
            max_field_cells: 1,
            ..LIMITS
        },
    )
    .unwrap();
    let wider = GpuFrame::field(2, 1, &[0.0; 2], palette(), FIELD_PARAMS, LIMITS).unwrap();
    assert!(field_state.publish(wider).is_err());
    assert!(Arc::ptr_eq(&field, &field_state.snapshot().unwrap()));
}

#[test]
fn raw_updates_use_slot_limits_and_preserve_failed_or_cleared_state() {
    let initial = frame();
    let rectangles = GpuState::new(initial.clone(), LIMITS).unwrap();
    assert!(
        rectangles
            .publish_rectangles(&[RECTANGLE; 3], palette(), GpuRectangleParams::default())
            .is_err()
    );
    assert!(
        rectangles
            .publish_field(1, 1, &[0.5], palette(), FIELD_PARAMS)
            .is_err()
    );
    assert!(Arc::ptr_eq(&initial, &rectangles.snapshot().unwrap()));
    rectangles.clear();
    assert!(
        rectangles
            .publish_rectangles(&[RECTANGLE; 3], palette(), GpuRectangleParams::default())
            .is_err()
    );
    assert!(rectangles.snapshot().is_none());
    rectangles
        .publish_rectangles(&[RECTANGLE; 2], palette(), GpuRectangleParams::default())
        .unwrap();
    let restored = rectangles.snapshot().unwrap();
    let GpuContent::Rectangles {
        rectangles: restored,
        ..
    } = restored.content()
    else {
        panic!("expected rectangles")
    };
    assert_eq!(restored.len(), 2);

    let initial = GpuFrame::field(1, 1, &[0.25], palette(), FIELD_PARAMS, LIMITS).unwrap();
    let field = GpuState::new(
        initial.clone(),
        GpuLimits {
            max_field_cells: 1,
            ..LIMITS
        },
    )
    .unwrap();
    assert!(
        field
            .publish_field(2, 1, &[0.5; 2], palette(), FIELD_PARAMS)
            .is_err()
    );
    assert!(
        field
            .publish_rectangles(&[], palette(), GpuRectangleParams::default())
            .is_err()
    );
    assert!(Arc::ptr_eq(&initial, &field.snapshot().unwrap()));
    field.clear();
    assert!(
        field
            .publish_field(
                1,
                1,
                &[0.5],
                palette(),
                GpuFieldParams {
                    mapping: GpuValueMapping::Linear { min: 1.0, max: 1.0 }
                }
            )
            .is_err()
    );
    assert!(field.snapshot().is_none());
    field
        .publish_field(1, 1, &[0.75], palette(), FIELD_PARAMS)
        .unwrap();
    let current = field.snapshot().unwrap();
    let GpuContent::Field { values, .. } = current.content() else {
        panic!("expected field")
    };
    assert_eq!(values.as_ref(), &[0.75]);
    let GpuContent::Field { values, .. } = initial.content() else {
        panic!("expected field")
    };
    assert_eq!(values.as_ref(), &[0.25]);
}

#[test]
fn concurrent_snapshots_never_mix_dimensions_values_palette_or_parameters() {
    let initial = GpuFrame::field(
        1,
        1,
        &[1.0],
        GpuPalette::new(&[[0.5; 4]], 1).unwrap(),
        FIELD_PARAMS,
        LIMITS,
    )
    .unwrap();
    let state = GpuState::new(initial, LIMITS).unwrap();
    std::thread::scope(|scope| {
        let writer = &state;
        scope.spawn(move || {
            for revision in 0..2000 {
                let width = revision % 2 + 1;
                writer
                    .publish_field(
                        width,
                        1,
                        &[width as f32; 2][..width as usize],
                        GpuPalette::new(&[[width as f32 / 2.0; 4]], 1).unwrap(),
                        GpuFieldParams {
                            mapping: GpuValueMapping::Linear {
                                min: 0.0,
                                max: width as f32,
                            },
                        },
                    )
                    .unwrap();
            }
        });
        for _ in 0..2000 {
            let snapshot = state.snapshot().unwrap();
            let GpuContent::Field {
                width,
                height,
                values,
                params,
                ..
            } = snapshot.content()
            else {
                panic!("expected field")
            };
            assert_eq!(*height, 1);
            assert_eq!(values.len(), *width as usize);
            assert!(values.iter().all(|v| *v == *width as f32));
            assert_eq!(snapshot.palette().colors()[0][0] * 2.0, *width as f32);
            assert_eq!(
                params.mapping,
                GpuValueMapping::Linear {
                    min: 0.0,
                    max: *width as f32
                }
            );
        }
    });
}

#[test]
fn clear_releases_content_but_preserves_captured_frames() {
    let initial = frame();
    let weak = Arc::downgrade(&initial);
    let palette_weak = Arc::downgrade(initial.palette());
    let state = GpuState::new(initial, LIMITS).unwrap();
    let captured = state.snapshot().unwrap();
    state.clear();
    assert!(state.snapshot().is_none());
    assert!(weak.upgrade().is_some());
    assert!(palette_weak.upgrade().is_some());
    drop(captured);
    assert!(weak.upgrade().is_none());
    assert!(palette_weak.upgrade().is_none());
    state.publish(frame()).unwrap();
    assert!(state.snapshot().is_some());
}

#[test]
fn column_footprints_include_minimum_width_without_changing_mapping() {
    let ordinary = GpuFrame::field(1, 1, &[1.0], palette(), FIELD_PARAMS, LIMITS).unwrap();
    let columns =
        GpuFrame::field_columns(&[[0.9, 1.0]], 1, &[1.0], palette(), FIELD_PARAMS, LIMITS).unwrap();
    let mapping = Bounds::new(
        point(ScaledPixels(0.0), ScaledPixels(0.0)),
        size(ScaledPixels(4.0), ScaledPixels(4.0)),
    );
    let mask = Bounds::new(
        point(ScaledPixels(4.0), ScaledPixels(0.0)),
        size(ScaledPixels(1.0), ScaledPixels(4.0)),
    );
    let mut draw = GpuDraw {
        order: 0,
        bounds: mapping,
        content_mask: ContentMask { bounds: mask },
        opacity: 1.0,
        scale_factor: 1.0,
        source: GpuSource::Fixed(columns.clone()),
    };
    let visible = draw.visible_bounds();
    assert_eq!(visible.origin, mask.origin);
    assert!((visible.size.width.0 - 0.6).abs() < 0.00001);
    assert_eq!(visible.size.height, ScaledPixels(4.0));
    assert_eq!(visible, draw.visible_bounds_for(&columns));
    assert_eq!(draw.bounds, mapping);

    let state = GpuState::new(ordinary.clone(), LIMITS).unwrap();
    draw.source = GpuSource::Dynamic(state.clone());
    // Scene bounds cannot depend on the currently published Field representation.
    assert_eq!(draw.visible_bounds(), mask);
    assert_eq!(
        draw.visible_bounds_for(&ordinary).size.width,
        ScaledPixels(0.0)
    );
    for publication in [columns.clone(), ordinary.clone(), columns.clone()] {
        state.publish(publication.clone()).unwrap();
        assert_eq!(draw.visible_bounds(), mask);
        let captured = state.snapshot().unwrap();
        assert_eq!(
            draw.visible_bounds_for(&captured),
            draw.visible_bounds_for(&publication)
        );
    }
    state.clear();
    assert_eq!(draw.visible_bounds(), mask);
    // Already captured publications keep their own footprint after clear.
    assert_eq!(draw.visible_bounds_for(&columns), visible);
    draw.scale_factor = 2.0;
    assert_eq!(draw.visible_bounds_for(&columns), mask);
    assert_eq!(draw.bounds, mapping);
}

#[test]
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn native_backend_rejects_gpu_placements() {
    assert!(placement(GpuSource::Fixed(frame())).is_err());
}

#[test]
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn all_geometries_capture_placement_clip_opacity_without_copying_content() {
    for frame in [
        frame(),
        GpuFrame::field(1, 1, &[0.5], palette(), FIELD_PARAMS, LIMITS).unwrap(),
        GpuFrame::path(path(), palette(), GpuPathParams::default(), LIMITS).unwrap(),
    ] {
        let draw = placement(GpuSource::Fixed(frame.clone())).unwrap();
        assert_eq!(
            draw.bounds.origin,
            point(ScaledPixels(6.0), ScaledPixels(8.0))
        );
        assert_eq!(
            draw.bounds.size,
            size(ScaledPixels(40.0), ScaledPixels(60.0))
        );
        assert_eq!(
            draw.content_mask.bounds.size,
            size(ScaledPixels(20.0), ScaledPixels(30.0))
        );
        assert_eq!(draw.opacity, 0.375);
        assert_eq!(draw.scale_factor, 2.0);
        assert!(
            matches!(&draw.source, GpuSource::Fixed(recorded) if Arc::ptr_eq(recorded, &frame))
        );
    }
}

#[test]
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn recording_rejects_invalid_placement_clip_opacity_and_scale() {
    let bounds = Bounds::new(
        point(Pixels(0.0), Pixels(0.0)),
        size(Pixels(8.0), Pixels(8.0)),
    );
    let source = GpuSource::Fixed(frame());
    for field in 0..4 {
        for invalid in [f32::NAN, f32::INFINITY, f32::MAX] {
            let mut values = [0.0, 0.0, 8.0, 8.0];
            values[field] = invalid;
            let invalid = Bounds::new(
                point(Pixels(values[0]), Pixels(values[1])),
                size(Pixels(values[2]), Pixels(values[3])),
            );
            assert!(
                GpuDraw::from_logical(
                    bounds,
                    ContentMask { bounds: invalid },
                    1.0,
                    2.0,
                    source.clone()
                )
                .is_err()
            );
            assert!(
                GpuDraw::from_logical(invalid, ContentMask { bounds }, 1.0, 2.0, source.clone())
                    .is_err()
            );
        }
    }
    for dimensions in [
        size(Pixels(-1.0), Pixels(8.0)),
        size(Pixels(8.0), Pixels(-1.0)),
    ] {
        let invalid = Bounds::new(bounds.origin, dimensions);
        assert!(
            GpuDraw::from_logical(
                bounds,
                ContentMask { bounds: invalid },
                1.0,
                1.0,
                source.clone()
            )
            .is_err()
        );
        assert!(
            GpuDraw::from_logical(invalid, ContentMask { bounds }, 1.0, 1.0, source.clone())
                .is_err()
        );
    }
    for opacity in [-1.0, 1.1, f32::NAN, f32::INFINITY] {
        assert!(
            GpuDraw::from_logical(bounds, ContentMask { bounds }, opacity, 1.0, source.clone())
                .is_err()
        );
    }
    for scale in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert!(
            GpuDraw::from_logical(bounds, ContentMask { bounds }, 1.0, scale, source.clone())
                .is_err()
        );
    }
}

#[test]
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn replay_keeps_fixed_content_but_resolves_dynamic_publication_and_revocation() {
    use crate::{PrimitiveBatch, Scene};
    let initial = frame();
    let initial_weak = Arc::downgrade(&initial);
    let state = GpuState::new(initial.clone(), LIMITS).unwrap();
    let fixed = placement(GpuSource::Fixed(initial)).unwrap();
    let dynamic = placement(GpuSource::Dynamic(state.clone())).unwrap();
    let bounds = fixed.bounds;
    let mut source = Scene::default();
    source.push_layer(bounds);
    source.push_layer(bounds);
    source.insert_primitive(fixed);
    source.pop_layer();
    source.insert_primitive(dynamic);
    source.pop_layer();
    source.finish();
    assert!(matches!(source.gpu_draws[0].source, GpuSource::Dynamic(_)));
    assert!(matches!(source.gpu_draws[1].source, GpuSource::Fixed(_)));
    state
        .publish(
            GpuFrame::rectangles(
                &[RECTANGLE; 2],
                palette(),
                GpuRectangleParams {
                    cell_height: 3.0,
                    cell_gap: 1.0,
                },
                LIMITS,
            )
            .unwrap(),
        )
        .unwrap();
    let published = state.snapshot().unwrap();
    let published_weak = Arc::downgrade(&published);
    let mut replayed = Scene::default();
    replayed.replay(0..source.len(), &source);
    replayed.finish();
    let batches: Vec<_> = replayed.batches().collect();
    assert!(matches!(&batches[..], [PrimitiveBatch::Gpu(range)] if *range == (0..2)));
    assert_eq!(replayed.gpu_draws[0].opacity, 0.375);
    assert_eq!(replayed.gpu_draws[0].bounds, bounds);
    assert_eq!(replayed.gpu_draws[0].scale_factor, 2.0);
    let GpuSource::Dynamic(slot) = &replayed.gpu_draws[0].source else {
        panic!("expected dynamic")
    };
    assert!(Arc::ptr_eq(&slot.snapshot().unwrap(), &published));
    let GpuSource::Fixed(recorded) = &replayed.gpu_draws[1].source else {
        panic!("expected fixed")
    };
    let (rectangles, params) = rectangle_content(recorded);
    assert_eq!(rectangles.len(), 1);
    assert_eq!(params.cell_height, 0.0);
    let mut partial = Scene::default();
    partial.replay(2..3, &source);
    assert!(matches!(partial.gpu_draws[0].source, GpuSource::Fixed(_)));
    source.clear();
    state.clear();
    drop(published);
    assert!(published_weak.upgrade().is_none());
    assert!(slot.snapshot().is_none());
    assert!(initial_weak.upgrade().is_some());
    replayed.clear();
    assert!(initial_weak.upgrade().is_some());
    partial.clear();
    assert!(initial_weak.upgrade().is_none());
    assert!(replayed.gpu_draws.is_empty());
    assert_eq!(replayed.batches().count(), 0);
}

#[test]
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn mixed_gpu_geometries_preserve_overlap_order_and_clipping() {
    use crate::{PrimitiveBatch, Quad, Scene};
    let rectangle = placement(GpuSource::Fixed(frame())).unwrap();
    let field = placement(GpuSource::Fixed(
        GpuFrame::field(1, 1, &[0.5], palette(), FIELD_PARAMS, LIMITS).unwrap(),
    ))
    .unwrap();
    let path = placement(GpuSource::Fixed(
        GpuFrame::path(path(), palette(), GpuPathParams::default(), LIMITS).unwrap(),
    ))
    .unwrap();
    let mut scene = Scene::default();
    scene.insert_primitive(rectangle.clone());
    scene.insert_primitive(Quad {
        bounds: rectangle.bounds,
        content_mask: rectangle.content_mask,
        ..Quad::default()
    });
    scene.insert_primitive(field);
    scene.insert_primitive(path);
    let mut clipped = rectangle;
    clipped.content_mask.bounds.origin.x = ScaledPixels(1000.0);
    scene.insert_primitive(clipped);
    scene.finish();
    assert_eq!(scene.len(), 4);
    let batches: Vec<_> = scene.batches().collect();
    assert!(
        matches!(&batches[..], [PrimitiveBatch::Gpu(a), PrimitiveBatch::Quads(b), PrimitiveBatch::Gpu(c)]
        if *a == (0..1) && *b == (0..1) && *c == (1..3))
    );
}

#[test]
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn segment_footprints_preserve_mapping_and_dynamic_publication_envelopes() {
    use crate::{PrimitiveBatch, Quad, Scene};
    let make_frame = |endpoints, width| {
        GpuFrame::segments(
            &[GpuSegment {
                endpoints,
                color: 0,
            }],
            &[GpuStrokeLayer {
                width,
                palette_offset: 0,
            }],
            palette(),
            LIMITS,
        )
        .unwrap()
    };
    let mapping = Bounds::new(
        point(Pixels(16.0), Pixels(16.0)),
        size(Pixels(16.0), Pixels(16.0)),
    );
    let full_mask = ContentMask {
        bounds: Bounds::new(
            point(Pixels(0.0), Pixels(0.0)),
            size(Pixels(64.0), Pixels(64.0)),
        ),
    };
    let outside = make_frame([-0.5, 0.5, 1.5, 0.5], 4.0);
    let draw =
        GpuDraw::from_logical(mapping, full_mask, 1.0, 1.0, GpuSource::Fixed(outside)).unwrap();
    assert_eq!(draw.bounds, mapping.scale(1.0));
    assert!(draw.visible_bounds().origin.x.0 <= 8.0);
    assert!(draw.visible_bounds().right().0 >= 40.0);
    let mut scene = Scene::default();
    scene.insert_primitive(draw);
    scene.insert_primitive(Quad {
        bounds: Bounds::new(
            point(ScaledPixels(8.0), ScaledPixels(22.0)),
            size(ScaledPixels(4.0), ScaledPixels(4.0)),
        ),
        content_mask: full_mask.scale(1.0),
        ..Quad::default()
    });
    scene.finish();
    assert!(matches!(
        &scene.batches().collect::<Vec<_>>()[..],
        [PrimitiveBatch::Gpu(_), PrimitiveBatch::Quads(_)]
    ));

    // The mapping does not intersect the mask, but a wide edge stroke does.
    let edge = make_frame([0.0, 0.0, 0.0, 1.0], 8.0);
    let clip = ContentMask {
        bounds: Bounds::new(
            point(Pixels(12.0), Pixels(18.0)),
            size(Pixels(2.0), Pixels(8.0)),
        ),
    };
    let edge = GpuDraw::from_logical(mapping, clip, 1.0, 2.0, GpuSource::Fixed(edge)).unwrap();
    let mut scene = Scene::default();
    scene.insert_primitive(edge);
    assert_eq!(scene.gpu_draws.len(), 1);

    let state = GpuState::new(make_frame([0.0, 0.5, 1.0, 0.5], 1.0), LIMITS).unwrap();
    let dynamic =
        GpuDraw::from_logical(mapping, clip, 1.0, 1.0, GpuSource::Dynamic(state.clone())).unwrap();
    assert_eq!(dynamic.visible_bounds(), clip.scale(1.0).bounds);
    state
        .publish(make_frame([-0.5, 0.5, 1.5, 0.5], 20.0))
        .unwrap();
    let snapshot = state.snapshot().unwrap();
    let captured = dynamic.visible_bounds_for(&snapshot);
    state.clear();
    assert_eq!(dynamic.visible_bounds_for(&snapshot), captured);
    assert!(!captured.is_empty());
}

#[test]
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn scene_detach_releases_fixed_cpu_payloads_without_clearing_other_primitives() {
    use crate::{Quad, Scene};
    let initial = frame();
    let weak = Arc::downgrade(&initial);
    let mut first = Scene::default();
    first.insert_primitive(placement(GpuSource::Fixed(initial)).unwrap());
    first.insert_primitive(Quad {
        bounds: first.gpu_draws[0].bounds,
        content_mask: first.gpu_draws[0].content_mask,
        ..Quad::default()
    });
    let mut second = Scene::default();
    second.replay(0..first.len(), &first);
    let operation_count = first.len();
    // A later normal paint of B must not retarget the cached replay of A.
    let ordinary = first.quads[0];
    first.clear();
    let replacement = frame();
    let replacement_weak = Arc::downgrade(&replacement);
    first.insert_primitive(placement(GpuSource::Fixed(replacement)).unwrap());
    first.insert_primitive(ordinary);
    let GpuSource::Fixed(recorded_a) = &second.gpu_draws[0].source else {
        panic!("fixed A")
    };
    let GpuSource::Fixed(recorded_b) = &first.gpu_draws[0].source else {
        panic!("fixed B")
    };
    assert_ne!(recorded_a.id(), recorded_b.id());
    first.detach_gpu_draws();
    assert!(replacement_weak.upgrade().is_none());
    assert!(weak.upgrade().is_some());
    second.detach_gpu_draws();
    assert!(weak.upgrade().is_none());
    assert_eq!(first.len(), operation_count);
    assert_eq!(second.len(), operation_count);
    assert_eq!(first.quads.len(), 1);
    assert_eq!(second.quads.len(), 1);
    first.detach_gpu_draws();
    second.detach_gpu_draws();
}

#[crate::test]
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn hidden_window_detaches_cached_fixed_scenes_without_another_frame(
    cx: &mut crate::TestAppContext,
) {
    use crate::{
        AppContext, Context, Entity, IntoElement, Render, StyleRefinement, Window,
        WindowVisibility, canvas, div, prelude::*,
    };
    use std::{cell::RefCell, rc::Rc, sync::Weak};

    struct Panel(Rc<RefCell<Vec<Weak<GpuFrame>>>>);
    impl Render for Panel {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let publications = self.0.clone();
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let publication = frame();
                    publications.borrow_mut().push(Arc::downgrade(&publication));
                    window
                        .paint_gpu(bounds, GpuSource::Fixed(publication))
                        .unwrap();
                },
            )
            .size_full()
        }
    }
    struct Root(Entity<Panel>);
    impl Render for Root {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                self.0
                    .clone()
                    .cached(StyleRefinement::default().size_full()),
            )
        }
    }
    let publications = Rc::new(RefCell::new(Vec::new()));
    let window = cx.open_window(size(Pixels(64.0), Pixels(64.0)), {
        let publications = publications.clone();
        move |_, cx| Root(cx.new(|_| Panel(publications)))
    });
    cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    let paints_before_replay = publications.borrow().len();
    assert!(paints_before_replay > 0);
    window.update(cx, |_, _, cx| cx.notify()).unwrap();
    cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    assert_eq!(publications.borrow().len(), paints_before_replay);
    assert!(publications.borrow().last().unwrap().upgrade().is_some());
    let platform = cx.test_window(window.into());
    let wakes = platform.frame_wake_count();
    platform.simulate_visibility_change(WindowVisibility::Hidden);
    // No draw, present, or executor drain occurs between hide and this check.
    assert!(
        publications
            .borrow()
            .iter()
            .all(|frame| frame.upgrade().is_none())
    );
    assert_eq!(platform.frame_wake_count(), wakes);
    platform.simulate_visibility_change(WindowVisibility::Visible);
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert_eq!(window.rendered_frame.scene.gpu_draws.len(), 1);
    })
    .unwrap();
    assert!(publications.borrow().len() > paints_before_replay);
    assert!(publications.borrow().last().unwrap().upgrade().is_some());
}

#[test]
fn waveform_publications_validate_levels_and_keep_dynamic_snapshots_coherent() {
    let samples = [GpuWaveformColumn {
        levels: [0.0, 1.0, f32::NAN, 0.5],
    }];
    let initial = GpuFrame::waveform(
        samples.to_vec(),
        palette(),
        GpuWaveformParams::default(),
        LIMITS,
    )
    .unwrap();
    assert_eq!(initial.schema(), GpuSchema::Waveform);
    let state = GpuState::new(initial.clone(), LIMITS).unwrap();
    for level in [-0.01, 1.01, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(
            GpuFrame::waveform(
                vec![GpuWaveformColumn { levels: [level; 4] }],
                palette(),
                GpuWaveformParams::default(),
                LIMITS
            )
            .is_err()
        );
    }
    assert!(
        GpuFrame::waveform(
            vec![samples[0]; 3],
            palette(),
            GpuWaveformParams::default(),
            LIMITS
        )
        .is_err()
    );
    assert!(
        GpuFrame::waveform(
            Vec::new(),
            palette(),
            GpuWaveformParams::default(),
            GpuLimits {
                max_waveform_columns: 0,
                ..LIMITS
            }
        )
        .is_ok()
    );
    assert!(
        GpuFrame::waveform(
            samples.to_vec(),
            GpuPalette::new(&[[1.0; 4]], 1).unwrap(),
            GpuWaveformParams::default(),
            LIMITS
        )
        .is_err()
    );
    for palette_start in [f32::NAN, f32::INFINITY, -0.01, 1.01] {
        assert!(
            GpuFrame::waveform(
                samples.to_vec(),
                palette(),
                GpuWaveformParams { palette_start },
                LIMITS
            )
            .is_err()
        );
    }
    let oversized = GpuFrame::waveform(
        vec![samples[0]; 3],
        palette(),
        GpuWaveformParams::default(),
        GpuLimits {
            max_waveform_columns: 3,
            ..LIMITS
        },
    )
    .unwrap();
    assert!(state.publish(oversized).is_err());
    assert!(state.publish(frame()).is_err());
    assert!(Arc::ptr_eq(&state.snapshot().unwrap(), &initial));
    let captured = state.snapshot().unwrap();
    state.clear();
    assert_eq!(state.schema(), GpuSchema::Waveform);
    assert!(state.snapshot().is_none());
    let next = GpuFrame::waveform(
        vec![GpuWaveformColumn { levels: [0.25; 4] }],
        palette(),
        GpuWaveformParams {
            palette_start: 0.06,
        },
        LIMITS,
    )
    .unwrap();
    assert_ne!(initial.id(), next.id());
    state.publish(next.clone()).unwrap();
    assert!(Arc::ptr_eq(&state.snapshot().unwrap(), &next));
    let GpuContent::Waveform { columns, params } = captured.content() else {
        panic!("expected waveform")
    };
    assert_eq!(columns[0].levels[0], 0.0);
    assert!(columns[0].levels[2].is_nan());
    assert_eq!(params.palette_start, 0.0);
    assert!(Arc::ptr_eq(captured.palette(), initial.palette()));
}
