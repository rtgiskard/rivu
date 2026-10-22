//! Window-local layout and pointer-session ownership.
//!
//! A session begins with its queue identity / split origin and ends exactly once:
//! only `finish_drag` or `finish_waveform` returns a commit. Cancellation returns
//! a split rollback, never a seek or persistence request. Callers apply rollbacks
//! to the layout and cancel the Waveform entity's local pointer capture too.
//!
//! Measurements belong to one layout/geometry epoch. Canvas handles from older
//! epochs cannot repopulate the cache, and only keys in the current tree can be
//! recorded. Resize, physical DPI and UI-scale changes invalidate every rectangle
//! and pointer session; docking may resume only after fresh tree measurements.
//! Layout replacement and window recreation additionally discard docking and
//! target-group identity. No frame-time tree or measurement collection is copied.

use super::{Layout, Node, docking, layout_geometry};
use gpui::{Bounds, Pixels, Point, Size, Window};
use std::{
    cell::{Ref, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::gui) struct DockRequest {
    pub(in crate::gui) target: docking::Target,
    pub(in crate::gui) index: Option<usize>,
}

pub(in crate::gui) struct PreparedDock {
    pub(in crate::gui) layout: Layout,
    pub(in crate::gui) panel: u64,
}

pub(in crate::gui) struct DockHover {
    pub(in crate::gui) request: DockRequest,
    pub(in crate::gui) owner: Bounds<Pixels>,
    pub(in crate::gui) preview: Option<Bounds<Pixels>>,
    pub(in crate::gui) prepared: Option<PreparedDock>,
    pub(in crate::gui) unchanged: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub(in crate::gui) struct DockGeometry {
    pub(in crate::gui) viewport: Size<Pixels>,
    pub(in crate::gui) ui_scale: f32,
    pub(in crate::gui) scale_factor: f32,
}

impl DockGeometry {
    pub(in crate::gui) fn current(window: &Window, ui_scale: f32) -> Self {
        Self {
            viewport: window.viewport_size(),
            ui_scale,
            scale_factor: window.scale_factor(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::gui) enum DockSource {
    Existing(u64),
    Catalog(&'static str),
}

pub(in crate::gui) struct PanelDocking {
    pub(in crate::gui) source: DockSource,
    pub(in crate::gui) hover: Option<DockHover>,
    pub(in crate::gui) geometry: DockGeometry,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub(in crate::gui) enum Measured {
    Workspace,
    Node(u64),
    Tab(u64),
    Seek(u64),
    Volume(u64),
}

#[derive(Clone, Copy)]
pub(in crate::gui) enum Dragging {
    Split(layout_geometry::SplitDrag),
    Seek(u64),
    Volume(u64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::gui) struct SeekCommit {
    pub(in crate::gui) queue_id: u64,
    pub(in crate::gui) seconds: f64,
}

#[derive(Debug, PartialEq)]
pub(in crate::gui) enum DragCommit {
    None,
    Seek(SeekCommit),
    PersistSplit,
}

#[derive(Default)]
pub(in crate::gui) struct Cancellation {
    pub(in crate::gui) split: Option<(u64, usize, f64)>,
    pub(in crate::gui) docking: bool,
}

impl Cancellation {
    /// Restore live split previews without ever persisting a cancelled edit.
    pub(in crate::gui) fn restore_split(&self, layout: &mut Layout) {
        if let Some((id, index, fraction)) = self.split {
            layout.set_boundary(id, index, fraction);
        }
    }
}

#[derive(Clone, Copy)]
enum SeekOrigin {
    Slider { duration: f64 },
    Waveform { generation: u64 },
}

struct SeekSession {
    origin: SeekOrigin,
    queue_id: u64,
    preview: Option<f64>,
}

#[derive(Default)]
struct Measurements {
    epoch: u64,
    geometry: Option<DockGeometry>,
    tree_measured: bool,
    allowed: HashSet<Measured>,
    bounds: HashMap<Measured, Bounds<Pixels>>,
}

impl Measurements {
    fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.tree_measured = false;
        self.bounds.clear();
    }
}

/// A paint callback capability, not a second owner of layout interaction state.
#[derive(Clone)]
pub(in crate::gui) struct MeasurementHandle {
    measurements: Rc<RefCell<Measurements>>,
    epoch: u64,
    key: Measured,
}

impl MeasurementHandle {
    pub(in crate::gui) fn record(&self, bounds: Bounds<Pixels>, geometry: DockGeometry) {
        let mut measurements = self.measurements.borrow_mut();
        if measurements.epoch != self.epoch
            || measurements.geometry != Some(geometry)
            || !measurements.allowed.contains(&self.key)
        {
            return;
        }
        if matches!(
            self.key,
            Measured::Workspace | Measured::Node(_) | Measured::Tab(_)
        ) {
            measurements.tree_measured = true;
        }
        measurements.bounds.insert(self.key, bounds);
    }
}

#[derive(Default)]
pub(in crate::gui) struct LayoutInteraction {
    target_group: Option<u64>,
    dragging: Option<Dragging>,
    docking: Option<PanelDocking>,
    split_references: Vec<f64>,
    split_origin: Option<f64>,
    seek: Option<SeekSession>,
    measurements: Rc<RefCell<Measurements>>,
}

impl LayoutInteraction {
    pub(in crate::gui) fn target_group(&self) -> Option<u64> {
        self.target_group
    }

    pub(in crate::gui) fn set_target_group(&mut self, target: Option<u64>) {
        self.target_group = target;
    }

    pub(in crate::gui) fn dragging(&self) -> Option<Dragging> {
        self.dragging
    }

    pub(in crate::gui) fn seek_preview(&self) -> Option<f64> {
        self.seek.as_ref().and_then(|seek| seek.preview)
    }

    pub(in crate::gui) fn docking(&self) -> Option<&PanelDocking> {
        self.docking.as_ref()
    }

    pub(in crate::gui) fn take_docking(&mut self) -> Option<PanelDocking> {
        self.docking.take()
    }

    pub(in crate::gui) fn restore_docking(&mut self, session: PanelDocking) {
        self.docking = Some(session);
    }

    pub(in crate::gui) fn clear_docking_preview(&mut self) {
        if let Some(session) = &mut self.docking {
            session.hover = None;
        }
    }

    pub(in crate::gui) fn cancel_docking(&mut self) -> bool {
        self.docking.take().is_some()
    }

    pub(in crate::gui) fn start_docking(
        &mut self,
        source: DockSource,
        geometry: DockGeometry,
    ) -> Cancellation {
        let cancellation = self.cancel();
        self.docking = Some(PanelDocking {
            source,
            hover: None,
            geometry,
        });
        cancellation
    }

    pub(in crate::gui) fn measurements(&self) -> Ref<'_, HashMap<Measured, Bounds<Pixels>>> {
        Ref::map(self.measurements.borrow(), |measurements| {
            &measurements.bounds
        })
    }

    pub(in crate::gui) fn measurement_handle(&self, key: Measured) -> MeasurementHandle {
        MeasurementHandle {
            measurements: Rc::clone(&self.measurements),
            epoch: self.measurements.borrow().epoch,
            key,
        }
    }

    pub(in crate::gui) fn measured_geometry(&self) -> Option<DockGeometry> {
        let measurements = self.measurements.borrow();
        measurements
            .tree_measured
            .then_some(measurements.geometry)
            .flatten()
    }

    pub(in crate::gui) fn scale_factor(&self) -> f64 {
        self.measurements
            .borrow()
            .geometry
            .map_or(1., |geometry| f64::from(geometry.scale_factor))
    }

    /// Rolled-back split bounds must not feed a subsequent structural edit.
    pub(in crate::gui) fn invalidate_measurements(&mut self) {
        self.measurements.borrow_mut().invalidate();
    }

    /// Invoke before processing pointer input as well as before rendering.
    /// Bounds notifications can precede repaint; old canvas handles are rejected.
    pub(in crate::gui) fn synchronize_geometry(
        &mut self,
        geometry: DockGeometry,
    ) -> Option<Cancellation> {
        if self.measurements.borrow().geometry == Some(geometry) {
            return None;
        }
        let cancellation = self.cancel_pointer();
        self.clear_docking_preview();
        let mut measurements = self.measurements.borrow_mut();
        measurements.invalidate();
        measurements.geometry = Some(geometry);
        Some(cancellation)
    }

    /// Refresh the finite key set only on layout edits, not every frame.
    /// Structural edits must use `layout_changed` before repainting instead.
    pub(in crate::gui) fn retain_layout(&mut self, root: Option<&Node>) {
        fn collect(node: &Node, keys: &mut HashSet<Measured>) {
            match node {
                Node::Split { id, children, .. } => {
                    keys.insert(Measured::Node(*id));
                    for child in children {
                        collect(child, keys);
                    }
                }
                Node::Tabs { id, panels, active } => {
                    keys.insert(Measured::Node(*id));
                    let show_tabs = panels
                        .iter()
                        .find(|panel| panel.id == *active)
                        .is_some_and(|panel| panel.show_tab_bar);
                    for panel in panels {
                        if show_tabs {
                            keys.insert(Measured::Tab(panel.id));
                        }
                        keys.insert(Measured::Seek(panel.id));
                        keys.insert(Measured::Volume(panel.id));
                    }
                }
            }
        }
        let mut measurements = self.measurements.borrow_mut();
        // Invalidate outstanding callbacks even if an ID is reused by a new tree.
        measurements.epoch = measurements.epoch.wrapping_add(1);
        measurements.allowed.clear();
        measurements.allowed.insert(Measured::Workspace);
        if let Some(root) = root {
            collect(root, &mut measurements.allowed);
        }
        let Measurements {
            allowed, bounds, ..
        } = &mut *measurements;
        bounds.retain(|key, _| allowed.contains(key));
        if self.target_group.is_some_and(|id| {
            !matches!(
                root.and_then(|root| layout_geometry::find(root, id)),
                Some(Node::Tabs { .. })
            )
        }) {
            self.target_group = None;
        }
        drop(measurements);
        self.clear_docking_preview();
    }

    /// After a tree edit/replacement, before its next measurement callbacks.
    /// The previous tree's split rollback must NOT be applied to the new tree.
    pub(in crate::gui) fn layout_changed(&mut self, root: Option<&Node>) -> Cancellation {
        let mut cancellation = self.invalidate_window();
        cancellation.split = None;
        self.target_group = None;
        self.retain_layout(root);
        cancellation
    }

    /// Use for window hide/recreation; focus loss may use `cancel` alone.
    pub(in crate::gui) fn invalidate_window(&mut self) -> Cancellation {
        let cancellation = self.cancel();
        self.target_group = None;
        self.measurements.borrow_mut().invalidate();
        cancellation
    }

    /// Layout locking cancels editing, not playback controls.
    pub(in crate::gui) fn lock_layout(&mut self) -> Cancellation {
        let mut cancellation = Cancellation {
            docking: self.cancel_docking(),
            ..Cancellation::default()
        };
        if matches!(self.dragging, Some(Dragging::Split(_))) {
            cancellation.split = self.take_split_origin();
            self.dragging = None;
            self.split_references.clear();
        }
        cancellation
    }

    pub(in crate::gui) fn begin_split(
        &mut self,
        drag: layout_geometry::SplitDrag,
        original_fraction: f64,
        root: &Node,
    ) -> Cancellation {
        let cancellation = self.cancel();
        layout_geometry::reference_centers(
            root,
            drag.axis,
            (drag.id, drag.index),
            &|id| {
                self.measurements
                    .borrow()
                    .bounds
                    .get(&Measured::Node(id))
                    .copied()
            },
            &mut self.split_references,
        );
        self.split_origin = Some(original_fraction);
        self.dragging = Some(Dragging::Split(drag));
        cancellation
    }

    pub(in crate::gui) fn update_split(
        &mut self,
        pointer: Point<Pixels>,
        snap: bool,
        ui_scale: f64,
        grid: u32,
    ) -> Option<(u64, usize, f64)> {
        let Some(Dragging::Split(mut drag)) = self.dragging else {
            return None;
        };
        drag.pointer = pointer;
        self.dragging = Some(Dragging::Split(drag));
        drag.fraction(&self.split_references, snap, ui_scale, grid)
            .map(|fraction| (drag.id, drag.index, fraction))
    }

    /// Caller applies the returned cancellation before updating the new session.
    pub(in crate::gui) fn begin_slider(
        &mut self,
        dragging: Dragging,
        queue_id: Option<u64>,
        duration: Option<f64>,
    ) -> Cancellation {
        let cancellation = self.cancel();
        match dragging {
            Dragging::Seek(_) => {
                if let (Some(queue_id), Some(duration)) = (queue_id, duration)
                    && duration.is_finite()
                    && duration > 0.
                {
                    self.seek = Some(SeekSession {
                        origin: SeekOrigin::Slider { duration },
                        queue_id,
                        preview: None,
                    });
                    self.dragging = Some(dragging);
                }
            }
            Dragging::Volume(_) => self.dragging = Some(dragging),
            Dragging::Split(_) => unreachable!("split sessions require an origin snapshot"),
        }
        cancellation
    }

    pub(in crate::gui) fn update_slider_seek(&mut self, fraction: f64) -> bool {
        let Some(seek) = &mut self.seek else {
            return false;
        };
        let SeekOrigin::Slider { duration } = seek.origin else {
            return false;
        };
        if !fraction.is_finite() {
            return false;
        }
        seek.preview = Some(duration * fraction.clamp(0., 1.));
        true
    }

    /// A capture's first Preview fixes queue identity. A new capture cannot
    /// inherit an orphaned session; delayed cancellation only affects its origin.
    pub(in crate::gui) fn waveform_preview(
        &mut self,
        seconds: f64,
        queue_id: Option<u64>,
        generation: u64,
    ) -> bool {
        if !seconds.is_finite() || seconds < 0. || self.dragging.is_some() || self.docking.is_some()
        {
            return false;
        }
        match self.seek.as_ref().map(|seek| seek.origin) {
            Some(SeekOrigin::Waveform {
                generation: current,
            }) if current == generation => {}
            None | Some(SeekOrigin::Waveform { .. }) => {
                let Some(queue_id) = queue_id else {
                    return false;
                };
                self.seek = Some(SeekSession {
                    origin: SeekOrigin::Waveform { generation },
                    queue_id,
                    preview: None,
                });
            }
            Some(SeekOrigin::Slider { .. }) => return false,
        }
        self.seek
            .as_mut()
            .expect("waveform capture has a seek session")
            .preview = Some(seconds);
        true
    }

    pub(in crate::gui) fn finish_waveform(
        &mut self,
        seconds: f64,
        generation: u64,
    ) -> Option<SeekCommit> {
        if !self
            .seek
            .as_ref()
            .is_some_and(|seek| matches!(seek.origin, SeekOrigin::Waveform { generation: current } if current == generation))
        {
            return None;
        }
        let seek = self.seek.take().unwrap();
        (seconds.is_finite() && seconds >= 0.).then_some(SeekCommit {
            queue_id: seek.queue_id,
            seconds,
        })
    }

    pub(in crate::gui) fn cancel_waveform(&mut self, generation: u64) -> bool {
        if self.seek.as_ref().is_some_and(|seek| {
            matches!(seek.origin, SeekOrigin::Waveform { generation: current } if current == generation)
        }) {
            self.seek = None;
            true
        } else {
            false
        }
    }

    pub(in crate::gui) fn finish_drag(&mut self) -> DragCommit {
        let dragging = self.dragging.take();
        self.split_origin = None;
        self.split_references.clear();
        match dragging {
            Some(Dragging::Seek(_)) => self
                .seek
                .take()
                .and_then(|seek| {
                    seek.preview.map(|seconds| SeekCommit {
                        queue_id: seek.queue_id,
                        seconds,
                    })
                })
                .map_or(DragCommit::None, DragCommit::Seek),
            Some(Dragging::Split(_)) => DragCommit::PersistSplit,
            _ => DragCommit::None,
        }
    }

    fn take_split_origin(&mut self) -> Option<(u64, usize, f64)> {
        match (self.dragging, self.split_origin.take()) {
            (Some(Dragging::Split(drag)), Some(fraction)) => Some((drag.id, drag.index, fraction)),
            _ => None,
        }
    }

    fn cancel_pointer(&mut self) -> Cancellation {
        let cancellation = Cancellation {
            split: self.take_split_origin(),
            docking: false,
        };
        self.dragging = None;
        self.seek = None;
        self.split_references.clear();
        cancellation
    }

    pub(in crate::gui) fn cancel(&mut self) -> Cancellation {
        let mut cancellation = self.cancel_pointer();
        cancellation.docking = self.cancel_docking();
        cancellation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn geometry(width: f32, ui_scale: f32, scale_factor: f32) -> DockGeometry {
        DockGeometry {
            viewport: size(px(width), px(400.)),
            ui_scale,
            scale_factor,
        }
    }

    fn tree(id: u64, panel_id: u64) -> Node {
        Node::Tabs {
            id,
            panels: vec![super::super::Panel {
                id: panel_id,
                kind: "transport".into(),
                show_tab_bar: true,
                audio_visualizer: None,
                album_artwork: None,
            }],
            active: panel_id,
        }
    }

    #[test]
    fn slider_keeps_initial_queue_and_duration_and_finishes_once() {
        let mut owner = LayoutInteraction::default();
        owner.begin_slider(Dragging::Seek(3), Some(10), Some(100.));
        assert!(owner.update_slider_seek(0.25));
        assert!(owner.update_slider_seek(0.75));
        assert_eq!(
            owner.finish_drag(),
            DragCommit::Seek(SeekCommit {
                queue_id: 10,
                seconds: 75.
            })
        );
        assert_eq!(owner.finish_drag(), DragCommit::None);
        assert_eq!(owner.seek_preview(), None);
    }

    #[test]
    fn cancelled_seek_never_commits_and_waveform_keeps_queue_identity() {
        let mut owner = LayoutInteraction::default();
        owner.begin_slider(Dragging::Seek(3), Some(10), Some(100.));
        owner.update_slider_seek(0.25);
        owner.cancel();
        assert_eq!(owner.finish_drag(), DragCommit::None);
        assert!(owner.waveform_preview(12., Some(10), 7));
        assert!(owner.waveform_preview(20., Some(11), 7));
        assert_eq!(
            owner.finish_waveform(22., 7),
            Some(SeekCommit {
                queue_id: 10,
                seconds: 22.
            })
        );
        assert_eq!(owner.finish_waveform(22., 7), None);
        owner.waveform_preview(10., Some(11), 8);
        owner.cancel();
        assert_eq!(owner.finish_waveform(30., 8), None);
    }

    #[test]
    fn root_mouse_up_does_not_clear_waveform_preview() {
        let mut owner = LayoutInteraction::default();
        owner.waveform_preview(10., Some(11), 7);
        assert_eq!(owner.finish_drag(), DragCommit::None);
        assert_eq!(owner.seek_preview(), Some(10.));
        assert_eq!(
            owner.finish_waveform(12., 7),
            Some(SeekCommit {
                queue_id: 11,
                seconds: 12.
            })
        );
    }

    #[test]
    fn delayed_surface_cancellation_cannot_clear_a_new_waveform_capture_or_slider() {
        let mut owner = LayoutInteraction::default();
        owner.waveform_preview(10., Some(11), 7);
        assert!(owner.cancel_waveform(7));
        assert_eq!(owner.seek_preview(), None);
        assert_eq!(owner.finish_waveform(12., 7), None);
        owner.waveform_preview(20., Some(12), 8);
        assert!(!owner.cancel_waveform(7));
        assert_eq!(owner.finish_waveform(12., 7), None);
        assert_eq!(
            owner.finish_waveform(21., 8),
            Some(SeekCommit {
                queue_id: 12,
                seconds: 21.
            })
        );
        owner.begin_slider(Dragging::Seek(3), Some(13), Some(100.));
        owner.update_slider_seek(0.5);
        assert!(!owner.cancel_waveform(8));
        assert_eq!(
            owner.finish_drag(),
            DragCommit::Seek(SeekCommit {
                queue_id: 13,
                seconds: 50.
            })
        );
    }

    #[test]
    fn old_canvases_cannot_repopulate_replaced_layout_or_geometry() {
        let mut owner = LayoutInteraction::default();
        let root = tree(1, 2);
        let initial = geometry(800., 1., 1.);
        let bounds = Bounds::new(point(px(0.), px(0.)), initial.viewport);
        owner.layout_changed(Some(&root));
        owner.synchronize_geometry(initial);
        let old = owner.measurement_handle(Measured::Node(1));
        old.record(bounds, initial);
        assert_eq!(owner.measurements().len(), 1);
        let scaled = geometry(800., 1., 2.);
        owner.synchronize_geometry(scaled);
        old.record(bounds, initial);
        old.record(bounds, scaled);
        assert!(owner.measurements().is_empty());
        assert!(owner.measured_geometry().is_none());
        let fresh = owner.measurement_handle(Measured::Node(1));
        fresh.record(bounds, scaled);
        assert!(owner.measured_geometry() == Some(scaled));
        owner.layout_changed(Some(&tree(1, 4)));
        fresh.record(bounds, scaled);
        assert!(owner.measurements().is_empty());
        owner
            .measurement_handle(Measured::Seek(2))
            .record(bounds, scaled);
        assert!(owner.measurements().is_empty());
    }

    #[test]
    fn geometry_changes_cancel_seek_and_keep_docking_without_old_targets() {
        let mut owner = LayoutInteraction::default();
        let initial = geometry(800., 1., 1.);
        owner.synchronize_geometry(initial);
        owner.begin_slider(Dragging::Seek(3), Some(10), Some(100.));
        owner.update_slider_seek(0.5);
        assert!(owner.synchronize_geometry(geometry(900., 1., 1.)).is_some());
        assert_eq!(owner.finish_drag(), DragCommit::None);
        owner.start_docking(DockSource::Existing(3), geometry(900., 1., 1.));
        owner.synchronize_geometry(geometry(900., 1.5, 1.));
        assert!(owner.docking().unwrap().source == DockSource::Existing(3));
        assert!(owner.docking().unwrap().hover.is_none());
        assert!(owner.measured_geometry().is_none());
    }

    #[test]
    fn locking_does_not_cancel_playback_controls() {
        let mut owner = LayoutInteraction::default();
        owner.begin_slider(Dragging::Seek(3), Some(10), Some(100.));
        owner.update_slider_seek(0.5);
        owner.lock_layout();
        assert_eq!(
            owner.finish_drag(),
            DragCommit::Seek(SeekCommit {
                queue_id: 10,
                seconds: 50.
            })
        );
    }

    #[test]
    fn measured_keys_and_target_are_bounded_by_current_tree() {
        let mut owner = LayoutInteraction::default();
        let first = tree(1, 2);
        let geometry = geometry(800., 1., 1.);
        owner.layout_changed(Some(&first));
        owner.synchronize_geometry(geometry);
        owner.set_target_group(Some(1));
        let bounds = Bounds::new(point(px(0.), px(0.)), geometry.viewport);
        owner
            .measurement_handle(Measured::Seek(2))
            .record(bounds, geometry);
        owner
            .measurement_handle(Measured::Node(99))
            .record(bounds, geometry);
        assert_eq!(owner.measurements().len(), 1);
        owner.retain_layout(Some(&tree(3, 4)));
        assert!(owner.measurements().is_empty());
        assert_eq!(owner.target_group(), None);
    }
    fn split_layout() -> Layout {
        Layout {
            root: Some(Node::Split {
                id: 1,
                axis: super::super::Axis::Horizontal,
                children: vec![tree(2, 3), tree(4, 5)],
                boundaries: vec![0.5],
            }),
            next_id: 6,
            locked: false,
        }
    }

    fn split_drag() -> layout_geometry::SplitDrag {
        layout_geometry::SplitDrag {
            id: 1,
            index: 0,
            axis: super::super::Axis::Horizontal,
            bounds: Bounds::new(point(px(0.), px(0.)), size(px(804.), px(400.))),
            content_length: 800.,
            gutter: 4.,
            before: 0.,
            after: 1.,
            pointer_offset: 0.,
            pointer: point(px(402.), px(10.)),
        }
    }

    #[test]
    fn cancelling_split_restores_live_preview_without_a_commit() {
        let mut owner = LayoutInteraction::default();
        let mut layout = split_layout();
        owner.begin_split(split_drag(), 0.5, layout.root.as_ref().unwrap());
        let (id, index, fraction) = owner
            .update_split(point(px(602.), px(10.)), false, 1., 8)
            .unwrap();
        assert!(layout.set_boundary(id, index, fraction));
        let cancellation = owner.cancel();
        assert_eq!(cancellation.split, Some((1, 0, 0.5)));
        cancellation.restore_split(&mut layout);
        assert_eq!(layout, split_layout());
        assert_eq!(owner.finish_drag(), DragCommit::None);
    }

    #[test]
    fn geometry_cancel_restores_split_but_tree_replacement_never_applies_old_rollback() {
        let mut owner = LayoutInteraction::default();
        let mut layout = split_layout();
        owner.synchronize_geometry(geometry(804., 1., 1.));
        owner.begin_split(split_drag(), 0.5, layout.root.as_ref().unwrap());
        layout.set_boundary(1, 0, 0.75);
        let cancellation = owner.synchronize_geometry(geometry(904., 1., 1.)).unwrap();
        cancellation.restore_split(&mut layout);
        assert_eq!(layout, split_layout());
        assert_eq!(owner.finish_drag(), DragCommit::None);
        owner.begin_split(split_drag(), 0.5, layout.root.as_ref().unwrap());
        let cancellation = owner.layout_changed(Some(&tree(1, 2)));
        assert_eq!(cancellation.split, None);
        assert!(owner.dragging().is_none());
        assert_eq!(owner.finish_drag(), DragCommit::None);
    }

    #[test]
    fn lock_and_window_invalidation_discard_docking_and_stale_target_identity() {
        let mut owner = LayoutInteraction::default();
        owner.start_docking(DockSource::Existing(3), geometry(800., 1., 1.));
        assert!(owner.lock_layout().docking);
        assert!(owner.docking().is_none());
        owner.set_target_group(Some(1));
        owner.waveform_preview(10., Some(5), 7);
        owner.invalidate_window();
        assert_eq!(owner.target_group(), None);
        assert_eq!(owner.finish_waveform(20., 7), None);
    }

    #[test]
    fn empty_workspace_measurements_enable_docking_without_accepting_stale_callbacks() {
        let mut owner = LayoutInteraction::default();
        let initial = geometry(800., 1., 1.);
        let bounds = Bounds::new(point(px(0.), px(40.)), size(px(800.), px(360.)));
        owner.layout_changed(None);
        owner.synchronize_geometry(initial);
        let stale = owner.measurement_handle(Measured::Workspace);
        stale.record(bounds, initial);
        assert!(owner.measured_geometry() == Some(initial));
        assert_eq!(
            owner.measurements().get(&Measured::Workspace),
            Some(&bounds)
        );
        owner.start_docking(DockSource::Catalog("transport"), initial);
        let resized = geometry(900., 1., 1.);
        owner.synchronize_geometry(resized);
        stale.record(bounds, resized);
        assert!(owner.measured_geometry().is_none());
        assert!(owner.measurements().is_empty());
        assert!(owner.docking().unwrap().source == DockSource::Catalog("transport"));
        let fresh = owner.measurement_handle(Measured::Workspace);
        fresh.record(bounds, resized);
        assert!(owner.measured_geometry() == Some(resized));
        owner.layout_changed(Some(&tree(1, 2)));
        fresh.record(bounds, resized);
        assert!(owner.measurements().is_empty());
        assert!(owner.measured_geometry().is_none());
    }
}
