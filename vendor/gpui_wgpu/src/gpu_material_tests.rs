use super::*;
use gpui::{
    ContentMask, GpuDraw, GpuFieldParams, GpuFrame, GpuLimits, GpuPalette, GpuPathParams,
    GpuRectangle, GpuRectangleParams, GpuSource, GpuState, GpuValueMapping,
    PlatformHeadlessRenderer,
};

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const BLACK: [u8; 4] = [0, 0, 0, 255];
fn limits() -> GpuLimits {
    GpuLimits {
        max_rectangles: 1024,
        max_palette_colors: 16,
        max_field_width: 1024,
        max_field_height: 1024,
        max_field_cells: 1024 * 1024,
        max_path_vertices: 4096,
        max_segments: 2208,
        max_stroke_layers: 3,
    }
}
fn size(width: i32, height: i32) -> Size<DevicePixels> {
    Size {
        width: DevicePixels(width),
        height: DevicePixels(height),
    }
}
fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
    Bounds {
        origin: Point {
            x: x.into(),
            y: y.into(),
        },
        size: Size {
            width: width.into(),
            height: height.into(),
        },
    }
}
fn rectangle() -> GpuRectangle {
    GpuRectangle {
        bounds: [0.0, 0.0, 1.0, 1.0],
        palette_range: [0.0, 1.0],
    }
}
fn rectangle_frame(
    rectangles: &[GpuRectangle],
    colors: &[[f32; 4]],
    params: GpuRectangleParams,
    limits: GpuLimits,
) -> anyhow::Result<Arc<GpuFrame>> {
    GpuFrame::rectangles(
        rectangles,
        GpuPalette::new(colors, limits.max_palette_colors)?,
        params,
        limits,
    )
}
fn frame(color: [f32; 4]) -> Arc<GpuFrame> {
    rectangle_frame(
        &[rectangle()],
        &[color],
        GpuRectangleParams::default(),
        limits(),
    )
    .unwrap()
}
fn draw(source: GpuSource, placement: Bounds<ScaledPixels>) -> GpuDraw {
    GpuDraw {
        order: 0,
        bounds: placement,
        content_mask: ContentMask { bounds: placement },
        opacity: 1.0,
        scale_factor: 1.0,
        source,
    }
}
fn quad(placement: Bounds<ScaledPixels>, color: gpui::Hsla) -> gpui::Quad {
    gpui::Quad {
        order: 0,
        border_style: gpui::BorderStyle::Solid,
        bounds: placement,
        content_mask: ContentMask { bounds: placement },
        background: color.into(),
        border_color: color,
        corner_radii: Default::default(),
        border_widths: Default::default(),
    }
}
fn pixel(image: &image::RgbaImage, x: u32, y: u32, expected: [u8; 4]) {
    let actual = image.get_pixel(x, y).0;
    assert!(
        actual.iter().zip(expected).all(|(a, e)| a.abs_diff(e) <= 3),
        "({x},{y}): {actual:?} != {expected:?}"
    );
}

#[test]
fn numerical_column_batches_keep_7200_columns_in_two_panels() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let palette = GpuPalette::new(&[[1.0, 0.0, 0.0, 1.0]], 1)?;
    let intervals: [[f32; 2]; 32] =
        std::array::from_fn(|i| [i as f32 / 32.0, (i + 1) as f32 / 32.0]);
    let limits = GpuLimits {
        max_field_width: 32,
        max_field_height: 1,
        max_field_cells: 32,
        max_palette_colors: 1,
        ..Default::default()
    };
    let mut scene = Scene::default();
    scene.insert_primitive(quad(bounds(0.0, 0.0, 7200.0, 8.0), gpui::black()));
    for offset in (0..7200).step_by(32) {
        let publication = GpuFrame::field_columns(
            &intervals,
            1,
            &[1.0; 32],
            palette.clone(),
            GpuFieldParams {
                mapping: GpuValueMapping::Linear { min: 0.0, max: 1.0 },
            },
            limits,
        )?;
        for y in [0.0, 4.0] {
            scene.insert_primitive(draw(
                GpuSource::Fixed(publication.clone()),
                bounds(offset as f32, y, 32.0, 4.0),
            ));
        }
    }
    scene.finish();
    let image = renderer.render_scene_to_image(&scene, size(7200, 8))?;
    for y in [1, 5] {
        for x in [0, 31, 32, 2048, 4095, 4096, 7199] {
            pixel(&image, x, y, RED);
        }
    }
    Ok(())
}

#[test]
fn numerical_history_high_rows_fit_shared_budget_and_reuse_between_panels() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let palette = GpuPalette::new(&[[1.0, 0.0, 0.0, 1.0]], 1)?;
    let mut scene = Scene::default();
    scene.gpu_material_budget = Some(64 * 1024 * 1024);
    let mut old_frames = Vec::new();
    for offset in (0..3600).step_by(32) {
        let columns = (3600 - offset).min(32);
        let intervals: Vec<_> = (0..columns)
            .map(|i| [i as f32 / columns as f32, (i + 1) as f32 / columns as f32])
            .collect();
        let publication = GpuFrame::field_columns(
            &intervals,
            580,
            &vec![1.0; columns * 580],
            palette.clone(),
            GpuFieldParams {
                mapping: GpuValueMapping::Linear { min: 0.0, max: 1.0 },
            },
            GpuLimits {
                max_field_width: 32,
                max_field_height: 580,
                max_field_cells: 32 * 580,
                max_palette_colors: 1,
                ..Default::default()
            },
        )?;
        for y in [0.0, 4.0] {
            scene.insert_primitive(draw(
                GpuSource::Fixed(publication.clone()),
                bounds(offset as f32, y, columns as f32, 4.0),
            ));
        }
        old_frames.push(publication);
    }
    scene.finish();
    let image = renderer.render_scene_to_image(&scene, size(3600, 8))?;
    for y in [0, 3, 4, 7] {
        for x in [0, 31, 32, 3583, 3584, 3599] {
            pixel(&image, x, y, RED);
        }
    }
    assert_eq!(renderer.core.resources.materials.stats().0, 113);
    renderer.render_scene_to_image(&scene, size(3600, 8))?;
    assert_eq!(renderer.core.resources.materials.stats().3, 113);
    // Keep every old upload wanted while admitting a completely new history.
    // Holding CPU Arcs alone would not prevent retirement of completed uploads.
    scene.clear();
    let blue = GpuPalette::new(&[[0.0, 0.0, 1.0, 1.0]], 1)?;
    for (index, old) in old_frames.into_iter().enumerate() {
        let gpui::GpuContent::Field {
            width,
            height,
            values,
            intervals: Some(intervals),
            params,
        } = old.content()
        else {
            unreachable!()
        };
        let new = GpuFrame::field_columns(
            intervals,
            *height,
            values,
            blue.clone(),
            *params,
            GpuLimits {
                max_field_width: 32,
                max_field_height: 580,
                max_field_cells: 32 * 580,
                max_palette_colors: 1,
                ..Default::default()
            },
        )?;
        let width = *width as f32;
        scene.insert_primitive(draw(
            GpuSource::Fixed(old),
            bounds((index * 32) as f32, 0.0, width, 4.0),
        ));
        scene.insert_primitive(draw(
            GpuSource::Fixed(new),
            bounds((index * 32) as f32, 4.0, width, 4.0),
        ));
    }
    scene.finish();
    let image = renderer.render_scene_to_image(&scene, size(3600, 8))?;
    for x in [0, 31, 32, 3583, 3584, 3599] {
        pixel(&image, x, 1, RED);
        pixel(&image, x, 5, BLUE);
    }
    assert_eq!(renderer.core.resources.materials.stats().0, 226);
    Ok(())
}

