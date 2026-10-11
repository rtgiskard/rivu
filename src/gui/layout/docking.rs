//! Measured, bounded docking strips; layout edits remain in the layout model.
use super::layout_geometry::node_id;
use super::{Axis, Edge, Node, split_geometry};
use gpui::{Bounds, Pixels, Point, point, px, size};

const MAX_BARS: usize = 4;
const ENTER: f32 = 24.;
const EXIT_MARGIN: f32 = 12.;
const MIN_LENGTH: f32 = 120.;
const INSET: f32 = 4.;
const GAP: f32 = 4.;
const THICKNESS: [f32; MAX_BARS] = [22., 20., 18., 16.];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) struct Target {
    pub node: u64,
    pub edge: Edge,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::gui) struct Bar {
    pub target: Target,
    pub bounds: Bounds<Pixels>,
    pub depth: usize,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::gui) struct Guide {
    pub bars: [Option<Bar>; MAX_BARS],
    pub leaf: u64,
    pub edge: Edge,
    pub anchor: Bounds<Pixels>,
}

/// Hidden bars have no tab targets, even when a previous frame measured them.
pub(super) fn tab_index(
    node: &Node,
    pointer: Point<Pixels>,
    bounds: &impl Fn(u64) -> Option<Bounds<Pixels>>,
) -> Option<usize> {
    let Node::Tabs { panels, active, .. } = node else {
        return None;
    };
    if !panels
        .iter()
        .find(|panel| panel.id == *active)?
        .show_tab_bar
    {
        return None;
    }
    panels
        .iter()
        .position(|panel| bounds(panel.id).is_some_and(|rect| rect.contains(&pointer)))
}

impl Guide {
    pub fn build(
        root: &Node,
        pointer: Point<Pixels>,
        ui_scale: f32,
        bounds: &impl Fn(u64) -> Option<Bounds<Pixels>>,
    ) -> Option<Self> {
        if !ui_scale.is_finite() || ui_scale <= 0. {
            return None;
        }
        // Descend through measured children, not overlay hitboxes. A missing sibling
        // measurement could hide an ambiguous hit, so it also disables this guide.
        let mut path = [None; MAX_BARS];
        let mut node = root;
        let mut rect = bounds(node_id(node))?;
        let mut depth = 0;
        let leaf = loop {
            if depth == MAX_BARS || !valid(rect) || !inside(rect, pointer) {
                return None;
            }
            path[depth] = Some((node_id(node), rect));
            depth += 1;
            let Node::Split { children, .. } = node else {
                break node_id(node);
            };
            let mut selected = None;
            for child in children {
                let child_bounds = bounds(node_id(child))?;
                if !valid(child_bounds) {
                    return None;
                }
                if inside(child_bounds, pointer) {
                    if selected.is_some() {
                        return None;
                    }
                    selected = Some((child, child_bounds));
                }
            }
            (node, rect) = selected?;
        };
        let anchor = rect;
        let (x, y, width, height) = components(anchor);
        let dx = f32::from(pointer.x) - x;
        let dy = f32::from(pointer.y) - y;
        let distances = [
            (Edge::Left, dx),
            (Edge::Right, width - dx),
            (Edge::Top, dy),
            (Edge::Bottom, height - dy),
        ];
        let mut edge = None;
        for (side, distance) in distances {
            if distance <= ENTER * ui_scale {
                // Reject corners and narrow regions with two nearby sides rather
                // than assigning a tie to whichever side was visited first.
                if edge.is_some() {
                    return None;
                }
                edge = Some(side);
            }
        }
        let edge = edge?;
        let mut guide = Self {
            bars: [None; MAX_BARS],
            leaf,
            edge,
            anchor,
        };
        let mut count = 0;
        let mut offset = INSET * ui_scale;
        for (depth, entry) in path.iter().enumerate() {
            let Some((id, owner)) = *entry else {
                break;
            };
            if (edge_coordinate(owner, edge) - edge_coordinate(anchor, edge)).abs() > ui_scale {
                continue;
            }
            let (x, y, width, height) = components(owner);
            let trim = ENTER * ui_scale;
            let thickness = THICKNESS[count] * ui_scale;
            let (bar_x, bar_y, bar_width, bar_height) = match edge {
                Edge::Left => (x + offset, y + trim, thickness, height - 2. * trim),
                Edge::Right => (
                    x + width - offset - thickness,
                    y + trim,
                    thickness,
                    height - 2. * trim,
                ),
                Edge::Top => (x + trim, y + offset, width - 2. * trim, thickness),
                Edge::Bottom => (
                    x + trim,
                    y + height - offset - thickness,
                    width - 2. * trim,
                    thickness,
                ),
                Edge::Center => return None,
            };
            let length = match edge {
                Edge::Left | Edge::Right => bar_height,
                _ => bar_width,
            };
            if length < MIN_LENGTH * ui_scale
                || bar_x < x
                || bar_y < y
                || bar_x + bar_width > x + width
                || bar_y + bar_height > y + height
            {
                return None;
            }
            let bar_bounds = Bounds::new(
                point(px(bar_x), px(bar_y)),
                size(px(bar_width), px(bar_height)),
            );
            // The full stack must leave at least half of the hovered leaf usable.
            // Measure against the leaf edge to account for the shared-edge tolerance.
            let normal_size = match edge {
                Edge::Left | Edge::Right => f32::from(anchor.size.width),
                _ => f32::from(anchor.size.height),
            };
            if inward_extent(bar_bounds, anchor, edge) > normal_size * 0.5 {
                return None;
            }
            for previous in guide.bars.iter().flatten() {
                if overlaps(previous.bounds, bar_bounds) {
                    return None;
                }
            }
            guide.bars[count] = Some(Bar {
                target: Target { node: id, edge },
                bounds: bar_bounds,
                depth,
            });
            count += 1;
            offset += thickness + GAP * ui_scale;
        }
        Some(guide)
    }

