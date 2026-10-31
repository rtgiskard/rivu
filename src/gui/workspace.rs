//! Window-local workspace transitions over session-owned layout persistence.
//!
//! Trees, docking measurements and panel scroll positions change together.
//! Feature drafts and accepted writes belong to their feature/session owners;
//! switching a workspace never resets them or takes ownership of save workers.

use super::{
    interaction::{Cancellation, WindowInteraction},
    layout::{Layout, LayoutSet, Node, Panel, persistence::LayoutPersistenceHandle},
    scrolling::ScrollViews,
};
use crate::config::{AlbumArtworkOptions, AudioVisualizerOptions};
use anyhow::Result;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum SaveStatus {
    #[default]
    Idle,
    Saving,
    Finished,
}

/// Membership and active-tab facts published only by workspace edits. Feature
/// owners retain their own resources; visibility merely gates these facts.
#[derive(Default)]
pub(super) struct WorkspacePanels {
    pub(super) revision: u64,
    panels: HashMap<u64, Panel>,
    active: HashSet<u64>,
    kinds: HashSet<String>,
    active_kinds: HashSet<String>,
    metadata: Vec<u64>,
}

impl WorkspacePanels {
    pub(super) fn reconcile(&mut self, layout: &Layout) {
        fn visit(node: &Node, demand: &mut WorkspacePanels) {
            match node {
                Node::Split { children, .. } => {
                    for child in children {
                        visit(child, demand);
                    }
                }
                Node::Tabs { panels, active, .. } => {
                    for panel in panels {
                        demand.panels.insert(panel.id, panel.clone());
                        demand.kinds.insert(panel.kind.clone());
                        if panel.id == *active {
                            demand.active.insert(panel.id);
                            demand.active_kinds.insert(panel.kind.clone());
                            if panel.kind == "metadata" {
                                demand.metadata.push(panel.id);
                            }
                        }
                    }
                }
            }
        }
        self.panels.clear();
        self.active.clear();
        self.metadata.clear();
        self.kinds.clear();
        self.active_kinds.clear();
        if let Some(root) = &layout.root {
            visit(root, self);
        }
        self.revision = self
            .revision
            .checked_add(1)
            .expect("workspace panel revision");
    }

    pub(super) fn panels(&self) -> impl Iterator<Item = &Panel> {
        self.panels.values()
    }
    pub(super) fn active_panels(&self) -> impl Iterator<Item = &Panel> {
        self.active.iter().filter_map(|id| self.panels.get(id))
    }
    pub(super) fn contains_panel(&self, id: u64) -> bool {
        self.panels.contains_key(&id)
    }
    pub(super) fn is_panel_active(&self, id: u64) -> bool {
        self.active.contains(&id)
    }
    pub(super) fn contains_kind(&self, kind: &str) -> bool {
        self.kinds.contains(kind)
    }
    pub(super) fn panel(&self, id: u64) -> Option<&Panel> {
        self.panels.get(&id)
    }
    pub(super) fn metadata_consumers(&self) -> &[u64] {
        &self.metadata
    }
    fn demanded(&self, kind: &str) -> bool {
        self.active_kinds.contains(kind)
    }
}

pub(super) struct Workspace {
    pub(super) layouts: LayoutSet,
    pub(super) panels: WorkspacePanels,
    pub(super) interaction: WindowInteraction,
    pub(super) scroll_views: Rc<RefCell<ScrollViews>>,
    save: LayoutPersistenceHandle,
    save_seen: u64,
    pub(super) save_status: SaveStatus,
    pub(super) save_error: Option<String>,
}

impl Workspace {
    pub(super) fn new(
        save: LayoutPersistenceHandle,
        visualizer: AudioVisualizerOptions,
        artwork: AlbumArtworkOptions,
    ) -> (Self, Option<String>) {
        let (mut layouts, load_error) = save.snapshot();
        layouts.reset_edit_mode();
        let mut workspace = Self {
            layouts,
            panels: WorkspacePanels::default(),
            interaction: WindowInteraction::default(),
            scroll_views: Rc::new(RefCell::new(ScrollViews::default())),
            save,
            save_seen: 0,
            save_status: SaveStatus::Idle,
            save_error: None,
        };
        if workspace.initialize_panels(visualizer, artwork) {
            workspace.submit_save();
        }
        workspace.panels.reconcile(workspace.layouts.current());
        workspace
            .interaction
            .retain_layout(workspace.layouts.current().root.as_ref());
        (workspace, load_error)
    }

    pub(super) fn panel_demand(&self, kind: &str, visible: bool) -> bool {
        visible && self.panels.demanded(kind)
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
        self.publish_structure(visualizer, artwork);
    }

