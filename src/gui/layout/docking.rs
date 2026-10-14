//! Measured tab insertion and docking preview geometry.
use super::layout_geometry::node_id;
use super::{Axis, Edge, Node, split_geometry};
use gpui::{Bounds, Pixels, Point, px};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) struct Target {
    pub node: u64,
    pub edge: Edge,
}

/// Hidden bars have no tab targets, even when a previous frame measured them.
/// The measured row spans the owner's width, including gaps and trailing space.
pub(super) fn tab_index(
    node: &Node,
    pointer: Point<Pixels>,
    owner: Bounds<Pixels>,
    bounds: &impl Fn(u64) -> Option<Bounds<Pixels>>,
) -> Option<usize> {
    let Node::Tabs { panels, active, .. } = node else {
        return None;
    };
    if !panels
        .iter()
        .find(|panel| panel.id == *active)?
        .show_tab_bar
        || !valid(owner)
        || !inside(owner, pointer)
    {
        return None;
    }
    let mut top = f32::INFINITY;
    let mut bottom = f32::NEG_INFINITY;
    for panel in panels {
        if let Some(rect) = bounds(panel.id).filter(|rect| valid(*rect)) {
            // Only rectangles intersecting the owner describe its visible row.
            if rect.origin.x < owner.origin.x + owner.size.width
                && rect.origin.x + rect.size.width > owner.origin.x
                && rect.origin.y < owner.origin.y + owner.size.height
                && rect.origin.y + rect.size.height > owner.origin.y
            {
                top = top.min(f32::from(rect.origin.y));
                bottom = bottom.max(f32::from(rect.origin.y + rect.size.height));
            }
        }
    }
    let y = f32::from(pointer.y);
    if y < top || y >= bottom {
        return None;
    }
    for (index, panel) in panels.iter().enumerate() {
        if let Some(rect) = bounds(panel.id).filter(|rect| valid(*rect))
            && pointer.x < rect.origin.x + rect.size.width * 0.5
        {
            return Some(index);
        }
    }
    Some(panels.len())
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
    use gpui::{point, size};

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
        let owner = rect(0., 0., 240., 300.);
        assert_eq!(tab_index(&node, position, owner, &bounds), Some(1));
        let Node::Tabs { panels, .. } = &mut node else {
            unreachable!()
        };
        panels[0].show_tab_bar = false;
        assert_eq!(tab_index(&node, position, owner, &bounds), None);
        let Node::Tabs { active, .. } = &mut node else {
            unreachable!()
        };
        *active = 102;
        assert_eq!(tab_index(&node, position, owner, &bounds), Some(1));
        assert_eq!(
            tab_index(&node, pointer(210., 30.), owner, &bounds),
            Some(2)
        );
    }

    #[test]
    fn tab_insertion_covers_row_gaps_tail_and_midpoint_boundaries() {
        let mut node = tabs(1);
        let Node::Tabs { panels, .. } = &mut node else {
            unreachable!()
        };
        let mut second = panels[0].clone();
        second.id = 102;
        panels.push(second);
        let owner = rect(0., 0., 240., 300.);
        let bounds = |id| match id {
            101 => Some(rect(10., 5., 90., 30.)),
            102 => Some(rect(104., 5., 90., 30.)),
            _ => None,
        };
        for (x, index) in [
            (0., 0),
            (54., 0),
            (55., 1),
            (102., 1),
            (148., 1),
            (149., 2),
            (239., 2),
        ] {
            assert_eq!(
                tab_index(&node, pointer(x, 5.), owner, &bounds),
                Some(index)
            );
        }
        for position in [
            pointer(-1., 10.),
            pointer(240., 10.),
            pointer(20., 4.),
            pointer(20., 35.),
        ] {
            assert_eq!(tab_index(&node, position, owner, &bounds), None);
        }
    }

    #[test]
    fn tab_insertion_requires_visible_row_measurements() {
        let node = tabs(1);
        let owner = rect(0., 0., 240., 300.);
        let position = pointer(20., 10.);
        assert_eq!(tab_index(&node, position, owner, &|_| None), None);
        assert_eq!(
            tab_index(&node, position, owner, &|_| Some(rect(240., 5., 90., 30.))),
            None
        );
        assert_eq!(
            tab_index(&node, position, owner, &|_| Some(rect(10., 300., 90., 30.))),
            None
        );
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