#[test]
fn column_upload_pressure_keeps_uploaded_history_drawable() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let palette = GpuPalette::new(&[[1.0, 0.0, 0.0, 1.0]], 1)?;
    let limits = GpuLimits {
        max_field_width: 1,
        max_field_height: 256,
        max_field_cells: 256,
        max_palette_colors: 1,
        ..Default::default()
    };
    let params = GpuFieldParams {
        mapping: GpuValueMapping::Linear { min: 0.0, max: 1.0 },
    };
    let old = GpuFrame::field_columns(&[[0.0, 1.0]], 1, &[1.0], palette.clone(), params, limits)?;
    let new = GpuFrame::field_columns(&[[0.0, 1.0]], 256, &[1.0; 256], palette, params, limits)?;
    let mut scene = Scene::default();
    scene.gpu_material_budget = Some(1024);
    scene.insert_primitive(draw(
        GpuSource::Fixed(old.clone()),
        bounds(0.0, 0.0, 4.0, 4.0),
    ));
    scene.finish();
    renderer.render_scene_to_image(&scene, size(8, 4))?;
    scene.clear();
    scene.insert_primitive(quad(bounds(0.0, 0.0, 8.0, 4.0), gpui::blue()));
    scene.insert_primitive(draw(GpuSource::Fixed(old), bounds(0.0, 0.0, 4.0, 4.0)));
    scene.insert_primitive(draw(GpuSource::Fixed(new), bounds(4.0, 0.0, 4.0, 4.0)));
    scene.finish();
    let image = renderer.render_scene_to_image(&scene, size(8, 4))?;
    pixel(&image, 1, 1, RED);
    pixel(&image, 6, 1, BLUE);
    Ok(())
}

#[test]
fn numerical_columns_keep_gaps_floor_and_lowest_valid_palette_color() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let palette = GpuPalette::new(
        &[[65.0 / 255.0, 72.0 / 255.0, 110.0 / 255.0, 1.0], [1.0; 4]],
        2,
    )?;
    let publication = GpuFrame::field_columns(
        &[[0.0, 0.25], [0.5, 0.75], [0.8, 1.0]],
        1,
        &[-1.0, -0.999999, f32::NAN],
        palette,
        GpuFieldParams {
            mapping: GpuValueMapping::Linear {
                min: -1.0,
                max: 0.0,
            },
        },
        GpuLimits {
            max_field_width: 3,
            max_field_height: 1,
            max_field_cells: 3,
            max_palette_colors: 2,
            ..Default::default()
        },
    )?;
    let extent = bounds(0.0, 0.0, 32.0, 4.0);
    let mut scene = Scene::default();
    scene.insert_primitive(quad(extent, gpui::black()));
    scene.insert_primitive(draw(GpuSource::Fixed(publication), extent));
    scene.finish();
    let image = renderer.render_scene_to_image(&scene, size(32, 4))?;
    pixel(&image, 3, 2, BLACK);
    pixel(&image, 12, 2, BLACK);
    pixel(&image, 20, 2, [65, 72, 110, 255]);
    pixel(&image, 29, 2, BLACK);
    Ok(())
}

#[test]
fn segment_admission_bounds_indices_and_preserves_publication_on_failure() {
    use gpui::{GpuSchema, GpuSegment, GpuStrokeLayer};
    const LIMITS: GpuLimits = GpuLimits {
        max_rectangles: 2,
        max_segments: 2,
        max_stroke_layers: 2,
        max_field_width: 2,
        max_field_height: 2,
        max_field_cells: 4,
        max_path_vertices: 6,
        max_palette_colors: 2,
    };
    fn palette() -> Arc<GpuPalette> {
        GpuPalette::new(&[[0.0, 0.5, 1.0, 0.0], [1.0; 4]], 2).unwrap()
    }
    let line = GpuSegment {
        endpoints: [0.0, 0.5, 1.0, 0.5],
        color: 0,
    };
    let layer = GpuStrokeLayer {
        width: 1.0,
        palette_offset: 1,
    };
    let initial = GpuFrame::segments(&[line], &[layer], palette(), LIMITS).unwrap();
    let slot = GpuState::new(initial.clone(), LIMITS).unwrap();
    assert!(GpuFrame::segments(&[line; 3], &[layer], palette(), LIMITS).is_err());
    assert!(GpuFrame::segments(&[line], &[layer; 3], palette(), LIMITS).is_err());
    for width in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert!(
            GpuFrame::segments(
                &[line],
                &[GpuStrokeLayer { width, ..layer }],
                palette(),
                LIMITS
            )
            .is_err()
        );
    }
    for palette_offset in [2, u32::MAX] {
        assert!(
            GpuFrame::segments(
                &[line],
                &[GpuStrokeLayer {
                    palette_offset,
                    ..layer
                }],
                palette(),
                LIMITS
            )
            .is_err()
        );
    }
    assert!(
        GpuFrame::segments(
            &[GpuSegment {
                endpoints: [f32::NAN; 4],
                ..line
            }],
            &[layer],
            palette(),
            LIMITS
        )
        .is_err()
    );
    let larger = GpuFrame::segments(
        &[line; 3],
        &[layer],
        palette(),
        GpuLimits {
            max_segments: 3,
            ..LIMITS
        },
    )
    .unwrap();
    assert!(slot.publish(larger).is_err());
    assert!(Arc::ptr_eq(&slot.snapshot().unwrap(), &initial));
    slot.clear();
    slot.publish(initial).unwrap();
    assert_eq!(slot.schema(), GpuSchema::Segments);
}
#[test]
fn gpu_segments_preserve_source_over_caps_layers_and_scaled_width() -> anyhow::Result<()> {
    use gpui::{GpuSegment, GpuStrokeLayer};
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let lines = [
        GpuSegment {
            endpoints: [0.125, 0.5, 0.875, 0.5],
            color: 0,
        },
        GpuSegment {
            endpoints: [0.5, 0.125, 0.5, 0.875],
            color: 0,
        },
        GpuSegment {
            endpoints: [0.25; 4],
            color: 0,
        },
    ];
    let palette = GpuPalette::new(&[[1.0, 1.0, 1.0, 0.5]], 1)?;
    let content = GpuFrame::segments(
        &lines,
        &[GpuStrokeLayer {
            width: 4.0,
            palette_offset: 0,
        }],
        palette,
        limits(),
    )?;
    for scale in [1.0, 2.0] {
        let extent = bounds(0.0, 0.0, 32.0 * scale, 32.0 * scale);
        let mut placement = draw(GpuSource::Fixed(content.clone()), extent);
        placement.scale_factor = scale;
        let mut scene = Scene::default();
        scene.insert_primitive(quad(extent, gpui::black()));
        scene.insert_primitive(placement);
        scene.finish();
        let image = renderer
            .render_scene_to_image(&scene, size((32.0 * scale) as i32, (32.0 * scale) as i32))?;
        let at = |v: f32| (v * scale) as u32;
        pixel(&image, at(8.0), at(16.0), [128, 128, 128, 255]);
        pixel(&image, at(16.0), at(16.0), [191, 191, 191, 255]);
        pixel(&image, at(3.0), at(16.0), BLACK);
        pixel(&image, at(8.0), at(13.0), BLACK);
        pixel(&image, at(8.0), at(8.0), BLACK);
    }
    let palette = GpuPalette::new(&[[1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]], 2)?;
    let content = GpuFrame::segments(
        &lines[..1],
        &[
            GpuStrokeLayer {
                width: 8.0,
                palette_offset: 0,
            },
            GpuStrokeLayer {
                width: 2.0,
                palette_offset: 1,
            },
        ],
        palette,
        limits(),
    )?;
    let extent = bounds(0.0, 0.0, 32.0, 32.0);
    let mut placement = draw(GpuSource::Fixed(content), extent);
    placement.content_mask.bounds = bounds(8.0, 0.0, 24.0, 32.0);
    let mut scene = Scene::default();
    scene.insert_primitive(quad(extent, gpui::black()));
    scene.insert_primitive(placement);
    scene.finish();
    let image = renderer.render_scene_to_image(&scene, size(32, 32))?;
    pixel(&image, 12, 16, BLUE);
    pixel(&image, 12, 13, RED);
    pixel(&image, 6, 16, BLACK);
    Ok(())
}

