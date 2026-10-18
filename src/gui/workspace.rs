//! Window-local workspace transitions over session-owned layout persistence.
//!
//! Trees, docking measurements and panel scroll positions change together.
//! Feature drafts and accepted writes belong to their feature/session owners;
//! switching a workspace never resets them or takes ownership of save workers.

use super::{
    layout::{
        LayoutSet, Node,
        interaction::{Cancellation, LayoutInteraction},
        save_owner::LayoutSaveHandle,
    },
    scrolling::ScrollViews,
};
use crate::config::{AlbumArtworkOptions, AudioVisualizerOptions};
use anyhow::Result;
use std::{cell::RefCell, rc::Rc};

pub(super) struct Workspace {
    pub(super) layouts: LayoutSet,
    pub(super) interaction: LayoutInteraction,
    pub(super) scroll_views: Rc<RefCell<ScrollViews>>,
    save: LayoutSaveHandle,
    save_seen: u64,
    pub(super) save_error: Option<String>,
}

impl Workspace {
    pub(super) fn new(
        save: LayoutSaveHandle,
        visualizer: AudioVisualizerOptions,
        artwork: AlbumArtworkOptions,
    ) -> (Self, Option<String>) {
        let (mut layouts, load_error) = save.snapshot();
        layouts.reset_edit_mode();
        let mut workspace = Self {
            layouts,
            interaction: LayoutInteraction::default(),
            scroll_views: Rc::new(RefCell::new(ScrollViews::default())),
            save,
            save_seen: 0,
            save_error: None,
        };
        if workspace.initialize_panels(visualizer, artwork) {
            workspace.submit_save();
        }
        workspace
            .interaction
            .retain_layout(workspace.layouts.current().root.as_ref());
        (workspace, load_error)
    }

    /// Demand is a read-only traversal of active tabs, without allocating a
    /// temporary panel list on every feature refresh.
    pub(super) fn panel_demand(&self, kind: &str, visible: bool) -> bool {
        fn demanded(node: &Node, kind: &str) -> bool {
            match node {
                Node::Split { children, .. } => children.iter().any(|node| demanded(node, kind)),
                Node::Tabs { panels, active, .. } => panels
                    .iter()
                    .any(|panel| panel.id == *active && panel.kind == kind),
            }
        }
        visible
            && self
                .layouts
                .current()
                .root
                .as_ref()
                .is_some_and(|root| demanded(root, kind))
    }

    /// Failed/no-op activation leaves live drag previews and scroll state intact.
    /// Rollback belongs to the old tree, before any incoming panel IDs are used.
    pub(super) fn activate(
        &mut self,
        name: &str,
        visualizer: AudioVisualizerOptions,
        artwork: AlbumArtworkOptions,
    ) -> Result<Option<Cancellation>> {
        let name = self.layouts.selectable_name(name)?;
        if name == self.layouts.active_name() {
            return Ok(None);
        }
        let cancellation = self.cancel_before_activation();
        self.layouts
            .select(name)
            .expect("validated layout selection");
        self.finish_activation(visualizer, artwork);
        Ok(Some(cancellation))
    }

    pub(super) fn duplicate(
        &mut self,
        name: &str,
        visualizer: AudioVisualizerOptions,
        artwork: AlbumArtworkOptions,
    ) -> Result<Cancellation> {
        let name = self.layouts.unused_name(name)?;
        let cancellation = self.cancel_before_activation();
        self.layouts.duplicate(name).expect("validated layout name");
        self.finish_activation(visualizer, artwork);
        Ok(cancellation)
    }

    fn cancel_before_activation(&mut self) -> Cancellation {
        let mut cancellation = self.interaction.cancel();
        self.restore_interaction(&cancellation);
        // Never replay old split IDs against the incoming tree.
        cancellation.split = None;
        cancellation
    }

    fn finish_activation(
        &mut self,
        visualizer: AudioVisualizerOptions,
        artwork: AlbumArtworkOptions,
    ) {
        *self.scroll_views.borrow_mut() = ScrollViews::default();
        self.initialize_panels(visualizer, artwork);
        self.reconcile_structure();
        self.submit_save();
    }