    pub fn contains(&self, pointer: Point<Pixels>, ui_scale: f32) -> bool {
        if !ui_scale.is_finite() || ui_scale <= 0. {
            return false;
        }
        // Whole-container strips can extend along the edge beyond the hovered
        // leaf. Their painted area must remain reachable without rebuilding.
        if self.target(pointer).is_some() {
            return true;
        }
        let (x, y, width, height) = components(self.anchor);
        let px = f32::from(pointer.x);
        let py = f32::from(pointer.y);
        let inward = match self.edge {
            Edge::Left => px - x,
            Edge::Right => x + width - px,
            Edge::Top => py - y,
            Edge::Bottom => y + height - py,
            Edge::Center => return false,
        };
        let extent = self
            .bars
            .iter()
            .flatten()
            .map(|bar| inward_extent(bar.bounds, self.anchor, self.edge))
            .fold(ENTER * ui_scale, f32::max);
        // Keep gaps and the complete stack active, but release unpainted corners
        // to the neighboring side. Rebuilding at each band would change ownership.
        self.bars.iter().flatten().any(|bar| {
            let (x, y, width, height) = components(bar.bounds);
            match self.edge {
                Edge::Left | Edge::Right => py >= y && py <= y + height,
                Edge::Top | Edge::Bottom => px >= x && px <= x + width,
                Edge::Center => false,
            }
        }) && inward >= -EXIT_MARGIN * ui_scale
            && inward <= extent + EXIT_MARGIN * ui_scale
    }

    pub fn target(&self, pointer: Point<Pixels>) -> Option<Target> {
        self.bars
            .iter()
            .flatten()
            .find(|bar| inside(bar.bounds, pointer))
            .map(|bar| bar.target)
    }
}

fn components(bounds: Bounds<Pixels>) -> (f32, f32, f32, f32) {
    (
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.y),
        f32::from(bounds.size.width),
        f32::from(bounds.size.height),
    )
}

fn valid(bounds: Bounds<Pixels>) -> bool {
    let (x, y, width, height) = components(bounds);
    x.is_finite()
        && y.is_finite()
        && width.is_finite()
        && height.is_finite()
        && width > 0.
        && height > 0.
        && (x + width).is_finite()
        && (y + height).is_finite()
}

fn inside(bounds: Bounds<Pixels>, pointer: Point<Pixels>) -> bool {
    let (x, y, width, height) = components(bounds);
    let px = f32::from(pointer.x);
    let py = f32::from(pointer.y);
    // Half-open rectangles give an exact shared boundary only one owner.
    px >= x && px < x + width && py >= y && py < y + height
}

