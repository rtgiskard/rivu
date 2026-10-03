//! Serializable workspace docking, independent of the windowing toolkit.
//!
//! Node and panel IDs share one monotonically allocated namespace. Every public
//! transition preserves a normalized tree: tabs are nonempty, each split has
//! two children, and each active ID belongs to its tab group. The caller chooses
//! the persistence path (the desktop uses `workspace.json`).
//! Versionless workspaces gain a docked Playback panel above their unchanged
//! tree on load. Versioned workspaces preserve deliberate panel removal.

use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Panel {
    pub id: u64,
    pub kind: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Node {
    Split {
        id: u64,
        axis: Axis,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
    Tabs {
        id: u64,
        panels: Vec<Panel>,
        #[serde(default)]
        active: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub root: Option<Node>,
    pub next_id: u64,
    #[serde(default)]
    version: u32,
}

impl Node {
    pub fn id(&self) -> u64 {
        match self {
            Self::Split { id, .. } | Self::Tabs { id, .. } => *id,
        }
    }

    fn find(&self, target: u64) -> Option<&Self> {
        if self.id() == target {
            return Some(self);
        }
        match self {
            Self::Split { first, second, .. } => first.find(target).or_else(|| second.find(target)),
            Self::Tabs { .. } => None,
        }
    }

    fn find_mut(&mut self, target: u64) -> Option<&mut Self> {
        if self.id() == target {
            return Some(self);
        }
        match self {
            Self::Split { first, second, .. } => {
                first.find_mut(target).or_else(|| second.find_mut(target))
            }
            Self::Tabs { .. } => None,
        }
    }

    fn panel_group(&self, panel_id: u64) -> Option<&Self> {
        match self {
            Self::Tabs { panels, .. } => panels
                .iter()
                .any(|panel| panel.id == panel_id)
                .then_some(self),
            Self::Split { first, second, .. } => first
                .panel_group(panel_id)
                .or_else(|| second.panel_group(panel_id)),
        }
    }

    fn first_tabs(&self) -> u64 {
        match self {
            Self::Tabs { id, .. } => *id,
            Self::Split { first, .. } => first.first_tabs(),
        }
    }

    fn collect_panels<'a>(&'a self, output: &mut Vec<&'a Panel>, active_only: bool) {
        match self {
            Self::Tabs { panels, active, .. } => {
                output.extend(
                    panels
                        .iter()
                        .filter(|panel| !active_only || panel.id == *active),
                );
            }
            Self::Split { first, second, .. } => {
                first.collect_panels(output, active_only);
                second.collect_panels(output, active_only);
            }
        }
    }

    // Consuming the node lets removal collapse splits without cloning panels.
    // Surviving split boxes are reused rather than rebuilding the tree.
    fn extract(self, panel_id: u64, removed: &mut Option<Panel>) -> Option<Self> {
        if removed.is_some() {
            return Some(self);
        }
        match self {
            Self::Tabs {
                id,
                mut panels,
                mut active,
            } => {
                if let Some(index) = panels.iter().position(|panel| panel.id == panel_id) {
                    *removed = Some(panels.remove(index));
                    if panels.is_empty() {
                        return None;
                    }
                    if active == panel_id {
                        active = panels[index.min(panels.len() - 1)].id;
                    }
                }
                Some(Self::Tabs { id, panels, active })
            }
            Self::Split {
                id,
                axis,
                ratio,
                mut first,
                mut second,
            } => {
                let Some(left) = (*first).extract(panel_id, removed) else {
                    return Some(*second);
                };
                *first = left;
                let Some(right) = (*second).extract(panel_id, removed) else {
                    return Some(*first);
                };
                *second = right;
                Some(Self::Split {
                    id,
                    axis,
                    ratio,
                    first,
                    second,
                })
            }
        }
    }

    fn map_node<F: FnOnce(Self) -> Self>(self, target: u64, update: &mut Option<F>) -> Self {
        if update.is_none() {
            return self;
        }
        if self.id() == target {
            return update.take().expect("matched update")(self);
        }
        match self {
            Self::Split {
                id,
                axis,
                ratio,
                mut first,
                mut second,
            } => {
                *first = (*first).map_node(target, update);
                *second = (*second).map_node(target, update);
                Self::Split {
                    id,
                    axis,
                    ratio,
                    first,
                    second,
                }
            }
            tabs => tabs,
        }
    }

    fn dock(self, panel: Panel, edge: Edge, tabs_id: u64, split_id: u64) -> Self {
        let incoming = Self::Tabs {
            id: tabs_id,
            active: panel.id,
            panels: vec![panel],
        };
        let axis = match edge {
            Edge::Left | Edge::Right => Axis::Horizontal,
            Edge::Top | Edge::Bottom => Axis::Vertical,
            Edge::Center => unreachable!("center docking inserts into existing tabs"),
        };
        let (first, second) = match edge {
            Edge::Left | Edge::Top => (incoming, self),
            _ => (self, incoming),
        };
        Self::Split {
            id: split_id,
            axis,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        }
    }
}

impl Default for Layout {
    fn default() -> Self {
        let tabs = |id, panel_id, kind: &str| Node::Tabs {
            id,
            panels: vec![Panel {
                id: panel_id,
                kind: kind.to_owned(),
            }],
            active: panel_id,
        };
        let mut layout = Self {
            root: Some(Node::Split {
                id: 1,
                axis: Axis::Horizontal,
                ratio: 0.65,
                first: Box::new(tabs(2, 3, "library")),
                second: Box::new(Node::Split {
                    id: 4,
                    axis: Axis::Vertical,
                    ratio: 0.45,
                    first: Box::new(tabs(5, 6, "queue")),
                    second: Box::new(Node::Split {
                        id: 7,
                        axis: Axis::Vertical,
                        ratio: 0.5,
                        first: Box::new(tabs(8, 9, "spectrum")),
                        second: Box::new(tabs(10, 11, "spectrogram")),
                    }),
                }),
            }),
            next_id: 12,
            version: 1,
        };
        layout
            .add_transport()
            .expect("default workspace has spare IDs");
        layout
    }
}

impl Layout {
    fn add_transport(&mut self) -> Result<()> {
        let id = self.next_id;
        let count = if self.root.is_some() { 3 } else { 2 };
        let next_id = id
            .checked_add(count)
            .ok_or_else(|| anyhow!("workspace ID space exhausted"))?;
        let transport = Node::Tabs {
            id: id + 1,
            panels: vec![Panel {
                id,
                kind: "transport".into(),
            }],
            active: id,
        };
        self.root = Some(match self.root.take() {
            Some(root) => Node::Split {
                id: id + 2,
                axis: Axis::Vertical,
                ratio: 0.25,
                first: Box::new(transport),
                second: Box::new(root),
            },
            None => transport,
        });
        self.next_id = next_id;
        Ok(())
    }

    fn migrate(&mut self) -> Result<()> {
        if self.version == 0 {
            if !self.panels().iter().any(|panel| panel.kind == "transport") {
                self.add_transport()?;
            }
            self.version = 1;
        }
        Ok(())
    }

    fn allocate_ids(&mut self, count: u64) -> u64 {
        let first = self.next_id;
        self.next_id = first
            .checked_add(count)
            .expect("workspace ID space exhausted");
        first
    }

    /// Add a fresh instance and activate it. A split target selects its first
    /// tabs; a missing target selects the workspace's first tabs. Empty layouts
    /// receive a new root group. Kinds are opaque catalog keys, not an enum.
    pub fn add(&mut self, kind: &str, target: Option<u64>) -> u64 {
        let count = if self.root.is_some() { 1 } else { 2 };
        let id = self.allocate_ids(count);
        let panel = Panel {
            id,
            kind: kind.to_owned(),
        };
        if let Some(root) = self.root.as_mut() {
            let target = target
                .and_then(|id| root.find(id))
                .unwrap_or(root)
                .first_tabs();
            if let Some(Node::Tabs { panels, active, .. }) = root.find_mut(target) {
                panels.push(panel);
                *active = id;
            }
        } else {
            self.root = Some(Node::Tabs {
                id: id + 1,
                panels: vec![panel],
                active: id,
            });
        }
        id
    }

    pub fn remove(&mut self, panel_id: u64) -> bool {
        let Some(root) = self.root.take() else {
            return false;
        };
        let mut removed = None;
        self.root = root.extract(panel_id, &mut removed);
        removed.is_some()
    }

    /// Move an instance without changing its ID. Center insertion indices refer
    /// to the destination after removing the source and are clamped to its end.
    /// Edge docking accepts either a group or a split. A lone tab dropped onto
    /// its own group is a successful no-op, including drops at an edge.
    /// All source/target checks happen before the tree or allocator is changed.
    pub fn move_panel(
        &mut self,
        panel_id: u64,
        target_node: u64,
        edge: Edge,
        index: Option<usize>,
    ) -> bool {
        let Some(root) = self.root.as_ref() else {
            return false;
        };
        let Some(source) = root.panel_group(panel_id) else {
            return false;
        };
        let Some(target) = root.find(target_node) else {
            return false;
        };
        if edge == Edge::Center && !matches!(target, Node::Tabs { .. }) {
            return false;
        }
        let same_group = source.id() == target_node;
        if same_group && matches!(source, Node::Tabs { panels, .. } if panels.len() == 1) {
            return true;
        }
        if same_group && edge == Edge::Center {
            if let Some(Node::Tabs { panels, active, .. }) = self
                .root
                .as_mut()
                .and_then(|root| root.find_mut(target_node))
            {
                let old_index = panels
                    .iter()
                    .position(|panel| panel.id == panel_id)
                    .expect("source was checked");
                let panel = panels.remove(old_index);
                panels.insert(index.unwrap_or(panels.len()).min(panels.len()), panel);
                *active = panel_id;
            }
            return true;
        }
        let target_contains_source = target.panel_group(panel_id).is_some();
        if edge == Edge::Center {
            let mut removed = None;
            self.root = self
                .root
                .take()
                .expect("source was checked")
                .extract(panel_id, &mut removed);
            let Some(Node::Tabs { panels, active, .. }) = self
                .root
                .as_mut()
                .and_then(|root| root.find_mut(target_node))
            else {
                unreachable!("a distinct destination group survives extraction");
            };
            panels.insert(
                index.unwrap_or(panels.len()).min(panels.len()),
                removed.expect("source was checked"),
            );
            *active = panel_id;
        } else {
            // Allocate before taking the root, so even exhausted IDs cannot
            // leave the workspace with its source removed.
            let tabs_id = self.allocate_ids(2);
            let split_id = tabs_id + 1;
            let mut root = self.root.take().expect("source was checked");
            if target_contains_source {
                // Normalize inside the destination before wrapping it. Its old
                // ID may disappear when removing a child collapses that split.
                root = root.map_node(
                    target_node,
                    &mut Some(|target: Node| {
                        let mut removed = None;
                        let remaining = target
                            .extract(panel_id, &mut removed)
                            .expect("lone self-drops were handled above");
                        remaining.dock(
                            removed.expect("source was checked"),
                            edge,
                            tabs_id,
                            split_id,
                        )
                    }),
                );
            } else {
                let mut removed = None;
                root = root
                    .extract(panel_id, &mut removed)
                    .expect("a distinct destination survives extraction");
                root = root.map_node(
                    target_node,
                    &mut Some(|target: Node| {
                        target.dock(
                            removed.expect("source was checked"),
                            edge,
                            tabs_id,
                            split_id,
                        )
                    }),
                );
            }
            self.root = Some(root);
        }
        true
    }

    pub fn set_ratio(&mut self, split_id: u64, ratio: f32) -> bool {
        if !ratio.is_finite() || !(0.1..=0.9).contains(&ratio) {
            return false;
        }
        match self.root.as_mut().and_then(|root| root.find_mut(split_id)) {
            Some(Node::Split { ratio: current, .. }) => {
                *current = ratio;
                true
            }
            _ => false,
        }
    }

    pub fn activate(&mut self, panel_id: u64) -> bool {
        let Some(group_id) = self
            .root
            .as_ref()
            .and_then(|root| root.panel_group(panel_id))
            .map(Node::id)
        else {
            return false;
        };
        if let Some(Node::Tabs { active, .. }) =
            self.root.as_mut().and_then(|root| root.find_mut(group_id))
        {
            *active = panel_id;
        }
        true
    }

    pub fn active_panels(&self) -> Vec<&Panel> {
        let mut panels = Vec::new();
        if let Some(root) = &self.root {
            root.collect_panels(&mut panels, true);
        }
        panels
    }

    pub fn panels(&self) -> Vec<&Panel> {
        let mut panels = Vec::new();
        if let Some(root) = &self.root {
            root.collect_panels(&mut panels, false);
        }
        panels
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        fn record(
            id: u64,
            next_id: u64,
            ids: &mut HashSet<u64>,
        ) -> std::result::Result<(), String> {
            if id == 0 {
                return Err("workspace IDs must be nonzero".into());
            }
            if id >= next_id {
                return Err(format!("ID {id} is not below next_id {next_id}"));
            }
            if !ids.insert(id) {
                return Err(format!("duplicate workspace ID {id}"));
            }
            Ok(())
        }
        fn visit(
            node: &Node,
            next_id: u64,
            ids: &mut HashSet<u64>,
        ) -> std::result::Result<(), String> {
            record(node.id(), next_id, ids)?;
            match node {
                Node::Split {
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    if !ratio.is_finite() || !(0.1..=0.9).contains(ratio) {
                        return Err(format!("split {} has invalid ratio {ratio}", node.id()));
                    }
                    visit(first, next_id, ids)?;
                    visit(second, next_id, ids)?;
                }
                Node::Tabs { panels, active, .. } => {
                    if panels.is_empty() {
                        return Err(format!("tab group {} is empty", node.id()));
                    }
                    for panel in panels {
                        record(panel.id, next_id, ids)?;
                    }
                    if !panels.iter().any(|panel| panel.id == *active) {
                        return Err(format!(
                            "tab group {} has invalid active ID {active}",
                            node.id()
                        ));
                    }
                }
            }
            Ok(())
        }
        if self.next_id == 0 {
            return Err("next_id must be nonzero".into());
        }
        if let Some(root) = &self.root {
            visit(root, self.next_id, &mut HashSet::new())?;
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("opening workspace {}", path.display()))?;
        let mut layout: Self = serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("decoding workspace {}", path.display()))?;
        layout
            .validate()
            .map_err(|error| anyhow!(error))
            .with_context(|| format!("invalid workspace {}", path.display()))?;
        layout
            .migrate()
            .with_context(|| format!("migrating workspace {}", path.display()))?;
        Ok(layout)
    }

    /// Validate, write and fsync a unique sibling temporary file, then atomically
    /// replace the destination and fsync its directory. A failed write never
    /// replaces the previous workspace, and its temporary file is removed.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate().map_err(|error| anyhow!(error))?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("workspace path has no filename"))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("creating workspace directory {}", parent.display()))?;
        static TEMP_ID: AtomicU64 = AtomicU64::new(1);
        let (temporary, file) = loop {
            let mut temporary_name = name.to_os_string();
            temporary_name.push(format!(
                ".{}.{}.tmp",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed),
            ));
            let temporary = parent.join(temporary_name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => break (temporary, file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("creating temporary workspace"),
            }
        };
        let result = (|| -> Result<()> {
            let mut writer = BufWriter::new(file);
            serde_json::to_writer_pretty(&mut writer, self).context("encoding workspace")?;
            writer.write_all(b"\n")?;
            writer.flush().context("flushing workspace")?;
            writer.get_ref().sync_all().context("syncing workspace")?;
            drop(writer);
            fs::rename(&temporary, path)
                .with_context(|| format!("replacing workspace {}", path.display()))?;
            File::open(parent)?
                .sync_all()
                .context("syncing workspace directory")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(layout: &Layout) -> Vec<u64> {
        let mut ids: Vec<_> = layout.panels().iter().map(|panel| panel.id).collect();
        ids.sort_unstable();
        ids
    }

    fn group(layout: &Layout, panel: u64) -> u64 {
        layout
            .root
            .as_ref()
            .unwrap()
            .panel_group(panel)
            .unwrap()
            .id()
    }

    #[test]
    fn default_transport_can_move_and_stay_closed_after_reopening() {
        let mut layout = Layout::default();
        let transport = layout
            .active_panels()
            .into_iter()
            .find(|panel| panel.kind == "transport")
            .unwrap()
            .id;
        assert!(layout.move_panel(transport, group(&layout, 3), Edge::Bottom, None));
        layout.validate().unwrap();
        let encoded = serde_json::to_vec(&layout).unwrap();
        let mut reopened: Layout = serde_json::from_slice(&encoded).unwrap();
        reopened.migrate().unwrap();
        assert_eq!(reopened, layout);
        assert!(layout.remove(transport));
        let encoded = serde_json::to_vec(&layout).unwrap();
        let mut reopened: Layout = serde_json::from_slice(&encoded).unwrap();
        reopened.migrate().unwrap();
        assert_eq!(reopened, layout);
        assert!(
            !reopened
                .panels()
                .iter()
                .any(|panel| panel.kind == "transport")
        );
    }

    #[test]
    fn legacy_migration_preserves_tree_and_only_adds_transport_once() {
        let mut original = Layout::default();
        assert!(original.remove(12));
        original.add("history", Some(2));
        assert!(original.set_ratio(1, 0.73));
        let mut legacy = serde_json::to_value(&original).unwrap();
        legacy.as_object_mut().unwrap().remove("version");
        let mut migrated: Layout = serde_json::from_value(legacy).unwrap();
        migrated.migrate().unwrap();
        migrated.validate().unwrap();
        let Some(Node::Split {
            axis,
            first,
            second,
            ..
        }) = &migrated.root
        else {
            panic!("migration must wrap the existing workspace");
        };
        assert_eq!(*axis, Axis::Vertical);
        assert_eq!(Some(second.as_ref()), original.root.as_ref());
        assert!(matches!(first.as_ref(), Node::Tabs { panels, active, .. }
            if panels.len() == 1 && panels[0].kind == "transport" && panels[0].id == *active));
        assert_eq!(migrated.next_id, original.next_id + 3);
        let once = migrated.clone();
        migrated.migrate().unwrap();
        assert_eq!(migrated, once);
        migrated.version = 0;
        migrated.migrate().unwrap();
        assert_eq!(migrated, once);
    }

    #[test]
    fn empty_legacy_workspace_migrates_and_id_exhaustion_is_non_destructive() {
        let mut empty: Layout = serde_json::from_str(r#"{"root":null,"next_id":1}"#).unwrap();
        empty.migrate().unwrap();
        empty.validate().unwrap();
        assert_eq!(empty.panels().len(), 1);
        assert_eq!(empty.panels()[0].kind, "transport");
        let mut exhausted = Layout {
            root: None,
            next_id: u64::MAX,
            version: 0,
        };
        let original = exhausted.clone();
        assert!(exhausted.migrate().is_err());
        assert_eq!(exhausted, original);
    }

    #[test]
    fn remove_collapses_splits_and_can_repopulate_empty_workspace() {
        let mut layout = Layout::default();
        layout.validate().unwrap();
        let original_ids = ids(&layout);
        for (index, panel) in original_ids.iter().enumerate() {
            assert!(layout.remove(*panel));
            assert!(!layout.remove(*panel));
            layout.validate().unwrap();
            assert_eq!(ids(&layout), original_ids[index + 1..]);
        }
        assert!(layout.root.is_none());
        let id = layout.add("history", None);
        assert!(id > *original_ids.last().unwrap());
        layout.validate().unwrap();
        assert_eq!(ids(&layout), vec![id]);
        assert_eq!(layout.active_panels()[0].id, id);
    }

    #[test]
    fn reorder_and_move_preserve_instances_and_active_selection() {
        let mut layout = Layout::default();
        let target = group(&layout, 3);
        let a = layout.add("playlists", Some(target));
        let b = layout.add("metadata", Some(target));
        let original_ids = ids(&layout);
        assert!(layout.move_panel(b, target, Edge::Center, Some(0)));
        assert!(matches!(layout.root.as_ref().unwrap().find(target),
            Some(Node::Tabs { panels, active, .. })
            if panels.iter().map(|panel| panel.id).collect::<Vec<_>>() == vec![b, 3, a] && *active == b));
        assert!(layout.activate(3));
        assert!(layout.move_panel(3, group(&layout, 6), Edge::Center, Some(0)));
        assert!(layout.active_panels().iter().any(|panel| panel.id == a));
        assert!(layout.active_panels().iter().any(|panel| panel.id == 3));
        assert!(layout.remove(a));
        layout.validate().unwrap();
        assert_eq!(
            ids(&layout),
            original_ids
                .into_iter()
                .filter(|id| *id != a)
                .collect::<Vec<_>>()
        );
        assert!(layout.active_panels().iter().any(|panel| panel.id == b));
    }

    #[test]
    fn invalid_moves_and_lone_self_drops_are_unchanged() {
        let mut layout = Layout::default();
        let original = layout.clone();
        assert!(!layout.move_panel(3, 999, Edge::Left, None));
        assert!(!layout.move_panel(999, 2, Edge::Center, None));
        assert!(!layout.move_panel(3, 1, Edge::Center, None));
        for edge in [
            Edge::Center,
            Edge::Left,
            Edge::Right,
            Edge::Top,
            Edge::Bottom,
        ] {
            assert!(layout.move_panel(3, 2, edge, None));
            assert_eq!(layout, original);
        }
    }

    #[test]
    fn edge_moves_handle_siblings_own_groups_and_ancestor_collapse() {
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            let mut layout = Layout::default();
            let original_ids = ids(&layout);
            assert!(layout.move_panel(3, 5, edge, None));
            layout.validate().unwrap();
            assert_eq!(ids(&layout), original_ids);
            let source_group = group(&layout, 3);
            let added = layout.add("devices", Some(source_group));
            assert!(layout.move_panel(3, source_group, edge, None));
            layout.validate().unwrap();
            assert_ne!(group(&layout, 3), group(&layout, added));
            let root_id = layout.root.as_ref().unwrap().id();
            assert!(layout.move_panel(3, root_id, edge, None));
            layout.validate().unwrap();
            let mut expected = original_ids;
            expected.push(added);
            expected.sort_unstable();
            assert_eq!(ids(&layout), expected);
            let Node::Split {
                axis,
                first,
                second,
                ..
            } = layout.root.as_ref().unwrap()
            else {
                panic!("edge docking must create a split");
            };
            assert_eq!(
                *axis,
                if matches!(edge, Edge::Left | Edge::Right) {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                }
            );
            let incoming = if matches!(edge, Edge::Left | Edge::Top) {
                first
            } else {
                second
            };
            assert!(
                matches!(incoming.as_ref(), Node::Tabs { panels, active, .. }
                if panels.len() == 1 && *active == 3)
            );
        }
        let mut layout = Layout::default();
        assert!(layout.move_panel(3, 1, Edge::Left, None));
        layout.validate().unwrap();
        assert_eq!(ids(&layout), vec![3, 6, 9, 11, 12]);
        assert!(layout.root.as_ref().unwrap().find(1).is_none());
    }

    #[test]
    fn ratios_and_structural_validation_reject_invalid_state() {
        let mut layout = Layout::default();
        let original = layout.clone();
        for ratio in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.09, 0.91] {
            assert!(!layout.set_ratio(1, ratio));
            assert_eq!(layout, original);
        }
        assert!(!layout.set_ratio(2, 0.5));
        assert!(!layout.set_ratio(999, 0.5));
        assert!(layout.set_ratio(1, 0.1));
        assert!(layout.set_ratio(1, 0.9));
        layout.validate().unwrap();
        for (node_id, panel_id, active, next_id) in [
            (0, 2, 2, 3),
            (1, 0, 0, 3),
            (1, 1, 1, 3),
            (1, 2, 99, 3),
            (1, 2, 2, 2),
            (1, 2, 2, 0),
        ] {
            let invalid = Layout {
                root: Some(Node::Tabs {
                    id: node_id,
                    panels: vec![Panel {
                        id: panel_id,
                        kind: "library".into(),
                    }],
                    active,
                }),
                next_id,
                version: 1,
            };
            assert!(invalid.validate().is_err());
        }
        let mut invalid = Layout::default();
        if let Some(Node::Split { ratio, .. }) = invalid.root.as_mut() {
            *ratio = f32::NAN;
        }
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn deterministic_transition_sequence_preserves_all_instances() {
        let mut layout = Layout::default();
        let mut expected = ids(&layout);
        let kinds = [
            "library",
            "queue",
            "playlists",
            "metadata",
            "history",
            "devices",
            "spectrum",
            "spectrogram",
        ];
        let edges = [
            Edge::Center,
            Edge::Left,
            Edge::Right,
            Edge::Top,
            Edge::Bottom,
        ];
        for step in 0..160 {
            let panels = ids(&layout);
            let source = panels[step % panels.len()];
            let target = group(&layout, panels[(step * 7 + 1) % panels.len()]);
            match step % 4 {
                0 => expected.push(layout.add(kinds[step / 4 % kinds.len()], Some(target))),
                1 | 2 => assert!(layout.move_panel(
                    source,
                    target,
                    edges[step % edges.len()],
                    Some(step % 5)
                )),
                _ => {
                    assert!(layout.remove(source));
                    expected.retain(|id| *id != source);
                }
            }
            expected.sort_unstable();
            layout.validate().unwrap();
            assert_eq!(ids(&layout), expected);
            let encoded = serde_json::to_vec(&layout).unwrap();
            let reopened: Layout = serde_json::from_slice(&encoded).unwrap();
            reopened.validate().unwrap();
            assert_eq!(layout, reopened);
        }
    }

    #[test]
    fn atomic_save_reopens_exactly_and_invalid_save_preserves_previous_file() {
        static TEST_ID: AtomicU64 = AtomicU64::new(1);
        let directory = std::env::temp_dir().join(format!(
            "rivu-layout-test-{}-{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("workspace.json");
        let mut layout = Layout::default();
        layout.save(&path).unwrap();
        let custom = layout.add("history", Some(2));
        assert!(layout.move_panel(custom, 5, Edge::Bottom, None));
        let split = layout.root.as_ref().unwrap().id();
        assert!(layout.set_ratio(split, 0.73));
        assert!(layout.activate(3));
        layout.save(&path).unwrap();
        assert_eq!(Layout::load(&path).unwrap(), layout);
        let mut invalid = layout.clone();
        invalid.next_id = 1;
        assert!(invalid.save(&path).is_err());
        assert_eq!(Layout::load(&path).unwrap(), layout);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        let mut legacy = serde_json::to_value(&layout).unwrap();
        legacy.as_object_mut().unwrap().remove("version");
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(Layout::load(&path).unwrap(), layout);
        fs::write(&path, b"{broken").unwrap();
        assert!(Layout::load(&path).is_err());
        fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(Layout::load(&path).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