    /// Structural edits invalidate geometry but retain surviving panels' scroll
    /// positions, including tabs that are not currently painted.
    pub(super) fn reconcile_structure(&mut self) -> Cancellation {
        self.scroll_views
            .borrow_mut()
            .retain_layout(self.layouts.current());
        self.interaction
            .layout_changed(self.layouts.current().root.as_ref())
    }

    pub(super) fn restore_interaction(&mut self, cancellation: &Cancellation) {
        if cancellation.split.is_some() {
            cancellation.restore_split(self.layouts.current_mut());
            self.interaction.invalidate_measurements();
        }
    }

    fn initialize_panels(
        &mut self,
        visualizer: AudioVisualizerOptions,
        artwork: AlbumArtworkOptions,
    ) -> bool {
        self.layouts
            .current_mut()
            .initialize_audio_visualizers(visualizer)
            | self
                .layouts
                .current_mut()
                .initialize_album_artworks(artwork)
    }

    /// Nonstructural edits preserve measurements and scroll; the session writer
    /// receives an immutable snapshot and remains alive after this window closes.
    pub(super) fn persist(
        &mut self,
        visualizer: AudioVisualizerOptions,
        artwork: AlbumArtworkOptions,
    ) {
        self.initialize_panels(visualizer, artwork);
        self.interaction
            .retain_layout(self.layouts.current().root.as_ref());
        self.submit_save();
    }

    pub(super) fn submit_save(&self) -> u64 {
        self.save.submit(&self.layouts)
    }

    /// Manual sync retains the existing single-writer, replaceable-pending-save
    /// policy. Ordinary edits only publish session memory, never start I/O.
    pub(super) fn request_save(&self) -> u64 {
        self.submit_save();
        self.save.sync()
    }

    pub(super) fn take_save_completion(&mut self) -> Option<Result<(), String>> {
        let completion = self.save.completion()?;
        if completion.revision <= self.save_seen {
            return None;
        }
        self.save_seen = completion.revision;
        Some(completion.result)
    }
}

#[cfg(test)]
mod tests {
    use super::super::layout::{
        Axis, Layout, Panel,
        interaction::{DockGeometry, Measured},
        layout_geometry::SplitDrag,
        save_owner::LayoutSaveOwner,
    };
    use super::*;
    use gpui::{Bounds, point, px, size};

    fn defaults() -> (AudioVisualizerOptions, AlbumArtworkOptions) {
        let config = crate::config::Config::default();
        (
            AudioVisualizerOptions::from(&config),
            AlbumArtworkOptions::from(&config),
        )
    }

    fn fixture() -> (tempfile::TempDir, LayoutSaveOwner, Workspace) {
        let directory = tempfile::tempdir().unwrap();
        let save = LayoutSaveOwner::new(directory.path().join("layout.json"));
        let (visualizer, artwork) = defaults();
        let (workspace, error) = Workspace::new(save.handle(), visualizer, artwork);
        assert!(error.is_none());
        (directory, save, workspace)
    }

    fn begin_split(workspace: &mut Workspace) {
        let tabs = |id, kind: &str| Node::Tabs {
            id,
            panels: vec![Panel {
                id: id + 1,
                kind: kind.into(),
                show_tab_bar: true,
                audio_visualizer: None,
                album_artwork: None,
            }],
            active: id + 1,
        };
        *workspace.layouts.current_mut() = Layout {
            root: Some(Node::Split {
                id: 1,
                axis: Axis::Horizontal,
                children: vec![tabs(2, "library"), tabs(4, "queue")],
                boundaries: vec![0.5],
            }),
            next_id: 6,
            locked: false,
        };
        workspace.reconcile_structure();
        workspace.interaction.begin_split(
            SplitDrag {
                id: 1,
                index: 0,
                axis: Axis::Horizontal,
                bounds: Bounds::new(point(px(0.), px(0.)), size(px(804.), px(400.))),
                content_length: 800.,
                gutter: 4.,
                before: 0.,
                after: 1.,
                pointer_offset: 0.,
                pointer: point(px(402.), px(10.)),
            },
            0.5,
            workspace.layouts.current().root.as_ref().unwrap(),
        );
        workspace.layouts.current_mut().set_boundary(1, 0, 0.7);
    }

