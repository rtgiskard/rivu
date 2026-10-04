use std::rc::Rc;

use gpui::{
    AnyElement, Background, Path, PathBuilder, Pixels, canvas, div, linear_color_stop,
    linear_gradient, point, prelude::*, px, rgb,
};

use super::{ACCENT, BG};

pub(super) struct Artwork {
    layers: Rc<Vec<(Path<Pixels>, Background)>>,
}

impl Artwork {
    pub(super) fn new() -> Self {
        let mut grooves = PathBuilder::stroke(px(1.5));
        for radius in [184., 196., 208., 220.] {
            circle(&mut grooves, 256., 256., radius);
        }
        let mut record = PathBuilder::stroke(px(24.));
        circle(&mut record, 256., 236., 120.);
        let mut inner = PathBuilder::stroke(px(6.));
        circle(&mut inner, 256., 236., 78.);
        let mut spindle = PathBuilder::fill();
        circle(&mut spindle, 256., 236., 16.);
        let layers = vec![
            (build(grooves, false), rgb(ACCENT).alpha(0.09).into()),
            (build(record, true), gradient(ACCENT, 0xbb9af7)),
            (build(inner, true), rgb(ACCENT).alpha(0.28).into()),
            (build(spindle, true), rgb(0x73daca).into()),
            (river(56., false), rgb(BG).into()),
            (river(28., false), gradient(0x7dcfff, 0x73daca)),
            (river(16., true), gradient(0x7dcfff, 0x73daca)),
        ];
        Self {
            layers: Rc::new(layers),
        }
    }

    pub(super) fn element(&self) -> AnyElement {
        let layers = Rc::clone(&self.layers);
        div()
            .size_full()
            .rounded_md()
            .overflow_hidden()
            .bg(rgb(BG))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let side = bounds.size.width.min(bounds.size.height);
                        let scale = side / px(512.);
                        let origin = bounds.origin
                            + point(
                                (bounds.size.width - side) / 2.,
                                (bounds.size.height - side) / 2.,
                            );
                        // Tessellation is cached. GPUI consumes each painted path,
                        // so only the scene's vertex copy and positioning repeat.
                        for (geometry, color) in layers.iter() {
                            let mut path = geometry.clone();
                            path.bounds.origin = origin + path.bounds.origin * scale;
                            path.bounds.size = path.bounds.size.map(|dimension| dimension * scale);
                            for vertex in &mut path.vertices {
                                vertex.xy_position = origin + vertex.xy_position * scale;
                            }
                            window.paint_path(path, *color);
                        }
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
}

fn gradient(from: u32, to: u32) -> Background {
    linear_gradient(
        135.,
        linear_color_stop(rgb(from), 0.),
        linear_color_stop(rgb(to), 1.),
    )
}

fn build(mut builder: PathBuilder, motif: bool) -> Path<Pixels> {
    if motif {
        builder.scale(0.75);
        builder.translate(point(px(64.), px(52.)));
    }
    builder
        .build()
        .expect("fixed artwork geometry must tessellate")
}

fn circle(builder: &mut PathBuilder, x: f32, y: f32, radius: f32) {
    let k = radius * 0.552_284_8;
    let p = |dx, dy| point(px(x + dx), px(y + dy));
    builder.move_to(p(radius, 0.));
    builder.cubic_bezier_to(p(0., radius), p(radius, k), p(k, radius));
    builder.cubic_bezier_to(p(-radius, 0.), p(-k, radius), p(-radius, k));
    builder.cubic_bezier_to(p(0., -radius), p(-radius, -k), p(-k, -radius));
    builder.cubic_bezier_to(p(radius, 0.), p(k, -radius), p(radius, -k));
    builder.close();
}

fn river(width: f32, lower: bool) -> Path<Pixels> {
    let p = |x, y| point(px(x), px(y));
    let mut line = PathBuilder::stroke(px(width * 0.75));
    let endpoints = if lower {
        line.move_to(p(136., 382.));
        line.cubic_bezier_to(p(244., 382.), p(176., 350.), p(208., 354.));
        line.cubic_bezier_to(p(364., 376.), p(280., 410.), p(320., 410.));
        [(136., 382.), (364., 376.)]
    } else {
        line.move_to(p(116., 324.));
        line.cubic_bezier_to(p(244., 324.), p(158., 282.), p(202., 282.));
        line.cubic_bezier_to(p(396., 300.), p(286., 366.), p(330., 366.));
        [(116., 324.), (396., 300.)]
    };
    let mut path = build(line, true);
    let mut caps = PathBuilder::fill();
    for (x, y) in endpoints {
        circle(&mut caps, x, y, width / 2.);
    }
    let caps = build(caps, true);
    path.bounds = path.bounds.union(&caps.bounds);
    path.vertices.extend(caps.vertices);
    path
}