#[test]
fn gpu_rectangles_order_mask_opacity_and_shared_upload() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let red = frame([1.0, 0.0, 0.0, 1.0]);
    let mut scene = Scene::default();
    scene.insert_primitive(quad(bounds(0.0, 0.0, 32.0, 16.0), gpui::blue()));
    let mut first = draw(GpuSource::Fixed(red.clone()), bounds(0.0, 0.0, 16.0, 16.0));
    first.content_mask.bounds = bounds(4.0, 0.0, 12.0, 16.0);
    first.opacity = 0.5;
    scene.insert_primitive(first);
    scene.insert_primitive(draw(
        GpuSource::Fixed(red.clone()),
        bounds(16.0, 0.0, 16.0, 16.0),
    ));
    scene.insert_primitive(quad(bounds(24.0, 0.0, 8.0, 16.0), gpui::blue()));
    scene.finish();
    let cpu_owners = Arc::strong_count(&red);
    let image = renderer.render_scene_to_image(&scene, size(32, 16))?;
    pixel(&image, 2, 8, BLUE);
    pixel(&image, 8, 8, [128, 0, 128, 255]);
    pixel(&image, 20, 8, RED);
    pixel(&image, 28, 8, BLUE);
    assert_eq!(renderer.core.resources.materials.stats().3, 1);
    assert_eq!(
        Arc::strong_count(&red),
        cpu_owners,
        "GPU cache must not retain CPU frames"
    );
    renderer.render_scene_to_image(&scene, size(32, 16))?;
    assert_eq!(
        renderer.core.resources.materials.stats().3,
        1,
        "static replay must not upload again"
    );
    let image = renderer.render_scene_to_image(&scene, size(48, 24))?;
    pixel(&image, 20, 8, RED);
    pixel(&image, 40, 20, BLACK);
    assert_eq!(
        renderer.core.resources.materials.stats().3,
        1,
        "resize changes placement, not immutable content"
    );
    Ok(())
}

#[test]
fn gpu_rectangles_palette_alpha_segments_and_scale() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let gradient = rectangle_frame(
        &[rectangle()],
        &[[0.0, 0.0, 1.0, 1.0], [1.0, 0.0, 0.0, 1.0]],
        GpuRectangleParams::default(),
        limits(),
    )?;
    let segmented = rectangle_frame(
        &[rectangle()],
        &[[1.0, 0.0, 0.0, 0.5]],
        GpuRectangleParams {
            cell_height: 2.0,
            cell_gap: 2.0,
        },
        limits(),
    )?;
    let mut scene = Scene::default();
    scene.insert_primitive(draw(
        GpuSource::Fixed(gradient),
        bounds(0.0, 0.0, 8.0, 16.0),
    ));
    let mut segments = draw(GpuSource::Fixed(segmented), bounds(8.0, 0.0, 8.0, 16.0));
    segments.scale_factor = 2.0;
    scene.insert_primitive(segments);
    scene.finish();
    let image = renderer.render_scene_to_image(&scene, size(16, 16))?;
    pixel(&image, 4, 0, [247, 0, 8, 255]);
    pixel(&image, 4, 15, [8, 0, 247, 255]);
    pixel(&image, 4, 7, [135, 0, 120, 255]);
    pixel(&image, 12, 14, [128, 0, 0, 255]);
    pixel(&image, 12, 10, BLACK);
    pixel(&image, 12, 6, [128, 0, 0, 255]);
    pixel(&image, 12, 2, BLACK);
    Ok(())
}

#[test]
fn gpu_rectangles_preserve_adjacent_coverage_gaps_and_fractional_edges() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    for segmented in [false, true] {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let rectangles =
                [(4.25, 1.0), (5.25, 1.0), (8.25, 0.25)].map(|(x, width)| GpuRectangle {
                    bounds: [x / 16.0, 2.25 / 16.0, width / 16.0, 7.25 / 16.0],
                    palette_range: [0.0, 1.0],
                });
            let params = if segmented {
                GpuRectangleParams {
                    cell_height: 3.0,
                    cell_gap: 2.0,
                }
            } else {
                GpuRectangleParams::default()
            };
            let content = rectangle_frame(&rectangles, &[[1.0; 4]], params, limits())?;
            let extent = bounds(0.0, 0.0, 16.0 * scale, 16.0 * scale);
            let mut placement = draw(GpuSource::Fixed(content), extent);
            placement.scale_factor = scale;
            let mut scene = Scene::default();
            scene.insert_primitive(placement);
            scene.finish();
            let actual = renderer.render_scene_to_image(&scene, size(32, 32))?;

            // Adjacent columns cover their shared boundary without an alpha seam.
            // A fractional outer edge has coverage, but does not become opaque.
            let edge_y = (8.0 * scale) as u32;
            let outer = actual.get_pixel((4.25 * scale).floor() as u32, edge_y).0[0];
            assert!(
                outer > 0 && outer < 255,
                "fractional edge lost at scale {scale}"
            );
            pixel(&actual, (3.0 * scale) as u32, edge_y, BLACK);
            if segmented {
                // The upper partial cell survives, and the middle gap stays empty.
                let x = (5.25 * scale) as u32;
                assert!(actual.get_pixel(x, (3.0 * scale) as u32).0[0] > 0);
                pixel(&actual, x, (5.5 * scale) as u32, BLACK);
            }
            pixel(
                &actual,
                (5.25 * scale) as u32,
                (8.0 * scale) as u32,
                [255; 4],
            );
        }
    }
    Ok(())
}

