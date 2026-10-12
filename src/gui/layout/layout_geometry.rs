//! Pixel-space edit reconciliation and separator drag snapshots.
use super::{Axis, Edge, Node};
use gpui::{Bounds, Pixels, Point};
use std::collections::HashMap;

pub(in crate::gui) fn node_id(node: &Node) -> u64 {
    match node {
        Node::Split { id, .. } | Node::Tabs { id, .. } => *id,
    }
}

pub(in crate::gui) fn find(node: &Node, id: u64) -> Option<&Node> {
    if node_id(node) == id {
        return Some(node);
    }
    if let Node::Split { children, .. } = node {
        return children.iter().find_map(|child| find(child, id));
    }
    None
}

pub(in crate::gui) fn reference_centers(
    node: &Node,
    axis: Axis,
    excluded: (u64, usize),
    bounds: &impl Fn(u64) -> Option<Bounds<Pixels>>,
    centers: &mut Vec<f64>,
) {
    if let Node::Split {
        id,
        axis: split_axis,
        children,
        ..
    } = node
    {
        if *split_axis == axis {
            for (index, pair) in children.windows(2).enumerate() {
                if (*id, index) == excluded {
                    continue;
                }
                if let (Some(first), Some(second)) =
                    (bounds(node_id(&pair[0])), bounds(node_id(&pair[1])))
                {
                    let center = if axis == Axis::Horizontal {
                        (first.origin.x + first.size.width + second.origin.x) * 0.5
                    } else {
                        (first.origin.y + first.size.height + second.origin.y) * 0.5
                    };
                    centers.push(f64::from(f32::from(center)));
                }
            }
        }
        for child in children {
            reference_centers(child, axis, excluded, bounds, centers);
        }
    }
}

fn resize(node: &Node, size: (f64, f64), sizes: &mut HashMap<u64, (f64, f64)>, gutter: f64) {
    if !sizes.contains_key(&node_id(node)) {
        return;
    }
    sizes.insert(node_id(node), size);
    let Node::Split { axis, children, .. } = node else {
        return;
    };
    let count = children
        .iter()
        .filter(|child| sizes.contains_key(&node_id(child)))
        .count();
    let total: f64 = children
        .iter()
        .filter_map(|child| sizes.get(&node_id(child)))
        .map(|size| {
            if *axis == Axis::Horizontal {
                size.0
            } else {
                size.1
            }
        })
        .sum();
    let length = if *axis == Axis::Horizontal {
        size.0
    } else {
        size.1
    };
    let content = (length - gutter * count.saturating_sub(1) as f64).max(0.);
    for child in children {
        let Some(old) = sizes.get(&node_id(child)).copied() else {
            continue;
        };
        let old_length = if *axis == Axis::Horizontal {
            old.0
        } else {
            old.1
        };
        let new_length = if total > 0. {
            content * old_length / total
        } else {
            0.
        };
        let child_size = if *axis == Axis::Horizontal {
            (new_length, size.1)
        } else {
            (size.0, new_length)
        };
        resize(child, child_size, sizes, gutter);
    }
}

// Returns whether the entire node disappears. The removed slot, including its
// gutter, belongs to the next surviving sibling (or the previous last sibling).
fn remove_region(
    node: &Node,
    panel: u64,
    sizes: &mut HashMap<u64, (f64, f64)>,
    gutter: f64,
) -> bool {
    match node {
        Node::Tabs { id, panels, .. } => {
            if panels.len() == 1 && panels[0].id == panel {
                sizes.remove(id);
                true
            } else {
                false
            }
        }
        Node::Split {
            id, axis, children, ..
        } => {
            for (index, child) in children.iter().enumerate() {
                let old_size = sizes.get(&node_id(child)).copied();
                if !remove_region(child, panel, sizes, gutter) {
                    continue;
                }
                let neighbor = children
                    .get(index + 1)
                    .or_else(|| index.checked_sub(1).and_then(|i| children.get(i)));
                let Some(neighbor) = neighbor else {
                    sizes.remove(id);
                    return true;
                };
                if let (Some(removed), Some(mut neighbor_size)) =
                    (old_size, sizes.get(&node_id(neighbor)).copied())
                {
                    if *axis == Axis::Horizontal {
                        neighbor_size.0 += removed.0 + gutter;
                    } else {
                        neighbor_size.1 += removed.1 + gutter;
                    }
                    resize(neighbor, neighbor_size, sizes, gutter);
                }
                return false;
            }
            false
        }
    }
}

pub(in crate::gui) fn edit_sizes(
    root: &Node,
    mut sizes: HashMap<u64, (f64, f64)>,
    panel: u64,
    docking: Option<(u64, Edge, u64)>,
    gutter: f64,
) -> HashMap<u64, (f64, f64)> {
    remove_region(root, panel, &mut sizes, gutter);
    if let Some((target, edge, tabs_id)) = docking.filter(|(_, edge, _)| *edge != Edge::Center)
        && let (Some(node), Some(outer)) = (find(root, target), sizes.get(&target).copied())
    {
        let mut half = outer;
        if matches!(edge, Edge::Left | Edge::Right) {
            half.0 = (outer.0 - gutter).max(0.) * 0.5;
        } else {
            half.1 = (outer.1 - gutter).max(0.) * 0.5;
        }
        resize(node, half, &mut sizes, gutter);
        sizes.insert(tabs_id, half);
        sizes.insert(tabs_id + 1, outer);
    }
    sizes
}

