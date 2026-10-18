use super::*;
use gpui::{
    ContentMask, GpuRectangle, GpuRectangleBatch, GpuRectangleFrame, GpuRectangleLimits,
    GpuRectangleParams, GpuRectangleSource, GpuRectangleState, PlatformHeadlessRenderer,
};

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const BLACK: [u8; 4] = [0, 0, 0, 255];
fn limits() -> GpuRectangleLimits {
    GpuRectangleLimits {
        max_rectangles: 1024,
        max_palette_colors: 16,
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
fn frame(color: [f32; 4]) -> Arc<GpuRectangleFrame> {
    GpuRectangleFrame::new(
        &[rectangle()],
        &[color],
        GpuRectangleParams::default(),
        limits(),
    )
    .unwrap()
}
fn draw(source: GpuRectangleSource, placement: Bounds<ScaledPixels>) -> GpuRectangleBatch {
    GpuRectangleBatch {
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
fn gpu_rectangles_order_mask_opacity_and_shared_upload() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let red = frame([1.0, 0.0, 0.0, 1.0]);
    let mut scene = Scene::default();
    scene.insert_primitive(quad(bounds(0.0, 0.0, 32.0, 16.0), gpui::blue()));
    let mut first = draw(
        GpuRectangleSource::Fixed(red.clone()),
        bounds(0.0, 0.0, 16.0, 16.0),
    );
    first.content_mask.bounds = bounds(4.0, 0.0, 12.0, 16.0);
    first.opacity = 0.5;
    scene.insert_primitive(first);
    scene.insert_primitive(draw(
        GpuRectangleSource::Fixed(red.clone()),
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
    assert_eq!(renderer.core.resources.rectangles.stats().3, 1);
    assert_eq!(
        Arc::strong_count(&red),
        cpu_owners,
        "GPU cache must not retain CPU frames"
    );
    renderer.render_scene_to_image(&scene, size(32, 16))?;
    assert_eq!(
        renderer.core.resources.rectangles.stats().3,
        1,
        "static replay must not upload again"
    );
    let image = renderer.render_scene_to_image(&scene, size(48, 24))?;
    pixel(&image, 20, 8, RED);
    pixel(&image, 40, 20, BLACK);
    assert_eq!(
        renderer.core.resources.rectangles.stats().3,
        1,
        "resize changes placement, not immutable content"
    );
    Ok(())
}

#[test]
fn gpu_rectangles_palette_alpha_segments_and_scale() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let gradient = GpuRectangleFrame::new(
        &[rectangle()],
        &[[0.0, 0.0, 1.0, 1.0], [1.0, 0.0, 0.0, 1.0]],
        GpuRectangleParams::default(),
        limits(),
    )?;
    let segmented = GpuRectangleFrame::new(
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
        GpuRectangleSource::Fixed(gradient),
        bounds(0.0, 0.0, 8.0, 16.0),
    ));
    let mut segments = draw(
        GpuRectangleSource::Fixed(segmented),
        bounds(8.0, 0.0, 8.0, 16.0),
    );
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
fn gpu_rectangles_fixed_dynamic_coherence_clear_and_retirement() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let initial = frame([1.0, 0.0, 0.0, 1.0]);
    let state = GpuRectangleState::new(initial.clone(), limits())?;
    let mut scene = Scene::default();
    scene.insert_primitive(draw(
        GpuRectangleSource::Fixed(initial.clone()),
        bounds(0.0, 0.0, 8.0, 8.0),
    ));
    for x in [8.0, 16.0] {
        scene.insert_primitive(draw(
            GpuRectangleSource::Dynamic(state.clone()),
            bounds(x, 0.0, 8.0, 8.0),
        ));
    }
    scene.finish();
    renderer.render_scene_to_image(&scene, size(24, 8))?;
    assert_eq!(renderer.core.resources.rectangles.stats().3, 1);
    assert_eq!(renderer.core.resources.rectangles.stats().4, 1);
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
    state.publish(&instances, &palette, GpuRectangleParams::default())?;
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
    assert_eq!(renderer.core.resources.rectangles.stats().4, 2);
    state.publish(&[], &[[0.0, 1.0, 0.0, 1.0]], GpuRectangleParams::default())?;
    let image = renderer.render_scene_to_image(&scene, size(24, 8))?;
    pixel(&image, 9, 4, BLACK);
    assert_eq!(
        renderer.core.resources.rectangles.stats().3,
        2,
        "empty content does not allocate"
    );
    state.clear();
    let image = renderer.render_scene_to_image(&scene, size(24, 8))?;
    pixel(&image, 4, 4, RED);
    pixel(&image, 17, 4, BLACK);
    assert_eq!(
        renderer.core.resources.rectangles.stats().4,
        4,
        "revoked slots also resolve once"
    );
    state.publish(&instances[..1], &palette, GpuRectangleParams::default())?;
    let image = renderer.render_scene_to_image(&scene, size(24, 8))?;
    pixel(&image, 17, 4, [0, 255, 0, 255]);
    drop(scene);
    let empty = Scene::default();
    renderer.render_scene_to_image(&empty, size(24, 8))?;
    let stats = renderer.core.resources.rectangles.stats();
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
    let large = GpuRectangleFrame::new(
        &vec![rectangle(); 512],
        &[[0.0, 1.0, 0.0, 1.0]],
        GpuRectangleParams::default(),
        limits(),
    )?;
    renderer.core.resources.rectangles.set_byte_limit(1024);
    let mut scene = Scene::default();
    scene.insert_primitive(quad(bounds(0.0, 0.0, 24.0, 8.0), gpui::blue()));
    scene.insert_primitive(draw(
        GpuRectangleSource::Fixed(large),
        bounds(0.0, 0.0, 8.0, 8.0),
    ));
    scene.insert_primitive(draw(
        GpuRectangleSource::Fixed(good.clone()),
        bounds(8.0, 0.0, 8.0, 8.0),
    ));
    let mut invalid = draw(GpuRectangleSource::Fixed(good), bounds(16.0, 0.0, 8.0, 8.0));
    invalid.scale_factor = f32::NAN;
    scene.insert_primitive(invalid);
    scene.finish();
    let image = renderer.render_scene_to_image(&scene, size(24, 8))?;
    pixel(&image, 4, 4, BLUE);
    pixel(&image, 12, 4, RED);
    pixel(&image, 20, 4, BLUE);
    let stats = renderer.core.resources.rectangles.stats();
    assert_eq!(stats.0, 1);
    assert!(stats.2 <= 1024);
    renderer.core.resources.rectangles.disable_program();
    let image = renderer.render_scene_to_image(&scene, size(24, 8))?;
    pixel(&image, 12, 4, BLUE);
    Ok(())
}

#[test]
fn gpu_rectangles_device_generation_rebuilds_from_owned_content() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let red = frame([1.0, 0.0, 0.0, 1.0]);
    let mut scene = Scene::default();
    scene.insert_primitive(draw(
        GpuRectangleSource::Fixed(red),
        bounds(0.0, 0.0, 8.0, 8.0),
    ));
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
    assert_eq!(renderer.core.resources.rectangles.stats().0, 0);
    let image = renderer.render_scene_to_image(&scene, size(8, 8))?;
    pixel(&image, 4, 4, RED);
    assert_eq!(renderer.core.resources.rectangles.stats().3, 1);
    Ok(())
}

#[test]
fn gpu_rectangles_recording_error_retires_prepared_storage_and_recovers() -> anyhow::Result<()> {
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let red = frame([1.0, 0.0, 0.0, 1.0]);
    let mut scene = Scene::default();
    scene.insert_primitive(draw(
        GpuRectangleSource::Fixed(red.clone()),
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
        renderer.core.resources.rectangles.stats().3,
        1,
        "failure must occur after preparation"
    );
    renderer.core.instance_data_capacity = capacity;
    renderer.core.max_instance_data_size = maximum;
    let mut valid = Scene::default();
    valid.insert_primitive(draw(
        GpuRectangleSource::Fixed(red),
        bounds(0.0, 0.0, 8.0, 8.0),
    ));
    valid.finish();
    let image = renderer.render_scene_to_image(&valid, size(8, 8))?;
    pixel(&image, 4, 4, RED);
    renderer.render_scene_to_image(&Scene::default(), size(8, 8))?;
    let stats = renderer.core.resources.rectangles.stats();
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
        let mut batch = draw(
            GpuRectangleSource::Fixed(frame(color)),
            bounds(0.0, 0.0, 8.0, 8.0),
        );
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