#[test]
fn gpu_rectangles_fixed_dynamic_coherence_clear_and_retirement() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let initial = frame([1.0, 0.0, 0.0, 1.0]);
    let state = GpuState::new(initial.clone(), limits())?;
    let mut scene = Scene::default();
    scene.insert_primitive(draw(
        GpuSource::Fixed(initial.clone()),
        bounds(0.0, 0.0, 8.0, 8.0),
    ));
    for x in [8.0, 16.0] {
        scene.insert_primitive(draw(
            GpuSource::Dynamic(state.clone()),
            bounds(x, 0.0, 8.0, 8.0),
        ));
    }
    scene.finish();
    renderer.render_scene_to_image(&scene, size(24, 8))?;
    assert_eq!(renderer.core.resources.materials.stats().3, 1);
    assert_eq!(renderer.core.resources.materials.stats().4, 1);
    // Change both instance count and palette; the draw count must come from the
    // same snapshot as the uploaded data, not the old recorded scene.
    let half = GpuRectangle {
        bounds: [0.0, 0.0, 0.5, 1.0],
        palette_range: [0.0, 0.0],
    };
    let top_right = GpuRectangle {
        bounds: [0.5, 0.0, 0.5, 0.5],
        palette_range: [0.0, 0.0],
    };
    let mut instances = [half, top_right];
    let mut palette = [[0.0, 0.0, 1.0, 1.0]];
    state.publish(rectangle_frame(
        &instances,
        &palette,
        GpuRectangleParams::default(),
        limits(),
    )?)?;
    // The caller reuses its scratch before the deferred upload. Only an explicit
    // later publication may change the already recorded blue, two-instance draw.
    instances.fill(rectangle());
    palette[0] = [0.0, 1.0, 0.0, 1.0];
    let image = renderer.render_scene_to_image(&scene, size(24, 8))?;
    pixel(&image, 4, 4, RED);
    pixel(&image, 9, 4, BLUE);
    pixel(&image, 13, 4, BLACK);
    pixel(&image, 17, 4, BLUE);
    pixel(&image, 21, 4, BLACK);
    pixel(&image, 13, 1, BLUE);
    pixel(&image, 21, 1, BLUE);
    assert_eq!(renderer.core.resources.materials.stats().4, 2);
    state.publish(rectangle_frame(
        &[],
        &[[0.0, 1.0, 0.0, 1.0]],
        GpuRectangleParams::default(),
        limits(),
    )?)?;
    let image = renderer.render_scene_to_image(&scene, size(24, 8))?;
    pixel(&image, 9, 4, BLACK);
    assert_eq!(
        renderer.core.resources.materials.stats().3,
        2,
        "empty content does not allocate"
    );
    state.clear();
    let image = renderer.render_scene_to_image(&scene, size(24, 8))?;
    pixel(&image, 4, 4, RED);
    pixel(&image, 17, 4, BLACK);
    assert_eq!(
        renderer.core.resources.materials.stats().4,
        4,
        "revoked slots also resolve once"
    );
    state.publish(rectangle_frame(
        &instances[..1],
        &palette,
        GpuRectangleParams::default(),
        limits(),
    )?)?;
    let image = renderer.render_scene_to_image(&scene, size(24, 8))?;
    pixel(&image, 17, 4, [0, 255, 0, 255]);
    drop(scene);
    let empty = Scene::default();
    renderer.render_scene_to_image(&empty, size(24, 8))?;
    let stats = renderer.core.resources.materials.stats();
    assert_eq!(
        (stats.0, stats.1, stats.2),
        (0, 0, 0),
        "empty scene reclaims completed GPU storage even if CPU owners remain"
    );
    drop(renderer);
    assert_eq!(Arc::strong_count(&initial), 1);
    Ok(())
}

#[test]
fn gpu_rectangles_budget_and_capability_failures_leave_other_draws_intact() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let good = frame([1.0, 0.0, 0.0, 1.0]);
    let large = rectangle_frame(
        &vec![rectangle(); 512],
        &[[0.0, 1.0, 0.0, 1.0]],
        GpuRectangleParams::default(),
        limits(),
    )?;
    renderer.core.resources.materials.set_byte_limit(1024);
    let mut scene = Scene::default();
    scene.insert_primitive(quad(bounds(0.0, 0.0, 24.0, 8.0), gpui::blue()));
    scene.insert_primitive(draw(GpuSource::Fixed(large), bounds(0.0, 0.0, 8.0, 8.0)));
    scene.insert_primitive(draw(
        GpuSource::Fixed(good.clone()),
        bounds(8.0, 0.0, 8.0, 8.0),
    ));
    let mut invalid = draw(GpuSource::Fixed(good), bounds(16.0, 0.0, 8.0, 8.0));
    invalid.scale_factor = f32::NAN;
    scene.insert_primitive(invalid);
    scene.finish();
    let image = renderer.render_scene_to_image(&scene, size(24, 8))?;
    pixel(&image, 4, 4, BLUE);
    pixel(&image, 12, 4, RED);
    pixel(&image, 20, 4, BLUE);
    let stats = renderer.core.resources.materials.stats();
    assert_eq!(stats.0, 1);
    assert!(stats.2 <= 1024);
    renderer.core.resources.materials.disable_program();
    let image = renderer.render_scene_to_image(&scene, size(24, 8))?;
    pixel(&image, 12, 4, BLUE);
    Ok(())
}

#[test]
fn gpu_rectangles_device_generation_rebuilds_from_owned_content() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let red = frame([1.0, 0.0, 0.0, 1.0]);
    let mut scene = Scene::default();
    scene.insert_primitive(draw(GpuSource::Fixed(red), bounds(0.0, 0.0, 8.0, 8.0)));
    scene.finish();
    renderer.render_scene_to_image(&scene, size(8, 8))?;
    // WgpuResources is the existing device recovery boundary. Reconstructing it
    // must not inherit any physical cache from the previous generation.
    renderer.core = WgpuRendererCore::new(
        &renderer.context,
        Arc::new(WgpuAtlas::from_context(&renderer.context)),
        renderer.core.target_format,
        wgpu::CompositeAlphaMode::Opaque,
    );
    assert_eq!(renderer.core.resources.materials.stats().0, 0);
    let image = renderer.render_scene_to_image(&scene, size(8, 8))?;
    pixel(&image, 4, 4, RED);
    assert_eq!(renderer.core.resources.materials.stats().3, 1);
    Ok(())
}