    #[test]
    fn invalid_activation_and_duplicate_preserve_live_interaction() {
        let (_directory, _save, mut workspace) = fixture();
        begin_split(&mut workspace);
        let (visualizer, artwork) = defaults();
        assert!(workspace.activate("missing", visualizer, artwork).is_err());
        assert!(workspace.duplicate("Default", visualizer, artwork).is_err());
        assert!(
            workspace
                .activate(" Default ", visualizer, artwork)
                .unwrap()
                .is_none()
        );
        assert!(workspace.interaction.dragging().is_some());
        let Node::Split { boundaries, .. } = workspace.layouts.current().root.as_ref().unwrap()
        else {
            panic!("split fixture");
        };
        assert_eq!(boundaries, &[0.7]);
    }

    #[test]
    fn activation_rolls_back_old_tree_and_retires_scroll_and_geometry() {
        let (_directory, save, mut workspace) = fixture();
        let scroll = workspace.scroll_views.borrow_mut().list(("library", 3));
        let retired = std::rc::Rc::downgrade(&scroll.0);
        drop(scroll);
        let geometry = DockGeometry {
            viewport: size(px(804.), px(400.)),
            scale_factor: 1.,
            ui_scale: 1.,
        };
        workspace.interaction.synchronize_geometry(geometry);
        begin_split(&mut workspace);
        let measured = workspace.interaction.measurement_handle(Measured::Node(1));
        measured.record(
            Bounds::new(point(px(0.), px(0.)), geometry.viewport),
            geometry,
        );
        let (visualizer, artwork) = defaults();
        workspace
            .activate("Album", visualizer, artwork)
            .unwrap()
            .unwrap();
        assert!(retired.upgrade().is_none());
        assert!(workspace.interaction.measurements().is_empty());
        measured.record(
            Bounds::new(point(px(0.), px(0.)), geometry.viewport),
            geometry,
        );
        assert!(workspace.interaction.measurements().is_empty());
        workspace
            .activate("Default", visualizer, artwork)
            .unwrap()
            .unwrap();
        let Node::Split { boundaries, .. } = workspace.layouts.current().root.as_ref().unwrap()
        else {
            panic!("split fixture");
        };
        assert_eq!(boundaries, &[0.5]);
        assert!(!workspace.layouts.current().locked);
        assert_eq!(save.handle().snapshot().0, workspace.layouts);
    }

    #[test]
    fn removal_cannot_republish_cancelled_split_measurements() {
        let (_directory, save, mut workspace) = fixture();
        let geometry = DockGeometry {
            viewport: size(px(804.), px(400.)),
            scale_factor: 1.,
            ui_scale: 1.,
        };
        workspace.interaction.synchronize_geometry(geometry);
        begin_split(&mut workspace);
        let panel = workspace.layouts.current_mut().add("history", Some(2));
        for (id, x, width) in [(1, 0., 804.), (2, 0., 560.), (4, 564., 240.)] {
            workspace
                .interaction
                .measurement_handle(Measured::Node(id))
                .record(
                    Bounds::new(point(px(x), px(0.)), size(px(width), px(400.))),
                    geometry,
                );
        }
        let stale = workspace.interaction.measurement_handle(Measured::Node(2));
        let cancellation = workspace.interaction.cancel();
        workspace.restore_interaction(&cancellation);
        stale.record(
            Bounds::new(point(px(0.), px(0.)), size(px(560.), px(400.))),
            geometry,
        );
        assert!(workspace.interaction.measurements().is_empty());
        let sizes = workspace
            .interaction
            .measurements()
            .iter()
            .filter_map(|(key, bounds)| {
                let Measured::Node(id) = key else { return None };
                Some((
                    *id,
                    (
                        f64::from(f32::from(bounds.size.width)),
                        f64::from(f32::from(bounds.size.height)),
                    ),
                ))
            })
            .collect();
        let sizes = super::super::layout::layout_geometry::edit_sizes(
            workspace.layouts.current().root.as_ref().unwrap(),
            sizes,
            panel,
            None,
            4.,
        );
        assert!(workspace.layouts.current_mut().remove(panel));
        workspace
            .layouts
            .current_mut()
            .preserve_region_sizes(&sizes, 4.);
        workspace.reconcile_structure();
        let (visualizer, artwork) = defaults();
        workspace.persist(visualizer, artwork);
        let retained = save.handle().snapshot().0;
        let Node::Split { boundaries, .. } = retained.current().root.as_ref().unwrap() else {
            panic!("split fixture");
        };
        assert_eq!(boundaries, &[0.5]);
        assert!(!retained.current().contains_panel(panel));
    }