    pub(super) fn publish_structure(
        &mut self,
        visualizer: AudioVisualizerOptions,
        artwork: AlbumArtworkOptions,
    ) -> Cancellation {
        self.initialize_panels(visualizer, artwork);
        let cancellation = self.reconcile_structure();
        self.submit_save();
        cancellation
    }

    /// Structural edits invalidate geometry but retain surviving panels' scroll
    /// positions, including tabs that are not currently painted.
    fn reconcile_structure(&mut self) -> Cancellation {
        self.panels.reconcile(self.layouts.current());
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
        self.panels.reconcile(self.layouts.current());
        self.interaction
            .retain_layout(self.layouts.current().root.as_ref());
        self.submit_save();
    }

    pub(super) fn publish_panel_options(&mut self) {
        self.panels.reconcile(self.layouts.current());
        self.submit_save();
    }

    /// Geometry-only commits do not change membership, styles or active tabs.
    pub(super) fn persist_geometry(&self) {
        self.submit_save();
    }

    pub(super) fn submit_save(&self) -> u64 {
        self.save.submit(&self.layouts)
    }

    /// Manual sync retains the existing single-writer, replaceable-pending-save
    /// policy. Ordinary edits only publish session memory, never start I/O.
    pub(super) fn request_save(&mut self) -> u64 {
        self.submit_save();
        let request = self.save.sync();
        self.save_status = SaveStatus::Saving;
        request
    }

    pub(super) fn take_save_completion(&mut self) -> Option<Result<(), String>> {
        let completion = self.save.completion()?;
        if completion.revision <= self.save_seen {
            return None;
        }
        self.save_seen = completion.revision;
        self.save_status = SaveStatus::Finished;
        Some(completion.result)
    }
}

#[cfg(test)]
mod tests {
    use super::super::layout::{
        Axis, Layout, Panel,
        layout_geometry::SplitDrag,
        persistence::{LayoutPersistence, LoadedLayouts},
    };
    use super::*;
    use crate::gui::interaction::{Measured, WindowGeometry};
    use gpui::{Bounds, point, px, size};

    fn defaults() -> (AudioVisualizerOptions, AlbumArtworkOptions) {
        let config = crate::config::Config::default();
        (
            AudioVisualizerOptions::from(&config),
            AlbumArtworkOptions::from(&config),
        )
    }

    fn fixture() -> (tempfile::TempDir, LayoutPersistence, Workspace) {
        let directory = tempfile::tempdir().unwrap();
        let save =
            LayoutPersistence::new(LoadedLayouts::load(directory.path().join("layout.json")));
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
        let geometry = WindowGeometry {
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
        let geometry = WindowGeometry {
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
                    super::super::interaction::DragCommit::PersistSplit
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
    fn demand_tracks_preferences_inactive_removal_layout_switch_and_reopen() {
        let (_directory, save, mut workspace) = fixture();
        let (visualizer, artwork) = defaults();
        *workspace.layouts.current_mut() = Layout {
            root: None,
            next_id: 1,
            locked: false,
        };
        let metadata = workspace.layouts.current_mut().add("metadata", None);
        let art = workspace.layouts.current_mut().add("album_artwork", None);
        workspace.publish_structure(visualizer, artwork);
        assert_eq!(
            workspace.panels.panel(art).unwrap().album_artwork,
            Some(artwork)
        );
        assert!(workspace.panels.metadata_consumers().is_empty());
        assert!(workspace.panel_demand("album_artwork", true));

        workspace.layouts.current_mut().activate(metadata);
        workspace.persist(visualizer, artwork);
        assert_eq!(workspace.panels.metadata_consumers(), &[metadata]);
        assert!(!workspace.panel_demand("album_artwork", true));
        assert!(workspace.panels.contains_panel(art));
        // Resize/lock changes and rollback do not change the published demand.
        workspace.layouts.current_mut().locked = true;
        workspace.persist_geometry();
        assert_eq!(workspace.panels.metadata_consumers(), &[metadata]);
        assert!(!workspace.panel_demand("metadata", false));
        assert!(workspace.panel_demand("metadata", true));

        workspace.duplicate("Work", visualizer, artwork).unwrap();
        assert!(workspace.panel_demand("metadata", true));
        assert!(workspace.layouts.current_mut().remove(art));
        workspace.publish_structure(visualizer, artwork);
        assert!(!workspace.panels.contains_panel(art));
        assert!(workspace.panel_demand("metadata", true));
        workspace.activate("Default", visualizer, artwork).unwrap();
        assert!(workspace.panels.contains_panel(art));
        workspace.activate("Work", visualizer, artwork).unwrap();
        assert!(!workspace.panels.contains_panel(art));
        let (reopened, error) = Workspace::new(save.handle(), visualizer, artwork);
        assert!(error.is_none());
        assert!(reopened.panel_demand("metadata", true));
        assert!(!reopened.panels.contains_panel(art));
        assert_eq!(reopened.panels.metadata_consumers(), &[metadata]);
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