#[test]
fn gpu_rectangles_recording_error_retires_prepared_storage_and_recovers() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let red = frame([1.0, 0.0, 0.0, 1.0]);
    let mut scene = Scene::default();
    scene.insert_primitive(draw(
        GpuSource::Fixed(red.clone()),
        bounds(0.0, 0.0, 8.0, 8.0),
    ));
    let mut builder = gpui::PathBuilder::fill();
    builder.move_to(gpui::point(gpui::px(0.0), gpui::px(0.0)));
    builder.line_to(gpui::point(gpui::px(8.0), gpui::px(0.0)));
    builder.line_to(gpui::point(gpui::px(8.0), gpui::px(8.0)));
    builder.line_to(gpui::point(gpui::px(0.0), gpui::px(8.0)));
    builder.close();
    let mut path = builder.build()?.scale(1.0);
    path.content_mask = ContentMask {
        bounds: path.bounds,
    };
    let original = path.vertices.as_ref().clone();
    let vertices = Arc::make_mut(&mut path.vertices);
    for _ in 0..128 {
        vertices.extend_from_slice(&original);
    }
    scene.insert_primitive(path);
    scene.finish();
    // Empty ordinary instance arrays fit; the later path expansion really
    // fails grow_instance_data after rectangle preparation and pass encoding.
    let capacity = renderer.core.instance_data_capacity;
    let maximum = renderer.core.max_instance_data_size;
    renderer.core.instance_data_capacity = 2048;
    renderer.core.max_instance_data_size = 2048;
    let error = renderer.render_scene(&scene, size(8, 8)).unwrap_err();
    assert!(
        error.to_string().contains("instance"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        renderer.core.resources.materials.stats().3,
        1,
        "failure must occur after preparation"
    );
    renderer.core.instance_data_capacity = capacity;
    renderer.core.max_instance_data_size = maximum;
    let mut valid = Scene::default();
    valid.insert_primitive(draw(GpuSource::Fixed(red), bounds(0.0, 0.0, 8.0, 8.0)));
    valid.finish();
    let image = renderer.render_scene_to_image(&valid, size(8, 8))?;
    pixel(&image, 4, 4, RED);
    renderer.render_scene_to_image(&Scene::default(), size(8, 8))?;
    let stats = renderer.core.resources.materials.stats();
    assert_eq!((stats.0, stats.1, stats.2), (0, 0, 0));
    Ok(())
}

#[test]
fn gpu_rectangles_premultiplied_and_srgb_target_conventions() -> anyhow::Result<()> {
    for (format, alpha_mode, color, opacity, expected) in [
        (
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::CompositeAlphaMode::PreMultiplied,
            [0.5, 0.0, 0.0, 0.5],
            0.5,
            [32, 0, 0, 64],
        ),
        (
            wgpu::TextureFormat::Rgba8UnormSrgb,
            wgpu::CompositeAlphaMode::Opaque,
            [0.5, 0.25, 0.75, 1.0],
            1.0,
            [128, 64, 191, 255],
        ),
    ] {
        let mut renderer = WgpuHeadlessRenderer::new()?;
        renderer.core = WgpuRendererCore::new(
            &renderer.context,
            Arc::new(WgpuAtlas::from_context(&renderer.context)),
            format,
            alpha_mode,
        );
        let mut scene = Scene::default();
        let mut batch = draw(GpuSource::Fixed(frame(color)), bounds(0.0, 0.0, 8.0, 8.0));
        batch.opacity = opacity;
        scene.insert_primitive(batch);
        scene.finish();
        renderer.ensure_render_target(size(8, 8))?;
        let view = renderer.render_target.as_ref().unwrap().view.clone();
        renderer.core.render_frame(
            &scene,
            &view,
            size(8, 8),
            alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::Color::TRANSPARENT,
        )?;
        pixel(&renderer.read_image()?, 4, 4, expected);
    }
    Ok(())
}

fn field_frame(
    width: u32,
    height: u32,
    values: &[f32],
    palette: Arc<GpuPalette>,
    mapping: GpuValueMapping,
) -> anyhow::Result<Arc<GpuFrame>> {
    GpuFrame::field(
        width,
        height,
        values,
        palette,
        GpuFieldParams { mapping },
        limits(),
    )
}

#[test]
fn fields_clamp_edges_and_map_before_color_interpolation() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let palette = GpuPalette::new(
        &[
            [1.0, 0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0, 1.0],
            [0.0, 0.0, 1.0, 1.0],
        ],
        3,
    )?;
    let mut scene = Scene::default();
    scene.insert_primitive(draw(
        GpuSource::Fixed(field_frame(
            2,
            1,
            &[0.0, 1.0],
            palette.clone(),
            GpuValueMapping::Linear { min: 0.0, max: 1.0 },
        )?),
        bounds(0.0, 0.0, 8.0, 8.0),
    ));
    scene.insert_primitive(draw(
        GpuSource::Fixed(field_frame(
            1,
            2,
            &[0.0, 1.0],
            palette.clone(),
            GpuValueMapping::Linear { min: 0.0, max: 1.0 },
        )?),
        bounds(8.0, 0.0, 8.0, 8.0),
    ));
    scene.insert_primitive(draw(
        GpuSource::Fixed(field_frame(
            2,
            2,
            &[0.0, 1.0, 1.0, 0.0],
            palette.clone(),
            GpuValueMapping::Linear { min: 0.0, max: 1.0 },
        )?),
        bounds(0.0, 8.0, 8.0, 8.0),
    ));
    scene.finish();
    let image = renderer.render_scene_to_image(&scene, size(16, 16))?;
    pixel(&image, 0, 0, RED);
    pixel(&image, 7, 7, BLUE);
    pixel(&image, 3, 4, [159, 0, 96, 255]);
    assert_ne!(
        image.get_pixel(3, 4).0,
        [0, 255, 0, 255],
        "interpolating scalars first would incorrectly select the middle palette entry"
    );
    pixel(&image, 8, 0, RED);
    pixel(&image, 15, 7, BLUE);
    pixel(&image, 12, 3, [159, 0, 96, 255]);
    pixel(&image, 0, 8, RED);
    pixel(&image, 7, 8, BLUE);
    pixel(&image, 0, 15, BLUE);
    pixel(&image, 7, 15, RED);
    pixel(&image, 3, 11, [135, 0, 120, 255]);
    assert_eq!(renderer.core.resources.materials.palette_stats(), (1, 1));
    assert_eq!(renderer.core.resources.materials.stats().3, 3);
    renderer.render_scene_to_image(&scene, size(16, 16))?;
    assert_eq!(renderer.core.resources.materials.stats().3, 3);
    assert_eq!(renderer.core.resources.materials.palette_stats(), (1, 1));
    Ok(())
}