fn edge_coordinate(bounds: Bounds<Pixels>, edge: Edge) -> f32 {
    let (x, y, width, height) = components(bounds);
    match edge {
        Edge::Left => x,
        Edge::Right => x + width,
        Edge::Top => y,
        Edge::Bottom => y + height,
        Edge::Center => unreachable!("a docking strip has no center edge"),
    }
}

fn inward_extent(bounds: Bounds<Pixels>, anchor: Bounds<Pixels>, edge: Edge) -> f32 {
    let (x, y, width, height) = components(bounds);
    let coordinate = edge_coordinate(anchor, edge);
    match edge {
        Edge::Left => x + width - coordinate,
        Edge::Right => coordinate - x,
        Edge::Top => y + height - coordinate,
        Edge::Bottom => coordinate - y,
        Edge::Center => unreachable!("a docking strip has no center edge"),
    }
}

fn overlaps(a: Bounds<Pixels>, b: Bounds<Pixels>) -> bool {
    let (ax, ay, aw, ah) = components(a);
    let (bx, by, bw, bh) = components(b);
    ax < bx + bw && bx < ax + aw && ay < by + bh && by < ay + ah
}

/// Project a node from the edited layout, using the same cumulative rounding and
/// gutters as rendering. Old measurements cannot describe source collapse.
pub(super) fn node_bounds(
    root: &Node,
    outer: Bounds<Pixels>,
    target: u64,
    gutter: f64,
    scale: f64,
) -> Option<Bounds<Pixels>> {
    fn project(
        node: &Node,
        outer: Bounds<Pixels>,
        target: u64,
        gutter: f64,
        scale: f64,
        depth: usize,
    ) -> Option<Bounds<Pixels>> {
        // The layout model permits at most 32 split levels.
        if depth > super::MAX_SPLIT_DEPTH {
            return None;
        }
        if node_id(node) == target {
            return Some(outer);
        }
        let Node::Split {
            axis,
            children,
            boundaries,
            ..
        } = node
        else {
            return None;
        };
        let length = match axis {
            Axis::Horizontal => f64::from(f32::from(outer.size.width)),
            Axis::Vertical => f64::from(f32::from(outer.size.height)),
        };
        for (child, (start, length)) in children.iter().zip(split_geometry(
            boundaries,
            children.len(),
            length,
            gutter,
            scale,
        )) {
            let mut child_bounds = outer;
            match axis {
                Axis::Horizontal => {
                    child_bounds.origin.x += px(start as f32);
                    child_bounds.size.width = px(length as f32);
                }
                Axis::Vertical => {
                    child_bounds.origin.y += px(start as f32);
                    child_bounds.size.height = px(length as f32);
                }
            }
            if let Some(found) = project(child, child_bounds, target, gutter, scale, depth + 1) {
                return Some(found);
            }
        }
        None
    }
    if !valid(outer) {
        return None;
    }
    project(root, outer, target, gutter, scale, 0)
}

#[cfg(test)]
mod tests {
    use super::super::{Axis, Panel};
    use super::*;

    fn tabs(id: u64) -> Node {
        Node::Tabs {
            id,
            active: id + 100,
            panels: vec![Panel {
                id: id + 100,
                kind: "library".into(),
                show_tab_bar: true,
                audio_visualizer: None,
            }],
        }
    }