    #[test]
    fn tab_publication_retains_rollback_but_preserves_completed_split() {
        for commit in [false, true] {
            let (_directory, save, mut workspace) = fixture();
            begin_split(&mut workspace);
            let panel = workspace.layouts.current_mut().add("history", Some(2));
            workspace.layouts.current_mut().activate(3);
            if commit {
                assert!(matches!(
                    workspace.interaction.finish_drag(),
                    super::super::layout::interaction::DragCommit::PersistSplit
                ));
            }

            // Native tab activation cancels and restores before changing the
            // active panel or publishing to the session owner.
            let cancellation = workspace.interaction.cancel();
            workspace.restore_interaction(&cancellation);
            workspace.layouts.current_mut().activate(panel);
            let (visualizer, artwork) = defaults();
            workspace.persist(visualizer, artwork);
            let retained = save.handle().snapshot().0;
            assert!(workspace.panel_demand("history", true));

            // A later Escape must not be needed to repair the retained snapshot.
            let cancellation = workspace.interaction.cancel();
            workspace.restore_interaction(&cancellation);
            assert_eq!(retained, workspace.layouts);
            let (reopened, error) = Workspace::new(save.handle(), visualizer, artwork);
            assert!(error.is_none());
            assert!(reopened.panel_demand("history", true));
            let Node::Split { boundaries, .. } = reopened.layouts.current().root.as_ref().unwrap()
            else {
                panic!("split fixture");
            };
            assert_eq!(boundaries, &[if commit { 0.7 } else { 0.5 }]);
        }
    }

    #[test]
    fn structural_edits_retain_hidden_scroll_and_only_active_tabs_demand_work() {
        let (_directory, _save, mut workspace) = fixture();
        let library = workspace
            .layouts
            .current()
            .panels()
            .iter()
            .find(|panel| panel.kind == "library")
            .unwrap()
            .id;
        let albums = workspace
            .layouts
            .current()
            .panels()
            .iter()
            .find(|panel| panel.kind == "albums")
            .unwrap()
            .id;
        let scroll = workspace
            .scroll_views
            .borrow_mut()
            .list(("library", library));
        scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-120.)));
        workspace.layouts.current_mut().activate(albums);
        workspace.reconcile_structure();
        assert!(!workspace.panel_demand("library", true));
        assert!(workspace.panel_demand("albums", true));
        assert!(!workspace.panel_demand("albums", false));
        let retained = workspace
            .scroll_views
            .borrow_mut()
            .list(("library", library));
        assert!(std::rc::Rc::ptr_eq(&scroll.0, &retained.0));
        assert_eq!(retained.0.borrow().base_handle.offset().y, px(-120.));
    }

    #[test]
    fn session_snapshot_and_requested_save_survive_workspace_close() {
        let (directory, save, mut workspace) = fixture();
        let (visualizer, artwork) = defaults();
        workspace.duplicate("Work", visualizer, artwork).unwrap();
        let expected = workspace.layouts.clone();
        workspace.request_save();
        drop(workspace);
        save.flush().unwrap();
        let (reopened, error) = Workspace::new(save.handle(), visualizer, artwork);
        assert!(error.is_none());
        assert_eq!(reopened.layouts, expected);
        let loaded = LayoutSet::load(&directory.path().join("layout.json")).unwrap();
        assert_eq!(loaded.active_name(), "Work");
        assert_eq!(
            loaded.current().panels().len(),
            expected.current().panels().len()
        );
    }
}