#[test]
fn fields_nonfinite_mapping_transparent_zero_and_single_cell() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let palette = GpuPalette::new(
        &[
            [1.0, 0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0, 1.0],
            [0.0, 0.0, 1.0, 1.0],
        ],
        3,
    )?;
    let mut scene = Scene::default();
    scene.insert_primitive(draw(
        GpuSource::Fixed(field_frame(
            4,
            1,
            &[f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.5],
            palette.clone(),
            GpuValueMapping::Linear {
                min: -1.0,
                max: 1.0,
            },
        )?),
        bounds(0.0, 0.0, 4.0, 1.0),
    ));
    scene.insert_primitive(draw(
        GpuSource::Fixed(field_frame(
            4,
            1,
            &[-0.5, 0.0, 0.5, 1.0],
            palette.clone(),
            GpuValueMapping::Linear {
                min: -1.0,
                max: 1.0,
            },
        )?),
        bounds(0.0, 1.0, 4.0, 1.0),
    ));
    scene.insert_primitive(draw(
        GpuSource::Fixed(field_frame(
            3,
            1,
            &[0.01, 0.1, 1.0],
            palette.clone(),
            GpuValueMapping::Logarithmic { db_range: 20.0 },
        )?),
        bounds(0.0, 2.0, 3.0, 1.0),
    ));
    scene.insert_primitive(draw(
        GpuSource::Fixed(field_frame(
            4,
            1,
            &[-1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY],
            palette.clone(),
            GpuValueMapping::Logarithmic { db_range: 20.0 },
        )?),
        bounds(0.0, 3.0, 4.0, 1.0),
    ));
    scene.insert_primitive(draw(
        GpuSource::Fixed(field_frame(
            1,
            1,
            &[1.0],
            palette,
            GpuValueMapping::Linear { min: 0.0, max: 1.0 },
        )?),
        bounds(4.0, 0.0, 4.0, 4.0),
    ));
    scene.insert_primitive(quad(bounds(0.0, 4.0, 8.0, 4.0), gpui::blue()));
    let transparent = GpuPalette::new(&[[0.0; 4], [1.0, 0.0, 0.0, 1.0]], 2)?;
    scene.insert_primitive(draw(
        GpuSource::Fixed(field_frame(
            2,
            1,
            &[0.0, 1.0],
            transparent,
            GpuValueMapping::Logarithmic { db_range: 60.0 },
        )?),
        bounds(0.0, 4.0, 8.0, 4.0),
    ));
    scene.finish();
    let image = renderer.render_scene_to_image(&scene, size(8, 8))?;
    for x in 0..3 {
        pixel(&image, x, 0, RED);
    }
    for x in 0..2 {
        assert_eq!(image.get_pixel(x, 1).0, [0, 255, 0, 255]);
    }
    for x in 2..4 {
        assert_eq!(image.get_pixel(x, 1).0, BLUE);
    }
    // Exactly half a palette step rounds upward like Rust f32::round, not ties-to-even.
    assert_eq!(image.get_pixel(3, 0).0, [0, 255, 0, 255]);
    for x in 0..4 {
        pixel(&image, x, 3, RED);
    }
    pixel(&image, 0, 2, RED);
    pixel(&image, 1, 2, [0, 255, 0, 255]);
    pixel(&image, 2, 2, BLUE);
    pixel(&image, 4, 0, BLUE);
    pixel(&image, 7, 3, BLUE);
    pixel(&image, 0, 6, BLUE);
    pixel(&image, 3, 6, [36, 0, 159, 255]);
    pixel(&image, 7, 6, RED);
    Ok(())
}

#[test]
fn field_dynamic_dimensions_clear_failed_publication_and_budget() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let palette = GpuPalette::new(&[[1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]], 2)?;
    let mapping = GpuValueMapping::Linear { min: 0.0, max: 1.0 };
    let initial = field_frame(1, 1, &[0.0], palette.clone(), mapping)?;
    let state = GpuState::new(initial.clone(), limits())?;
    let mut scene = Scene::default();
    scene.insert_primitive(draw(GpuSource::Fixed(initial), bounds(0.0, 0.0, 4.0, 4.0)));
    for x in [4.0, 8.0] {
        scene.insert_primitive(draw(
            GpuSource::Dynamic(state.clone()),
            bounds(x, 0.0, 4.0, 4.0),
        ));
    }
    scene.finish();
    renderer.render_scene_to_image(&scene, size(12, 4))?;
    state.publish(field_frame(1, 2, &[1.0, 0.0], palette.clone(), mapping)?)?;
    assert!(state.publish(frame([0.0, 1.0, 0.0, 1.0])).is_err());
    let image = renderer.render_scene_to_image(&scene, size(12, 4))?;
    pixel(&image, 1, 0, RED);
    pixel(&image, 5, 0, BLUE);
    pixel(&image, 9, 0, BLUE);
    pixel(&image, 5, 3, RED);
    assert_eq!(renderer.core.resources.materials.stats().4, 2);
    assert_eq!(renderer.core.resources.materials.palette_stats(), (1, 1));
    state.publish(field_frame(0, 2, &[], palette.clone(), mapping)?)?;
    let image = renderer.render_scene_to_image(&scene, size(12, 4))?;
    pixel(&image, 5, 0, BLACK);
    state.clear();
    renderer.render_scene_to_image(&scene, size(12, 4))?;
    assert_eq!(renderer.core.resources.materials.stats().4, 4);
    let before = renderer.core.resources.materials.stats().3;
    state.publish(field_frame(1024, 1, &vec![1.0; 1024], palette, mapping)?)?;
    renderer.core.resources.materials.set_byte_limit(2048);
    let image = renderer.render_scene_to_image(&scene, size(12, 4))?;
    pixel(&image, 1, 0, RED);
    pixel(&image, 5, 0, BLACK);
    assert_eq!(renderer.core.resources.materials.stats().3, before);
    renderer.render_scene_to_image(&Scene::default(), size(12, 4))?;
    assert_eq!(renderer.core.resources.materials.stats().2, 0);
    assert_eq!(renderer.core.resources.materials.palette_stats().0, 0);
    Ok(())
}

fn curved_path() -> Arc<gpui::Path<gpui::Pixels>> {
    let point = |x, y| gpui::point(gpui::px(x), gpui::px(y));
    let mut path = gpui::Path::new(point(2.0, 22.0));
    path.line_to(point(2.0, 2.0));
    path.curve_to(point(22.0, 22.0), point(22.0, 2.0));
    path.line_to(point(2.0, 22.0));
    Arc::new(path)
}