#[derive(Clone, Copy)]
pub(in crate::gui) struct SplitDrag {
    pub id: u64,
    pub index: usize,
    pub axis: Axis,
    pub bounds: Bounds<Pixels>,
    pub content_length: f64,
    pub gutter: f64,
    pub before: f64,
    pub after: f64,
    pub pointer_offset: f64,
    pub pointer: Point<Pixels>,
}

impl SplitDrag {
    pub fn origin(self) -> f64 {
        f64::from(f32::from(if self.axis == Axis::Horizontal {
            self.bounds.origin.x
        } else {
            self.bounds.origin.y
        }))
    }

    pub fn pointer_coordinate(self, position: Point<Pixels>) -> f64 {
        f64::from(f32::from(if self.axis == Axis::Horizontal {
            position.x
        } else {
            position.y
        }))
    }

    pub fn center(self, fraction: f64) -> f64 {
        self.origin() + fraction * self.content_length + self.gutter * (self.index as f64 + 0.5)
    }

    pub fn fraction(
        self,
        references: &[f64],
        enabled: bool,
        ui_scale: f64,
        grid: u32,
    ) -> Option<f64> {
        if self.content_length <= 0. {
            return None;
        }
        let minimum = if self.axis == Axis::Horizontal {
            160.
        } else {
            100.
        };
        let minimum = minimum / self.content_length;
        let lower = self.before + minimum;
        let upper = self.after - minimum;
        // An undersized pair cannot meet both minima: keep its equal-content point.
        if lower > upper {
            return Some((self.before + self.after) * 0.5);
        }
        let center_offset = self.origin() + self.gutter * (self.index as f64 + 0.5);
        let pointer = self.pointer_coordinate(self.pointer) - self.pointer_offset;
        let fraction_at = |center: f64| (center - center_offset) / self.content_length;
        let feasible = |center: f64| {
            let fraction = fraction_at(center);
            fraction >= lower && fraction <= upper
        };
        if enabled {
            let threshold = 4. * ui_scale;
            let reference = references
                .iter()
                .copied()
                .filter(|center| feasible(*center) && (*center - pointer).abs() <= threshold)
                .min_by(|a, b| (a - pointer).abs().total_cmp(&(b - pointer).abs()));
            if let Some(reference) = reference {
                return Some(fraction_at(reference));
            }
            let midpoint = self.center((self.before + self.after) * 0.5);
            if feasible(midpoint) && (midpoint - pointer).abs() <= threshold {
                return Some(fraction_at(midpoint));
            }
            let spacing = f64::from(grid) * ui_scale;
            let grid_center =
                self.origin() + ((pointer - self.origin()) / spacing).round() * spacing;
            if feasible(grid_center) {
                return Some(fraction_at(grid_center));
            }
        }
        Some(fraction_at(pointer).clamp(lower, upper))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn drag(pointer: f32) -> SplitDrag {
        SplitDrag {
            id: 1,
            index: 1,
            axis: Axis::Horizontal,
            bounds: Bounds::new(point(px(10.), px(20.)), size(px(1000.), px(400.))),
            content_length: 992.,
            gutter: 4.,
            before: 0.25,
            after: 1.,
            pointer_offset: 1.,
            pointer: point(px(pointer), px(20.)),
        }
    }

    #[test]
    fn snap_priority_and_alt_raw_position() {
        let drag = drag(638.);
        let reference = 640.;
        assert!(
            (drag.center(drag.fraction(&[reference], true, 1., 8).unwrap()) - reference).abs()
                < 1e-9
        );
        assert!(
            (drag.center(drag.fraction(&[reference], false, 1., 8).unwrap()) - 637.).abs() < 1e-9
        );
        let midpoint = drag.center((drag.before + drag.after) * 0.5);
        let mut drag = drag;
        drag.pointer.x = px(midpoint as f32 + 2.);
        assert!((drag.center(drag.fraction(&[], true, 1., 8).unwrap()) - midpoint).abs() < 1e-9);
    }

    #[test]
    fn grid_origin_and_infeasible_reference() {
        let drag = drag(677.);
        assert!((drag.center(drag.fraction(&[], true, 1., 16).unwrap()) - 682.).abs() < 1e-9);
        let raw = drag.center(drag.fraction(&[100.], true, 1., 8).unwrap());
        assert!((raw - 674.).abs() < 1e-9);
    }

    #[test]
    fn infeasible_near_reference_is_skipped_and_ties_are_stable() {
        let constrained = drag(425.);
        let fraction = constrained.fraction(&[423.], true, 1., 1).unwrap();
        assert!((constrained.center(fraction) - 424.).abs() < 1e-9);
        let drag = drag(638.);
        let fraction = drag.fraction(&[639., 635.], true, 1., 8).unwrap();
        assert!((drag.center(fraction) - 639.).abs() < 1e-9);
    }

    fn tabs(id: u64, panels: &[u64]) -> Node {
        Node::Tabs {
            id,
            active: panels[0],
            panels: panels
                .iter()
                .map(|id| super::super::Panel {
                    id: *id,
                    kind: "library".into(),
                    show_tab_bar: true,
                    audio_visualizer: None,
                })
                .collect(),
        }
    }

    fn horizontal(children: Vec<Node>) -> Node {
        Node::Split {
            id: 1,
            axis: Axis::Horizontal,
            children,
            boundaries: Vec::new(),
        }
    }

    fn spans(layout: &super::super::Layout, width: f64) -> Vec<f64> {
        let Node::Split {
            boundaries,
            children,
            ..
        } = layout.root.as_ref().unwrap()
        else {
            panic!("expected split")
        };
        super::super::split_geometry(boundaries, children.len(), width, 4., 1.)
            .map(|(_, width)| width)
            .collect()
    }

    #[test]
    fn insertion_charges_only_target_for_new_gutter() {
        let root = horizontal(vec![tabs(2, &[3, 4]), tabs(5, &[6]), tabs(7, &[8])]);
        let measured = HashMap::from([
            (1, (1008., 400.)),
            (2, (600., 400.)),
            (5, (200., 400.)),
            (7, (200., 400.)),
        ]);
        let sizes = edit_sizes(&root, measured, 4, Some((5, Edge::Right, 100)), 4.);
        let mut layout = super::super::Layout {
            root: Some(root),
            next_id: 100,
            locked: false,
        };
        assert!(layout.move_panel(4, 5, Edge::Right, None));
        layout.preserve_region_sizes(&sizes, 4.);
        assert_eq!(spans(&layout, 1008.), vec![600., 98., 98., 200.]);
    }

    #[test]
    fn removal_gives_gutter_and_content_to_next_or_previous() {
        for (panel, expected) in [
            (6, vec![600., 404.]),
            (8, vec![600., 404.]),
            (3, vec![804., 200.]),
        ] {
            let root = horizontal(vec![tabs(2, &[3]), tabs(5, &[6]), tabs(7, &[8])]);
            let measured = HashMap::from([
                (1, (1008., 400.)),
                (2, (600., 400.)),
                (5, (200., 400.)),
                (7, (200., 400.)),
            ]);
            let sizes = edit_sizes(&root, measured, panel, None, 4.);
            let mut layout = super::super::Layout {
                root: Some(root),
                next_id: 100,
                locked: false,
            };
            assert!(layout.remove(panel));
            layout.preserve_region_sizes(&sizes, 4.);
            assert_eq!(spans(&layout, 1008.), expected);
        }
    }

    #[test]
    fn nested_target_preserves_descendant_outer_sizes_on_flatten() {
        let target = Node::Split {
            id: 5,
            axis: Axis::Vertical,
            boundaries: vec![],
            children: vec![tabs(6, &[7]), tabs(8, &[9])],
        };
        let root = horizontal(vec![tabs(2, &[3, 4]), target]);
        let measured = HashMap::from([
            (1, (1004., 404.)),
            (2, (600., 404.)),
            (5, (400., 404.)),
            (6, (400., 200.)),
            (8, (400., 200.)),
        ]);
        let sizes = edit_sizes(&root, measured, 4, Some((5, Edge::Right, 100)), 4.);
        assert_eq!(sizes[&6], (198., 200.));
        assert_eq!(sizes[&8], (198., 200.));
        let mut layout = super::super::Layout {
            root: Some(root),
            next_id: 100,
            locked: false,
        };
        assert!(layout.move_panel(4, 5, Edge::Right, None));
        layout.preserve_region_sizes(&sizes, 4.);
        assert_eq!(spans(&layout, 1004.), vec![600., 198., 198.]);
    }

    #[test]
    fn reference_targets_include_separate_rows() {
        let root = Node::Split {
            id: 1,
            axis: Axis::Vertical,
            boundaries: vec![],
            children: vec![
                Node::Split {
                    id: 2,
                    axis: Axis::Horizontal,
                    boundaries: vec![],
                    children: vec![tabs(3, &[4]), tabs(5, &[6])],
                },
                Node::Split {
                    id: 7,
                    axis: Axis::Horizontal,
                    boundaries: vec![],
                    children: vec![tabs(8, &[9]), tabs(10, &[11])],
                },
            ],
        };
        let bounds = HashMap::from([
            (
                3,
                Bounds::new(point(px(10.), px(10.)), size(px(400.), px(200.))),
            ),
            (
                5,
                Bounds::new(point(px(414.), px(10.)), size(px(500.), px(200.))),
            ),
            (
                8,
                Bounds::new(point(px(10.), px(214.)), size(px(600.), px(200.))),
            ),
            (
                10,
                Bounds::new(point(px(614.), px(214.)), size(px(300.), px(200.))),
            ),
        ]);
        let mut centers = Vec::new();
        reference_centers(
            &root,
            Axis::Horizontal,
            (7, 0),
            &|id| bounds.get(&id).copied(),
            &mut centers,
        );
        assert_eq!(centers, vec![412.]);
    }
}