    fn split(id: u64, axis: Axis, children: Vec<Node>) -> Node {
        Node::Split {
            id,
            axis,
            children,
            boundaries: Vec::new(),
        }
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    fn pointer(x: f32, y: f32) -> Point<Pixels> {
        point(px(x), px(y))
    }

    fn center(bounds: Bounds<Pixels>) -> Point<Pixels> {
        let (x, y, width, height) = components(bounds);
        pointer(x + width * 0.5, y + height * 0.5)
    }

    #[test]
    fn hidden_bar_ignores_stale_tab_hits_until_a_visible_tab_is_active() {
        let mut node = tabs(1);
        let Node::Tabs { panels, .. } = &mut node else {
            unreachable!()
        };
        let mut second = panels[0].clone();
        second.id = 102;
        panels.push(second);
        let bounds = |id| {
            Some(match id {
                101 => rect(10., 5., 90., 30.),
                102 => rect(104., 5., 90., 30.),
                _ => return None,
            })
        };
        let position = pointer(140., 30.);
        assert_eq!(tab_index(&node, position, &bounds), Some(1));
        let Node::Tabs { panels, .. } = &mut node else {
            unreachable!()
        };
        panels[0].show_tab_bar = false;
        assert_eq!(tab_index(&node, position, &bounds), None);
        let Node::Tabs { active, .. } = &mut node else {
            unreachable!()
        };
        *active = 102;
        assert_eq!(tab_index(&node, position, &bounds), Some(1));
        assert_eq!(tab_index(&node, pointer(210., 30.), &bounds), None);
    }

    fn rows() -> Node {
        split(
            1,
            Axis::Vertical,
            vec![
                split(2, Axis::Horizontal, vec![tabs(3), tabs(4)]),
                split(5, Axis::Horizontal, vec![tabs(6), tabs(7)]),
            ],
        )
    }

    fn row_bounds(id: u64) -> Option<Bounds<Pixels>> {
        Some(match id {
            1 => rect(0., 0., 804., 604.),
            2 => rect(0., 0., 804., 300.),
            3 => rect(0., 0., 400., 300.),
            4 => rect(404., 0., 400., 300.),
            5 => rect(0., 304., 804., 300.),
            6 => rect(0., 304., 400., 300.),
            7 => rect(404., 304., 400., 300.),
            _ => return None,
        })
    }

    #[test]
    fn shared_edge_targets_whole_row_then_panel_not_adjacent_root_or_branch() {
        let guide = Guide::build(&rows(), pointer(200., 306.), 1., &row_bounds).unwrap();
        assert_eq!(guide.leaf, 6);
        assert_eq!(guide.edge, Edge::Top);
        let row = guide.bars[0].unwrap();
        let panel = guide.bars[1].unwrap();
        assert_eq!(
            row.target,
            Target {
                node: 5,
                edge: Edge::Top
            }
        );
        assert_eq!(
            panel.target,
            Target {
                node: 6,
                edge: Edge::Top
            }
        );
        assert_eq!((row.depth, panel.depth), (1, 2));
        assert!(row.bounds.size.height > panel.bounds.size.height);
        assert!(row.bounds.origin.y < panel.bounds.origin.y);
        assert_eq!(guide.target(center(row.bounds)), Some(row.target));
        assert_eq!(guide.target(center(panel.bounds)), Some(panel.target));
        assert!(guide.bars[2].is_none());
        assert!(!overlaps(row.bounds, panel.bounds));
    }

    #[test]
    fn retention_reaches_inner_bands_without_reordering_and_covers_gaps() {
        let guide = Guide::build(&rows(), pointer(200., 2.), 1., &row_bounds).unwrap();
        assert_eq!(guide.bars.iter().flatten().count(), 3);
        for bar in guide.bars.iter().flatten() {
            assert!(guide.contains(center(bar.bounds), 1.));
            assert_eq!(guide.target(center(bar.bounds)), Some(bar.target));
        }
        // The deepest strip is well beyond the enter distance. A fresh build
        // would produce no guide, so the caller must retain this arrangement.
        let inner = guide.bars[2].unwrap();
        assert!(Guide::build(&rows(), center(inner.bounds), 1., &row_bounds).is_none());
        assert!(guide.contains(pointer(200., 28.), 1.));
        assert!(guide.target(pointer(200., 28.)).is_none());
        assert!(guide.contains(pointer(600., 15.), 1.));
        assert!(!guide.contains(pointer(200., 100.), 1.));
    }

    #[test]
    fn ancestor_strip_retains_exit_margin_across_another_row() {
        let guide = Guide::build(&rows(), pointer(2., 150.), 1., &row_bounds).unwrap();
        assert_eq!(
            guide.target(pointer(25., 450.)),
            Some(Target {
                node: 1,
                edge: Edge::Left
            })
        );
        assert!(guide.contains(pointer(27., 450.), 1.));
        assert!(guide.target(pointer(27., 450.)).is_none());
        assert!(Guide::build(&rows(), pointer(27., 450.), 1., &row_bounds).is_none());
        assert!(guide.contains(pointer(25., 450.), 1.));
        assert!(!guide.contains(pointer(27., 2.), 1.));
    }

    #[test]
    fn all_four_sides_are_trimmed_disjoint_and_inside_their_owners() {
        for (position, edge) in [
            (pointer(200., 2.), Edge::Top),
            (pointer(200., 602.), Edge::Bottom),
            (pointer(2., 150.), Edge::Left),
            (pointer(802., 150.), Edge::Right),
        ] {
            let guide = Guide::build(&rows(), position, 1., &row_bounds).unwrap();
            assert_eq!(guide.edge, edge);
            for (index, bar) in guide.bars.iter().flatten().enumerate() {
                let owner = row_bounds(bar.target.node).unwrap();
                let (x, y, width, height) = components(owner);
                let (bx, by, bw, bh) = components(bar.bounds);
                assert!(bx >= x && by >= y && bx + bw <= x + width && by + bh <= y + height);
                match edge {
                    Edge::Left | Edge::Right => {
                        assert!(by >= y + ENTER && by + bh <= y + height - ENTER);
                    }
                    _ => assert!(bx >= x + ENTER && bx + bw <= x + width - ENTER),
                }
                for other in guide.bars.iter().flatten().skip(index + 1) {
                    assert!(!overlaps(bar.bounds, other.bounds));
                }
            }
        }
    }

    #[test]
    fn corners_release_to_neighboring_sides_without_stealing_hits() {
        let root = tabs(1);
        let bounds = |_: u64| Some(rect(0., 0., 400., 300.));
        for corner in [
            pointer(2., 2.),
            pointer(398., 2.),
            pointer(2., 298.),
            pointer(398., 298.),
        ] {
            assert!(Guide::build(&root, corner, 1., &bounds).is_none());
        }
        let top = Guide::build(&root, pointer(200., 2.), 1., &bounds).unwrap();
        assert!(!top.contains(pointer(2., 35.), 1.));
        assert!(top.target(pointer(2., 10.)).is_none());
        let left = Guide::build(&root, pointer(2., 35.), 1., &bounds).unwrap();
        assert_eq!(left.edge, Edge::Left);
        assert_eq!(
            left.target(pointer(10., 35.)),
            Some(Target {
                node: 1,
                edge: Edge::Left
            })
        );
        assert!(top.contains(pointer(200., 35.), 1.));
        assert!(!top.contains(pointer(200., 39.), 1.));
    }

    #[test]
    fn four_path_nodes_work_but_five_skip_even_with_few_shared_edges() {
        let four = split(
            1,
            Axis::Horizontal,
            vec![split(
                2,
                Axis::Vertical,
                vec![split(3, Axis::Horizontal, vec![tabs(4)])],
            )],
        );
        let bounds = |_: u64| Some(rect(0., 0., 500., 500.));
        let guide = Guide::build(&four, pointer(250., 2.), 1., &bounds).unwrap();
        assert_eq!(guide.bars.iter().flatten().count(), 4);
        let five = split(5, Axis::Vertical, vec![four]);
        assert!(Guide::build(&five, pointer(250., 2.), 1., &bounds).is_none());
        let staggered = |id: u64| Some(rect(0., id as f32 * 2., 500., 500.));
        assert!(Guide::build(&five, pointer(250., 10.), 1., &staggered).is_none());
    }

    #[test]
    fn dense_and_short_regions_skip_the_entire_guide() {
        let root = split(1, Axis::Horizontal, vec![tabs(2)]);
        let dense = |_: u64| Some(rect(0., 0., 400., 90.));
        assert!(Guide::build(&root, pointer(200., 2.), 1., &dense).is_none());
        let short = |_: u64| Some(rect(0., 0., 167., 400.));
        assert!(Guide::build(&root, pointer(80., 2.), 1., &short).is_none());
        let threshold = |_: u64| Some(rect(0., 0., 168., 400.));
        assert!(Guide::build(&root, pointer(80., 2.), 1., &threshold).is_some());
    }

    #[test]
    fn missing_ambiguous_and_gutter_measurements_skip_guides() {
        let root = rows();
        let missing = |id| if id == 7 { None } else { row_bounds(id) };
        assert!(Guide::build(&root, pointer(200., 306.), 1., &missing).is_none());
        let overlapping = |id| {
            if id == 7 {
                row_bounds(6)
            } else {
                row_bounds(id)
            }
        };
        assert!(Guide::build(&root, pointer(200., 306.), 1., &overlapping).is_none());
        assert!(Guide::build(&root, pointer(402., 306.), 1., &row_bounds).is_none());
        let invalid = |_: u64| Some(rect(0., 0., f32::NAN, 300.));
        assert!(Guide::build(&root, pointer(200., 2.), 1., &invalid).is_none());
    }

    #[test]
    fn shared_edge_tolerance_and_geometry_scale_together() {
        let root = split(1, Axis::Horizontal, vec![tabs(2)]);
        let bounds = |id| {
            Some(if id == 1 {
                rect(0., 0., 800., 600.)
            } else {
                rect(0., 1.5, 800., 598.5)
            })
        };
        let scaled = Guide::build(&root, pointer(400., 4.), 2., &bounds).unwrap();
        assert_eq!(scaled.bars.iter().flatten().count(), 2);
        assert_eq!(scaled.bars[0].unwrap().bounds.size.height, px(44.));
        let unscaled = Guide::build(&root, pointer(400., 4.), 1., &bounds).unwrap();
        assert_eq!(unscaled.bars.iter().flatten().count(), 1);
        assert_eq!(unscaled.bars[0].unwrap().target.node, 2);
    }

    #[test]
    fn preview_projects_new_tabs_after_source_collapse_and_gutter_reconciliation() {
        use super::super::Layout;
        use super::super::layout_geometry::{edit_sizes, find};
        use std::collections::HashMap;

        let root = split(
            1,
            Axis::Horizontal,
            vec![tabs(2), split(5, Axis::Vertical, vec![tabs(6), tabs(8)])],
        );
        let measured = HashMap::from([
            (1, (1004., 404.)),
            (2, (600., 404.)),
            (5, (400., 404.)),
            (6, (400., 200.)),
            (8, (400., 200.)),
        ]);
        let sizes = edit_sizes(&root, measured, 102, Some((6, Edge::Right, 1000)), 4.);
        let mut edited = Layout {
            root: Some(root),
            next_id: 1000,
            locked: false,
        };
        assert!(edited.move_panel(102, 6, Edge::Right, None));
        edited.preserve_region_sizes(&sizes, 4.);
        let root = edited.root.as_ref().unwrap();
        assert_eq!(node_id(root), 5);
        assert!(find(root, 2).is_none());
        let outer = rect(10., 20., 1004., 404.);
        assert_eq!(
            node_bounds(root, outer, 1000, 4., 1.),
            Some(rect(514., 20., 500., 200.))
        );
        assert_eq!(
            node_bounds(root, outer, 6, 4., 1.),
            Some(rect(10., 20., 500., 200.))
        );
        assert_eq!(
            node_bounds(root, outer, 8, 4., 1.),
            Some(rect(10., 224., 1004., 200.))
        );
        assert_eq!(node_bounds(root, outer, 1, 4., 1.), None);
    }

    #[test]
    fn preview_uses_cumulative_physical_rounding_for_nested_splits() {
        let mut root = split(
            1,
            Axis::Horizontal,
            vec![tabs(2), split(3, Axis::Vertical, vec![tabs(4), tabs(5)])],
        );
        let Node::Split { boundaries, .. } = &mut root else {
            unreachable!()
        };
        *boundaries = vec![0.3];
        let outer = rect(10., 20., 101., 81.);
        // At scale 2, gutter 3.3 rounds to 3.5; horizontal boundary
        // rounds to 29.5. The second vertical child starts at 39 + 3.5.
        assert_eq!(
            node_bounds(&root, outer, 5, 3.3, 2.),
            Some(rect(43., 62.5, 68., 38.5))
        );
        assert_eq!(node_bounds(&root, outer, 1, 3.3, 2.), Some(outer));
        assert_eq!(node_bounds(&root, outer, 99, 3.3, 2.), None);
    }
}