#[test]
fn palette_paths_preserve_existing_curve_coverage_and_msaa() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let path = curved_path();
    let mut reference = path.scale(1.0);
    reference.color = gpui::white().into();
    reference.content_mask = ContentMask {
        bounds: reference.bounds,
    };
    let placement = reference.bounds;
    let mut scene = Scene::default();
    scene.insert_primitive(reference);
    scene.finish();
    let expected = renderer.render_scene_to_image(&scene, size(24, 24))?;
    let palette = GpuPalette::new(&[[1.0; 4]], 1)?;
    let material = GpuFrame::path(path, palette, GpuPathParams::default(), limits())?;
    let mut scene = Scene::default();
    scene.insert_primitive(draw(GpuSource::Fixed(material), placement));
    scene.finish();
    let actual = renderer.render_scene_to_image(&scene, size(24, 24))?;
    for (a, e) in actual.pixels().zip(expected.pixels()) {
        assert!(
            a.0.iter().zip(e.0).all(|(a, e)| a.abs_diff(e) <= 3),
            "coverage mismatch {a:?} vs {e:?}"
        );
    }
    assert!(
        actual.pixels().any(|p| p.0[0] > 3 && p.0[0] < 252),
        "curve must retain fractional coverage"
    );
    pixel(&actual, 21, 3, BLACK);
    assert_eq!(
        renderer
            .core
            .resources
            .path_msaa_texture
            .as_ref()
            .map(|texture| texture.sample_count())
            .unwrap_or(1),
        renderer.core.rendering_params.path_sample_count
    );
    Ok(())
}

#[test]
fn palette_paths_keep_full_mapping_clip_order_opacity_and_shared_palette() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let palette = GpuPalette::new(&[[0.0, 0.0, 1.0, 1.0], [1.0, 0.0, 0.0, 1.0]], 2)?;
    let mut builder = gpui::PathBuilder::fill();
    for (index, (x, y)) in [(0.0, 0.0), (8.0, 0.0), (8.0, 16.0), (0.0, 16.0)]
        .into_iter()
        .enumerate()
    {
        let p = gpui::point(gpui::px(x), gpui::px(y));
        if index == 0 {
            builder.move_to(p);
        } else {
            builder.line_to(p);
        }
    }
    builder.close();
    let material = GpuFrame::path(
        Arc::new(builder.build()?),
        palette.clone(),
        GpuPathParams::default(),
        limits(),
    )?;
    let mut scene = Scene::default();
    scene.insert_primitive(quad(bounds(0.0, 0.0, 24.0, 16.0), gpui::blue()));
    let mut record = draw(
        GpuSource::Fixed(material.clone()),
        bounds(0.0, 0.0, 8.0, 16.0),
    );
    record.content_mask.bounds = bounds(0.0, 8.0, 8.0, 8.0);
    record.opacity = 0.5;
    scene.insert_primitive(record);
    scene.insert_primitive(draw(
        GpuSource::Fixed(material),
        bounds(8.0, 0.0, 8.0, 16.0),
    ));
    scene.insert_primitive(draw(
        GpuSource::Fixed(GpuFrame::rectangles(
            &[rectangle()],
            palette.clone(),
            GpuRectangleParams::default(),
            limits(),
        )?),
        bounds(16.0, 0.0, 8.0, 16.0),
    ));
    scene.insert_primitive(quad(bounds(8.0, 0.0, 8.0, 4.0), gpui::blue()));
    scene.finish();
    let image = renderer.render_scene_to_image(&scene, size(24, 16))?;
    pixel(&image, 4, 4, BLUE);
    pixel(&image, 4, 8, [60, 0, 195, 255]);
    pixel(&image, 12, 1, BLUE);
    pixel(&image, 12, 8, [120, 0, 135, 255]);
    pixel(&image, 20, 8, [120, 0, 135, 255]);
    assert_eq!(renderer.core.resources.materials.palette_stats(), (1, 1));
    renderer.render_scene_to_image(&Scene::default(), size(24, 16))?;
    assert!(renderer.core.resources.path_intermediate_texture.is_none());
    assert_eq!(renderer.core.resources.materials.stats().2, 0);
    Ok(())
}

fn palette_curve(colors: &[[f32; 4]]) -> anyhow::Result<Arc<GpuFrame>> {
    GpuFrame::path(
        curved_path(),
        GpuPalette::new(colors, limits().max_palette_colors)?,
        GpuPathParams::default(),
        limits(),
    )
}

fn compare_individual_material_draws(
    renderer: &mut WgpuHeadlessRenderer,
    scene: &Scene,
    target_size: Size<DevicePixels>,
) -> anyhow::Result<image::RgbaImage> {
    let actual = renderer.render_scene_to_image(scene, target_size)?;
    let mut reference = Scene::default();
    let mut order = 0;
    // Preserve the actual scene iterator's order, but put an offscreen ordinary
    // primitive after every material. Each reference Gpu batch then contains
    // exactly one draw, retaining the original per-path resolve/composite route.
    for batch in scene.batches() {
        match batch {
            PrimitiveBatch::Gpu(range) => {
                for record in &scene.gpu_draws[range] {
                    let mut record = record.clone();
                    record.order = order;
                    reference.gpu_draws.push(record);
                    order += 1;
                    let mut barrier = quad(bounds(-2.0, -2.0, 1.0, 1.0), gpui::black());
                    barrier.order = order;
                    reference.quads.push(barrier);
                    order += 1;
                }
            }
            PrimitiveBatch::Quads(range) => {
                for record in &scene.quads[range] {
                    let mut record = *record;
                    record.order = order;
                    reference.quads.push(record);
                    order += 1;
                }
            }
            _ => panic!("material comparison scenes contain only GPU draws and quads"),
        }
    }
    reference.finish();
    assert!(reference.batches().all(|batch| match batch {
        PrimitiveBatch::Gpu(range) => range.len() == 1,
        _ => true,
    }));
    let expected = renderer.render_scene_to_image(&reference, target_size)?;
    assert_eq!(actual.dimensions(), expected.dimensions());
    assert_eq!(
        actual.as_raw(),
        expected.as_raw(),
        "batched materials must exactly match individually resolved draws"
    );
    Ok(actual)
}

#[test]
fn palette_path_batches_match_individual_disjoint_clipped_paths() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let red = palette_curve(&[[1.0, 0.0, 0.0, 0.8], [1.0, 1.0, 0.0, 0.5]])?;
    let green = palette_curve(&[[0.0, 1.0, 0.0, 0.7]])?;
    let viewport = bounds(0.0, 0.0, 64.0, 28.0);
    let mut scene = Scene::default();
    scene.insert_primitive(quad(viewport, gpui::blue()));
    scene.push_layer(viewport);
    for (material, placement, clip, opacity) in [
        (
            red.clone(),
            bounds(-4.0, 2.0, 28.0, 24.0),
            bounds(0.0, 4.0, 10.0, 18.0),
            0.45,
        ),
        (
            green.clone(),
            bounds(0.0, 2.0, 28.0, 24.0),
            bounds(14.0, 4.0, 10.0, 18.0),
            0.75,
        ),
        (
            red,
            bounds(26.0, 2.0, 26.0, 24.0),
            bounds(28.0, 4.0, 12.0, 18.0),
            0.6,
        ),
        (
            green,
            bounds(48.0, 2.0, 28.0, 24.0),
            bounds(54.0, 4.0, 10.0, 18.0),
            0.9,
        ),
    ] {
        let mut record = draw(GpuSource::Fixed(material), placement);
        record.content_mask.bounds = clip;
        record.opacity = opacity;
        scene.insert_primitive(record);
    }
    scene.pop_layer();
    scene.finish();
    assert!(
        scene.gpu_draws[0]
            .bounds
            .intersects(&scene.gpu_draws[1].bounds)
    );
    assert!(
        scene
            .batches()
            .any(|batch| matches!(batch, PrimitiveBatch::Gpu(range) if range.len() == 4))
    );
    let actual = compare_individual_material_draws(&mut renderer, &scene, size(64, 28))?;
    for x in [12, 26, 52] {
        pixel(&actual, x, 12, BLUE);
    }
    pixel(&actual, 4, 2, BLUE);
    assert!(
        actual
            .pixels()
            .any(|pixel| pixel.0[0] > 10 && pixel.0[2] < 245)
    );
    assert!(
        actual
            .pixels()
            .any(|pixel| pixel.0[1] > 10 && pixel.0[0] < 3)
    );
    Ok(())
}

#[test]
fn palette_path_batches_preserve_translucent_msaa_overlap_resolves() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    anyhow::ensure!(
        renderer.core.rendering_params.path_sample_count > 1,
        "the palette path overlap regression requires a multisampling-capable adapter"
    );
    let viewport = bounds(0.0, 0.0, 32.0, 32.0);
    let mut scene = Scene::default();
    scene.insert_primitive(quad(viewport, gpui::blue()));
    scene.push_layer(viewport);
    for (material, opacity) in [
        (palette_curve(&[[1.0, 0.0, 0.0, 0.8]])?, 0.5),
        (palette_curve(&[[0.0, 1.0, 0.0, 0.6]])?, 0.75),
    ] {
        let mut record = draw(GpuSource::Fixed(material), bounds(2.0, 2.0, 28.0, 28.0));
        record.content_mask.bounds = viewport;
        record.opacity = opacity;
        scene.insert_primitive(record);
    }
    scene.pop_layer();
    scene.finish();
    assert_eq!(scene.gpu_draws[0].order, scene.gpu_draws[1].order);
    let actual = compare_individual_material_draws(&mut renderer, &scene, size(32, 32))?;
    pixel(&actual, 5, 20, [56, 115, 84, 255]);
    assert!(
        actual
            .pixels()
            .any(|pixel| pixel.0[1] > 3 && pixel.0[1] < 110),
        "the coincident translucent curves must exercise fractional edge coverage"
    );
    Ok(())
}

#[test]
fn palette_path_batches_preserve_order_and_interleaved_materials() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let red = palette_curve(&[[1.0, 0.0, 0.0, 0.7]])?;
    let green = palette_curve(&[[0.0, 1.0, 0.0, 0.6]])?;
    let viewport = bounds(0.0, 0.0, 64.0, 28.0);
    let mut scene = Scene::default();
    scene.insert_primitive(quad(viewport, gpui::blue()));
    // These disjoint paths have different numeric orders but no intervening
    // primitive in the scene iterator, so they may still share a path pass.
    for (material, placement) in [
        (red.clone(), bounds(0.0, 0.0, 20.0, 24.0)),
        (green.clone(), bounds(24.0, 0.0, 20.0, 24.0)),
    ] {
        scene.push_layer(viewport);
        scene.insert_primitive(draw(GpuSource::Fixed(material), placement));
        scene.pop_layer();
    }
    scene.insert_primitive(draw(
        GpuSource::Fixed(field_frame(
            2,
            1,
            &[0.0, 1.0],
            GpuPalette::new(&[[1.0, 1.0, 0.0, 0.4], [0.0, 1.0, 1.0, 0.6]], 2)?,
            GpuValueMapping::Linear { min: 0.0, max: 1.0 },
        )?),
        bounds(4.0, 6.0, 36.0, 12.0),
    ));
    scene.insert_primitive(draw(
        GpuSource::Fixed(red.clone()),
        bounds(4.0, 2.0, 20.0, 24.0),
    ));
    scene.insert_primitive(draw(
        GpuSource::Fixed(frame([1.0, 1.0, 1.0, 0.35])),
        bounds(12.0, 4.0, 24.0, 16.0),
    ));
    scene.insert_primitive(draw(
        GpuSource::Fixed(green.clone()),
        bounds(28.0, 2.0, 20.0, 24.0),
    ));
    scene.insert_primitive(quad(bounds(0.0, 12.0, 52.0, 4.0), gpui::white()));
    scene.insert_primitive(draw(GpuSource::Fixed(red), bounds(0.0, 2.0, 20.0, 24.0)));
    // An unprepared dynamic draw is also a boundary, without retaining its frame.
    let empty = GpuState::new(green.clone(), limits())?;
    empty.clear();
    scene.insert_primitive(draw(GpuSource::Dynamic(empty), viewport));
    scene.insert_primitive(draw(GpuSource::Fixed(green), bounds(28.0, 2.0, 20.0, 24.0)));
    scene.finish();
    assert_ne!(scene.gpu_draws[0].order, scene.gpu_draws[1].order);
    assert_eq!(
        scene
            .batches()
            .filter(|batch| matches!(batch, PrimitiveBatch::Gpu(_)))
            .count(),
        2,
        "the ordinary quad must split GPU batches"
    );
    let actual = compare_individual_material_draws(&mut renderer, &scene, size(64, 28))?;
    pixel(&actual, 50, 13, [255; 4]);
    pixel(&actual, 60, 20, BLUE);
    Ok(())
}

#[test]
fn palette_path_batches_keep_fractional_bounds_on_individual_passes() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let material = palette_curve(&[[1.0, 0.0, 0.0, 0.65], [0.0, 1.0, 0.0, 0.8]])?;
    let viewport = bounds(0.0, 0.0, 104.0, 52.0);
    let mut scene = Scene::default();
    scene.insert_primitive(quad(viewport, gpui::blue()));
    scene.push_layer(viewport);
    for y in [0.0, 26.0] {
        scene.insert_primitive(draw(
            GpuSource::Fixed(material.clone()),
            bounds(0.0, y + 2.0, 18.0, 22.0),
        ));
        for (index, fraction) in [0.499, 0.5, 0.501, 0.75].into_iter().enumerate() {
            let x = 20.0 + index as f32 * 21.0;
            let mut record = draw(
                GpuSource::Fixed(material.clone()),
                bounds(x, y + 2.0, 18.0, 22.0),
            );
            if y == 0.0 {
                record.bounds.origin.x += ScaledPixels(fraction);
                record.bounds.origin.y += ScaledPixels(fraction);
                record.content_mask.bounds = viewport;
            } else {
                record.content_mask.bounds = bounds(x + fraction, y + 3.0 + fraction, 16.0, 20.0);
            }
            assert!(!WgpuRendererCore::gpu_path_bounds_are_integer(
                record.bounds.intersect(&record.content_mask.bounds)
            ));
            scene.insert_primitive(record);
        }
    }
    scene.pop_layer();
    scene.finish();
    let actual = compare_individual_material_draws(&mut renderer, &scene, size(104, 52))?;
    assert!(
        actual
            .pixels()
            .any(|pixel| pixel.0[0] > 10 && pixel.0[2] < 245)
    );
    Ok(())
}
