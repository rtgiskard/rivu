//! Normalized docking trees and their named desktop collection.
//!
//! IDs are runtime-only. Persistence borrows the collection, writes one strict
//! format, and atomically replaces the caller-selected file.
//!
//! Split children are direct nodes. Empty boundary vectors mean equal content
//! spans; explicit boundaries are strictly increasing cumulative fractions in
//! `(0, 1)`, excluding separator gutters. Editing one boundary never changes
//! another. Normalization collapses empty/singleton containers and affinely
//! flattens same-axis splits, without integral sizing or quantization.

pub(super) mod docking;
mod docking_ui;
pub(super) mod interaction;
pub(super) mod layout_geometry;
pub(super) mod layout_manager;
pub(super) mod panel_menu;
pub(super) mod save_owner;
mod view;

use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::{BufReader, BufWriter, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::config::{AlbumArtworkOptions, AudioVisualizerOptions, AudioVisualizerStyle};

pub const MAX_SPLIT_DEPTH: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub struct Panel {
    pub id: u64,
    pub kind: String,
    pub show_tab_bar: bool,
    pub audio_visualizer: Option<AudioVisualizerOptions>,
    pub album_artwork: Option<AlbumArtworkOptions>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

impl Edge {
    fn axis(self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Horizontal,
            Self::Top | Self::Bottom => Axis::Vertical,
            Self::Center => unreachable!("center docking does not split"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Split {
        id: u64,
        axis: Axis,
        children: Vec<Node>,
        boundaries: Vec<f64>,
    },
    Tabs {
        id: u64,
        panels: Vec<Panel>,
        active: u64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub root: Option<Node>,
    pub next_id: u64,
    pub locked: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum LayoutTemplate {
    Default,
    Album,
    Visual,
}

impl LayoutTemplate {
    pub const ALL: [Self; 3] = [Self::Default, Self::Album, Self::Visual];

    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Album => "Album",
            Self::Visual => "Visual",
        }
    }

    fn layout(self) -> Layout {
        fn tabs(id: u64, kinds: &[&str], show_tab_bar: bool) -> Node {
            Node::Tabs {
                id,
                panels: kinds
                    .iter()
                    .enumerate()
                    .map(|(index, kind)| Panel {
                        id: id + 1 + index as u64,
                        kind: (*kind).into(),
                        show_tab_bar,
                        audio_visualizer: None,
                        album_artwork: None,
                    })
                    .collect(),
                active: id + 1,
            }
        }
        fn visual_tabs(id: u64, styles: &[AudioVisualizerStyle], show_tab_bar: bool) -> Node {
            Node::Tabs {
                id,
                panels: styles
                    .iter()
                    .enumerate()
                    .map(|(index, &style)| Panel {
                        id: id + 1 + index as u64,
                        kind: "audio_visualizer".into(),
                        show_tab_bar,
                        audio_visualizer: Some(AudioVisualizerOptions {
                            style,
                            grid: false,
                            label: false,
                        }),
                        album_artwork: None,
                    })
                    .collect(),
                active: id + 1,
            }
        }
        fn split(id: u64, axis: Axis, boundary: f64, children: Vec<Node>) -> Node {
            Node::Split {
                id,
                axis,
                boundaries: vec![boundary],
                children,
            }
        }
        // Templates describe new workspaces only; loading never merges these
        // defaults into a saved tree. Browsing leads, playback stays at the foot.
        let (content, transport, main_boundary, next_id) = match self {
            Self::Default => (
                split(
                    2,
                    Axis::Horizontal,
                    0.72,
                    vec![
                        tabs(3, &["library", "albums", "playlists", "history"], true),
                        tabs(8, &["queue", "metadata"], true),
                    ],
                ),
                split(
                    11,
                    Axis::Horizontal,
                    0.21,
                    vec![
                        tabs(12, &["album_artwork"], false),
                        split(
                            14,
                            Axis::Vertical,
                            0.38,
                            vec![
                                visual_tabs(15, &[AudioVisualizerStyle::Spectrum], false),
                                tabs(17, &["transport"], false),
                            ],
                        ),
                    ],
                ),
                0.64,
                19,
            ),
            Self::Album => (
                split(
                    2,
                    Axis::Horizontal,
                    0.72,
                    vec![
                        tabs(3, &["albums", "library", "playlists", "history"], true),
                        split(
                            8,
                            Axis::Vertical,
                            0.55,
                            vec![
                                tabs(9, &["album_artwork"], false),
                                tabs(11, &["queue", "metadata"], true),
                            ],
                        ),
                    ],
                ),
                tabs(14, &["transport"], true),
                0.74,
                16,
            ),
            Self::Visual => (
                visual_tabs(
                    2,
                    &[
                        AudioVisualizerStyle::BarsRings,
                        AudioVisualizerStyle::Spectrum,
                        AudioVisualizerStyle::Spectrogram,
                        AudioVisualizerStyle::Waveform,
                    ],
                    true,
                ),
                tabs(7, &["transport"], true),
                0.74,
                9,
            ),
        };
        Layout {
            root: Some(split(
                1,
                Axis::Vertical,
                main_boundary,
                vec![content, transport],
            )),
            next_id,
            locked: true,
        }
    }
}

/// Cheap immutable snapshots; edits copy only the map and the edited layout
/// when a background save or another named layout still shares them.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutSet {
    active: Arc<str>,
    layouts: Arc<BTreeMap<String, Arc<Layout>>>,
}

// Runtime types intentionally do not implement serde: IDs can never leak into
// the file, and saving does not construct an owned copy of any layout.
mod persistence {
    use super::*;
    use serde::ser::{SerializeMap, SerializeSeq};

    #[derive(Serialize)]
    pub(super) struct View<'a> {
        active: &'a str,
        layouts: LayoutsRef<'a>,
    }

    impl<'a> From<&'a LayoutSet> for View<'a> {
        fn from(set: &'a LayoutSet) -> Self {
            Self {
                active: &set.active,
                layouts: LayoutsRef(&set.layouts),
            }
        }
    }

    struct LayoutsRef<'a>(&'a BTreeMap<String, Arc<Layout>>);
    struct NodeRef<'a>(&'a Node);
    struct ChildrenRef<'a>(&'a [Node]);
    struct PanelsRef<'a>(&'a [Panel]);

    #[derive(Serialize)]
    struct LayoutView<'a> {
        root: Option<NodeRef<'a>>,
    }

    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "lowercase")]
    enum NodeView<'a> {
        Split {
            axis: Axis,
            children: ChildrenRef<'a>,
            #[serde(skip_serializing_if = "boundaries_empty")]
            boundaries: &'a [f64],
        },
        Tabs {
            panels: PanelsRef<'a>,
            #[serde(skip_serializing_if = "is_zero")]
            active: usize,
        },
    }

    #[derive(PartialEq, Serialize)]
    struct PanelView<'a> {
        kind: &'a str,
        #[serde(skip_serializing_if = "is_true")]
        show_tab_bar: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        style: Option<AudioVisualizerStyle>,
        #[serde(skip_serializing_if = "Option::is_none")]
        grid: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        album_artwork: Option<AlbumArtworkOptions>,
    }

    impl<'a> From<&'a Panel> for PanelView<'a> {
        fn from(panel: &'a Panel) -> Self {
            Self {
                kind: &panel.kind,
                show_tab_bar: panel.show_tab_bar,
                style: panel.audio_visualizer.map(|options| options.style),
                grid: panel.audio_visualizer.map(|options| options.grid),
                label: panel.audio_visualizer.map(|options| options.label),
                album_artwork: panel.album_artwork,
            }
        }
    }

    pub(super) fn roots_eq(left: Option<&Node>, right: Option<&Node>) -> bool {
        match (left, right) {
            (None, None) => true,
            (
                Some(Node::Split {
                    axis: a,
                    children: ac,
                    boundaries: ab,
                    ..
                }),
                Some(Node::Split {
                    axis: b,
                    children: bc,
                    boundaries: bb,
                    ..
                }),
            ) => {
                a == b
                    && ab == bb
                    && ac.len() == bc.len()
                    && ac.iter().zip(bc).all(|(a, b)| roots_eq(Some(a), Some(b)))
            }
            (
                Some(Node::Tabs {
                    panels: a,
                    active: aa,
                    ..
                }),
                Some(Node::Tabs {
                    panels: b,
                    active: ba,
                    ..
                }),
            ) => {
                a.len() == b.len()
                    && a.iter().position(|panel| panel.id == *aa)
                        == b.iter().position(|panel| panel.id == *ba)
                    && a.iter()
                        .zip(b)
                        .all(|(a, b)| PanelView::from(a) == PanelView::from(b))
            }
            _ => false,
        }
    }

    fn is_true(value: &bool) -> bool {
        *value
    }
    fn is_zero(value: &usize) -> bool {
        *value == 0
    }
    fn boundaries_empty(value: &&[f64]) -> bool {
        value.is_empty()
    }
    fn default_true() -> bool {
        true
    }

    impl Serialize for LayoutsRef<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut map = serializer.serialize_map(Some(self.0.len()))?;
            for (name, layout) in self.0 {
                map.serialize_entry(
                    name,
                    &LayoutView {
                        root: layout.root.as_ref().map(NodeRef),
                    },
                )?;
            }
            map.end()
        }
    }

    impl Serialize for NodeRef<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let view = match self.0 {
                Node::Split {
                    axis,
                    children,
                    boundaries,
                    ..
                } => NodeView::Split {
                    axis: *axis,
                    children: ChildrenRef(children),
                    boundaries,
                },
                Node::Tabs { panels, active, .. } => NodeView::Tabs {
                    panels: PanelsRef(panels),
                    active: panels
                        .iter()
                        .position(|panel| panel.id == *active)
                        .ok_or_else(|| {
                            <S::Error as serde::ser::Error>::custom("invalid active panel")
                        })?,
                },
            };
            view.serialize(serializer)
        }
    }

    impl Serialize for ChildrenRef<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
            for child in self.0 {
                sequence.serialize_element(&NodeRef(child))?;
            }
            sequence.end()
        }
    }

    impl Serialize for PanelsRef<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
            for panel in self.0 {
                sequence.serialize_element(&PanelView::from(panel))?;
            }
            sequence.end()
        }
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Input {
        active: String,
        #[serde(deserialize_with = "read_layouts")]
        layouts: BTreeMap<String, LayoutInput>,
    }

    // A map visitor rejects duplicate keys instead of silently losing a layout.
    fn read_layouts<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, LayoutInput>, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = BTreeMap<String, LayoutInput>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("uniquely named layouts")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut layouts = BTreeMap::new();
                while let Some((name, layout)) = map.next_entry::<String, LayoutInput>()? {
                    if layouts.insert(name, layout).is_some() {
                        return Err(serde::de::Error::custom("duplicate layout name"));
                    }
                }
                Ok(layouts)
            }
        }
        deserializer.deserialize_map(Visitor)
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LayoutInput {
        // deserialize_with makes a missing root an error, while allowing null.
        #[serde(deserialize_with = "read_root")]
        root: Option<NodeInput>,
    }

    fn read_root<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<NodeInput>, D::Error> {
        Option::deserialize(deserializer)
    }

    #[derive(Deserialize)]
    #[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
    enum NodeInput {
        Split {
            axis: Axis,
            children: Vec<NodeInput>,
            #[serde(default)]
            boundaries: Vec<f64>,
        },
        Tabs {
            panels: Vec<PanelInput>,
            #[serde(default)]
            active: usize,
        },
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PanelInput {
        kind: String,
        #[serde(default = "default_true")]
        show_tab_bar: bool,
        #[serde(default, deserialize_with = "present")]
        style: Option<AudioVisualizerStyle>,
        #[serde(default, deserialize_with = "present")]
        grid: Option<bool>,
        #[serde(default, deserialize_with = "present")]
        label: Option<bool>,
        #[serde(default, deserialize_with = "present")]
        album_artwork: Option<AlbumArtworkOptions>,
    }

    fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
        deserializer: D,
    ) -> Result<Option<T>, D::Error> {
        T::deserialize(deserializer).map(Some)
    }

    fn allocate(next_id: &mut u64) -> Result<u64> {
        let id = *next_id;
        *next_id = id
            .checked_add(1)
            .ok_or_else(|| anyhow!("layout ID space exhausted"))?;
        Ok(id)
    }

    impl NodeInput {
        fn into_node(self, next_id: &mut u64, depth: usize) -> Result<Node> {
            if matches!(&self, Self::Split { .. }) && depth >= MAX_SPLIT_DEPTH {
                return Err(anyhow!("split depth exceeds {MAX_SPLIT_DEPTH}"));
            }
            let id = allocate(next_id)?;
            Ok(match self {
                Self::Split {
                    axis,
                    children,
                    boundaries,
                } => {
                    if !valid_boundaries(&boundaries, children.len()) {
                        return Err(anyhow!("invalid split boundaries"));
                    }
                    Node::Split {
                        id,
                        axis,
                        children: children
                            .into_iter()
                            .map(|child| child.into_node(next_id, depth + 1))
                            .collect::<Result<_>>()?,
                        boundaries,
                    }
                }
                Self::Tabs { panels, active } => {
                    let panels = panels.into_iter().map(|panel| {
                        let audio_visualizer = match (panel.style, panel.grid, panel.label) {
                            (None, None, None) => None,
                            (Some(style), Some(grid), Some(label)) if panel.kind == "audio_visualizer" =>
                                Some(AudioVisualizerOptions { style, grid, label }),
                            _ => return Err(anyhow!("visualizer options must be complete and belong to an audio_visualizer")),
                        };
                        if let Some(options) = panel.album_artwork
                            && (panel.kind != "album_artwork" || !options.period_seconds.is_finite() || options.period_seconds <= 0.0)
                        {
                            return Err(anyhow!("artwork options require an album_artwork and a positive finite period"));
                        }
                        Ok(Panel { id: allocate(next_id)?, kind: panel.kind,
                            show_tab_bar: panel.show_tab_bar, audio_visualizer,
                            album_artwork: panel.album_artwork })
                    }).collect::<Result<Vec<_>>>()?;
                    let active = panels
                        .get(active)
                        .ok_or_else(|| anyhow!("invalid active tab index {active}"))?
                        .id;
                    Node::Tabs { id, panels, active }
                }
            })
        }
    }

    impl Input {
        pub(super) fn into_set(self) -> Result<LayoutSet> {
            let mut layouts = BTreeMap::new();
            for (name, input) in self.layouts {
                let name = checked_name(&name)?.to_owned();
                let mut next_id = 1;
                let root = input
                    .root
                    .map(|root| root.into_node(&mut next_id, 0))
                    .transpose()?;
                let layout = Layout {
                    root,
                    next_id,
                    locked: true,
                };
                layout.validate().map_err(|error| anyhow!(error))?;
                if layouts.insert(name, Arc::new(layout)).is_some() {
                    return Err(anyhow!("duplicate trimmed layout name"));
                }
            }
            let active = Arc::from(checked_name(&self.active)?);
            let set = LayoutSet {
                active,
                layouts: Arc::new(layouts),
            };
            set.validate()?;
            Ok(set)
        }
    }
}

fn checked_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("layout name must not be empty"));
    }
    Ok(name)
}

fn valid_boundaries(boundaries: &[f64], count: usize) -> bool {
    boundaries.is_empty()
        || (boundaries.len() == count.saturating_sub(1)
            && boundaries.iter().enumerate().all(|(index, &value)| {
                value.is_finite()
                    && value > 0.0
                    && value < 1.0
                    && (index == 0 || value > boundaries[index - 1])
            }))
}

fn boundary_at(boundaries: &[f64], count: usize, index: usize) -> f64 {
    if index == count {
        1.0
    } else if boundaries.is_empty() {
        index as f64 / count as f64
    } else if index == 0 {
        0.0
    } else {
        boundaries[index - 1]
    }
}

/// Content spans in logical pixels. Pane edges and gutters are rounded
/// cumulatively in physical pixels, so no rounding error accumulates.
pub fn split_geometry(
    boundaries: &[f64],
    count: usize,
    length: f64,
    gutter: f64,
    scale: f64,
) -> impl ExactSizeIterator<Item = (f64, f64)> + '_ {
    let length = length.max(0.0);
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let gutter = (if count > 1 {
        (gutter.max(0.0) * scale).round() / scale
    } else {
        0.0
    })
    .min(length / count.saturating_sub(1).max(1) as f64);
    let content = (length - gutter * count.saturating_sub(1) as f64).max(0.0);
    (0..count).map(move |index| {
        let start = ((content * boundary_at(boundaries, count, index) * scale).round() / scale)
            .min(content)
            + gutter * index as f64;
        let end = if index + 1 == count {
            length
        } else {
            ((content * boundary_at(boundaries, count, index + 1) * scale).round() / scale)
                .min(content)
                + gutter * index as f64
        };
        (start, (end - start).max(0.0))
    })
}

// Project collapse and same-axis flattening before consuming any containers.
// Tabs have depth zero; a singleton split inherits its surviving child's shape.
fn normalized_shape(node: &Node) -> Option<(Option<Axis>, usize)> {
    let Node::Split { axis, children, .. } = node else {
        return match node {
            Node::Tabs { panels, .. } if panels.is_empty() => None,
            _ => Some((None, 0)),
        };
    };
    let mut first = None;
    let mut multiple = false;
    let mut max_depth = 0;
    for child in children {
        let Some((child_axis, depth)) = normalized_shape(child) else {
            continue;
        };
        if first.is_some() {
            multiple = true;
        } else {
            first = Some((child_axis, depth));
        }
        let depth = if child_axis == Some(*axis) {
            depth - 1
        } else {
            depth
        };
        max_depth = max_depth.max(depth);
    }
    if multiple {
        Some((Some(*axis), max_depth + 1))
    } else {
        first
    }
}

// Stream normalized pane ends using the same bottom-up affine arithmetic as
// normalize. No temporary tree or child list is needed for this preflight.
fn normalized_ends(node: &Node, wanted_axis: Axis, emit: &mut dyn FnMut(f64) -> bool) -> bool {
    let Node::Split {
        axis,
        children,
        boundaries,
        ..
    } = node
    else {
        return emit(1.0);
    };
    let mut surviving = children
        .iter()
        .enumerate()
        .filter(|(_, child)| normalized_shape(child).is_some());
    let Some((first_index, first)) = surviving.next() else {
        return true;
    };
    if surviving.next().is_none() {
        return normalized_ends(first, wanted_axis, emit);
    }
    if *axis != wanted_axis {
        return emit(1.0);
    }
    let last = children
        .iter()
        .rposition(|child| normalized_shape(child).is_some())
        .expect("surviving child");
    let mut start = 0.0;
    for (index, child) in children.iter().enumerate().skip(first_index) {
        if normalized_shape(child).is_none() {
            continue;
        }
        let end = if index == last {
            1.0
        } else {
            boundary_at(boundaries, children.len(), index + 1)
        };
        if !normalized_ends(child, wanted_axis, &mut |fraction| {
            emit(if fraction == 1.0 {
                end
            } else {
                start + (end - start) * fraction
            })
        }) {
            return false;
        }
        start = end;
    }
    true
}

fn normalized_boundaries_representable(node: &Node) -> bool {
    if let Node::Split { children, .. } = node
        && !children.iter().all(normalized_boundaries_representable)
    {
        return false;
    }
    let Some((Some(axis), _)) = normalized_shape(node) else {
        return true;
    };
    let mut previous = 0.0;
    normalized_ends(node, axis, &mut |end| {
        let valid = end.is_finite() && end > previous && end <= 1.0;
        previous = end;
        valid
    })
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
            Self::Split { children, .. } => children.iter().find_map(|child| child.find(target)),
            Self::Tabs { .. } => None,
        }
    }

    fn find_mut(&mut self, target: u64) -> Option<&mut Self> {
        if self.id() == target {
            return Some(self);
        }
        match self {
            Self::Split { children, .. } => {
                children.iter_mut().find_map(|child| child.find_mut(target))
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
            Self::Split { children, .. } => children
                .iter()
                .find_map(|child| child.panel_group(panel_id)),
        }
    }

    pub(super) fn find_panel(&self, predicate: &impl Fn(&Panel) -> bool) -> Option<&Panel> {
        match self {
            Self::Tabs { panels, .. } => panels.iter().find(|panel| predicate(panel)),
            Self::Split { children, .. } => children
                .iter()
                .find_map(|child| child.find_panel(predicate)),
        }
    }

    fn first_tabs(&self) -> u64 {
        match self {
            Self::Tabs { id, .. } => *id,
            Self::Split { children, .. } => children[0].first_tabs(),
        }
    }

    pub(super) fn visit_panels<'a>(&'a self, active_only: bool, visit: &mut impl FnMut(&'a Panel)) {
        match self {
            Self::Tabs { panels, active, .. } => {
                for panel in panels {
                    if !active_only || panel.id == *active {
                        visit(panel);
                    }
                }
            }
            Self::Split { children, .. } => {
                for child in children {
                    child.visit_panels(active_only, visit);
                }
            }
        }
    }

    fn map_node<F: FnOnce(Self) -> Self>(mut self, target: u64, update: &mut Option<F>) -> Self {
        if update.is_none() {
            return self;
        }
        if self.id() == target {
            return update.take().expect("matched update")(self);
        }
        if let Self::Split { children, .. } = &mut self {
            // Ownership traversal reuses the existing child allocation.
            for child in children {
                let placeholder = Self::Tabs {
                    id: 0,
                    panels: Vec::new(),
                    active: 0,
                };
                *child = std::mem::replace(child, placeholder).map_node(target, update);
                if update.is_none() {
                    break;
                }
            }
        }
        self
    }

    fn normalize(self) -> Option<Self> {
        match self {
            Self::Tabs { ref panels, .. } if panels.is_empty() => None,
            Self::Tabs { .. } => Some(self),
            Self::Split {
                id,
                axis,
                mut children,
                mut boundaries,
            } => {
                let count = children.len();
                // Remove the boundary immediately after an empty child. This
                // gives its span to the following child, or the preceding last
                // child, without moving any other surviving separator.
                let mut index = 0;
                while index < children.len() {
                    let placeholder = Self::Tabs {
                        id: 0,
                        panels: Vec::new(),
                        active: 0,
                    };
                    let node = std::mem::replace(&mut children[index], placeholder);
                    if let Some(node) = node.normalize() {
                        children[index] = node;
                        index += 1;
                    } else {
                        if boundaries.is_empty() && children.len() > 1 {
                            boundaries.extend((1..count).map(|i| i as f64 / count as f64));
                        }
                        children.remove(index);
                        if !boundaries.is_empty() {
                            boundaries.remove(index.min(boundaries.len() - 1));
                        }
                    }
                }
                if children.is_empty() {
                    return None;
                }
                if children.len() == 1 {
                    return children.pop();
                }
                let flattened_len: usize = children
                    .iter()
                    .map(|child| match child {
                        Self::Split {
                            axis: child_axis,
                            children,
                            ..
                        } if *child_axis == axis => children.len(),
                        _ => 1,
                    })
                    .sum();
                if flattened_len == children.len() {
                    return Some(Self::Split {
                        id,
                        axis,
                        children,
                        boundaries,
                    });
                }
                let count = children.len();
                let mut flattened = Vec::with_capacity(flattened_len);
                let mut transformed = Vec::with_capacity(flattened_len - 1);
                for (index, child) in children.into_iter().enumerate() {
                    let start = boundary_at(&boundaries, count, index);
                    let end = boundary_at(&boundaries, count, index + 1);
                    match child {
                        Self::Split {
                            axis: child_axis,
                            children: inner,
                            boundaries: inner_bounds,
                            ..
                        } if child_axis == axis => {
                            for inner_index in 1..inner.len() {
                                transformed.push(
                                    start
                                        + (end - start)
                                            * boundary_at(&inner_bounds, inner.len(), inner_index),
                                );
                            }
                            flattened.extend(inner);
                        }
                        node => flattened.push(node),
                    }
                    if index + 1 < count {
                        transformed.push(end);
                    }
                }
                Some(Self::Split {
                    id,
                    axis,
                    children: flattened,
                    boundaries: transformed,
                })
            }
        }
    }

    fn dock(self, panel: Panel, edge: Edge, tabs_id: u64, split_id: u64) -> Self {
        let incoming = Self::Tabs {
            id: tabs_id,
            active: panel.id,
            panels: vec![panel],
        };
        let (first, second) = match edge {
            Edge::Left | Edge::Top => (incoming, self),
            _ => (self, incoming),
        };
        Self::Split {
            id: split_id,
            axis: edge.axis(),
            children: vec![first, second],
            boundaries: Vec::new(),
        }
    }
}

impl Default for Layout {
    fn default() -> Self {
        LayoutTemplate::Default.layout()
    }
}

impl Layout {
    /// Add and activate an instance. Missing targets use the first tabs; split
    /// targets select their first tabs. Kinds are opaque panel catalog keys.
    pub fn add(&mut self, kind: &str, target: Option<u64>) -> u64 {
        let id = self.next_id;
        let count = if self.root.is_some() { 1 } else { 2 };
        let next_id = id.checked_add(count).expect("layout ID space exhausted");
        let panel = Panel {
            id,
            kind: kind.into(),
            show_tab_bar: true,
            audio_visualizer: None,
            album_artwork: None,
        };
        if let Some(root) = &mut self.root {
            let target = target
                .and_then(|id| root.find(id))
                .unwrap_or(root)
                .first_tabs();
            let Some(Node::Tabs { panels, active, .. }) = root.find_mut(target) else {
                unreachable!("tabs target");
            };
            panels.push(panel);
            *active = id;
        } else {
            self.root = Some(Node::Tabs {
                id: id + 1,
                panels: vec![panel],
                active: id,
            });
        }
        self.next_id = next_id;
        id
    }

    /// Build a new-panel docking candidate without changing this layout.
    /// Unlike add, an explicit destination must exist; None is only for an empty layout.
    pub fn with_docked_panel(
        &self,
        kind: &str,
        target: Option<u64>,
        edge: Edge,
        index: Option<usize>,
    ) -> Option<(Self, u64)> {
        match (&self.root, target) {
            (Some(root), Some(target)) => {
                let node = root.find(target)?;
                if edge == Edge::Center && !matches!(node, Node::Tabs { .. }) {
                    return None;
                }
            }
            (None, None) if edge == Edge::Center => {}
            _ => return None,
        }
        let count = if self.root.is_none() {
            2
        } else if edge == Edge::Center {
            1
        } else {
            3
        };
        self.next_id.checked_add(count)?;
        let mut candidate = self.clone();
        let panel = candidate.add(kind, target);
        if let Some(target) = target
            && !candidate.move_panel(panel, target, edge, index)
        {
            return None;
        }
        Some((candidate, panel))
    }

    fn take_panel(&mut self, group: u64, panel_id: u64) -> (Panel, usize, u64) {
        let Some(Node::Tabs { panels, active, .. }) =
            self.root.as_mut().and_then(|root| root.find_mut(group))
        else {
            unreachable!("checked panel group");
        };
        let index = panels
            .iter()
            .position(|panel| panel.id == panel_id)
            .expect("checked panel");
        let previous_active = *active;
        let panel = panels.remove(index);
        if *active == panel_id {
            *active = panels
                .get(index.min(panels.len().saturating_sub(1)))
                .map_or(0, |panel| panel.id);
        }
        (panel, index, previous_active)
    }

    fn insert_panel(&mut self, group: u64, panel: Panel, index: Option<usize>, active_id: u64) {
        let Some(Node::Tabs { panels, active, .. }) =
            self.root.as_mut().and_then(|root| root.find_mut(group))
        else {
            unreachable!("preserved panel group");
        };
        panels.insert(index.unwrap_or(panels.len()).min(panels.len()), panel);
        *active = active_id;
    }

    pub fn remove(&mut self, panel_id: u64) -> bool {
        let Some(group) = self
            .root
            .as_ref()
            .and_then(|root| root.panel_group(panel_id))
            .map(Node::id)
        else {
            return false;
        };
        let (panel, index, active) = self.take_panel(group, panel_id);
        if !normalized_boundaries_representable(self.root.as_ref().expect("source root")) {
            self.insert_panel(group, panel, Some(index), active);
            return false;
        }
        self.root = self.root.take().and_then(Node::normalize);
        true
    }

    /// Center indices refer to the destination after source removal and clamp
    /// to its end. Split targets accept edge drops. A lone self-drop is a no-op.
    /// All invalid edits preserve both the tree and allocator exactly.
    /// Finite-resolution boundaries may reject a structural edit when its
    /// normalized panes cannot have distinct representable f64 boundaries.
    pub fn move_panel(
        &mut self,
        panel_id: u64,
        target_node: u64,
        edge: Edge,
        index: Option<usize>,
    ) -> bool {
        let Some(root) = &self.root else {
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
        let group = source.id();
        if group == target_node && matches!(source, Node::Tabs { panels, .. } if panels.len() == 1)
        {
            return true;
        }
        let next_id = if edge == Edge::Center {
            self.next_id
        } else {
            let Some(next_id) = self.next_id.checked_add(2) else {
                return false;
            };
            next_id
        };
        let (panel, old_index, old_active) = self.take_panel(group, panel_id);
        let tabs_id = self.next_id;
        let split_id = tabs_id.saturating_add(1);
        if edge == Edge::Center
            && !normalized_boundaries_representable(self.root.as_ref().expect("source root"))
        {
            self.insert_panel(group, panel, Some(old_index), old_active);
            return false;
        }
        if edge == Edge::Center {
            self.insert_panel(target_node, panel, index, panel_id);
        } else {
            // Normalize only after docking: ancestor destinations and their IDs
            // remain reachable even when removal empties one of their children.
            self.root = self.root.take().map(|root| {
                root.map_node(
                    target_node,
                    &mut Some(|target: Node| target.dock(panel, edge, tabs_id, split_id)),
                )
            });
        }
        if edge != Edge::Center
            && (normalized_shape(self.root.as_ref().expect("destination root"))
                .is_some_and(|(_, depth)| depth > MAX_SPLIT_DEPTH)
                || !normalized_boundaries_representable(
                    self.root.as_ref().expect("destination root"),
                ))
        {
            let mut recovered = None;
            self.root = self.root.take().map(|root| {
                root.map_node(
                    split_id,
                    &mut Some(|wrapper: Node| {
                        let Node::Split { mut children, .. } = wrapper else {
                            unreachable!("new docking split");
                        };
                        let incoming = if matches!(edge, Edge::Left | Edge::Top) {
                            0
                        } else {
                            1
                        };
                        let Node::Tabs { mut panels, .. } = children.remove(incoming) else {
                            unreachable!("incoming tabs");
                        };
                        recovered = panels.pop();
                        children.pop().expect("original target")
                    }),
                )
            });
            let panel = recovered.expect("incoming panel");
            self.insert_panel(group, panel, Some(old_index), old_active);
            return false;
        }
        self.root = self.root.take().and_then(Node::normalize);
        self.next_id = next_id;
        true
    }

    /// Move one absolute cumulative content boundary. Neighboring boundaries
    /// and the allocator remain exactly unchanged; equal defaults materialize
    /// only after a valid edit.
    pub fn set_boundary(&mut self, split_id: u64, index: usize, fraction: f64) -> bool {
        if !fraction.is_finite() {
            return false;
        }
        let Some(Node::Split {
            children,
            boundaries,
            ..
        }) = self.root.as_mut().and_then(|root| root.find_mut(split_id))
        else {
            return false;
        };
        let count = children.len();
        if index >= count.saturating_sub(1)
            || fraction <= boundary_at(boundaries, count, index)
            || fraction >= boundary_at(boundaries, count, index + 2)
        {
            return false;
        }
        if boundaries.is_empty() {
            boundaries.extend((1..count).map(|i| i as f64 / count as f64));
        }
        boundaries[index] = fraction;
        true
    }

    /// Reconcile intended post-edit outer node sizes with the normalized tree.
    /// The caller adjusts captured measurements for the edit's new/removed
    /// gutters. Absent nodes are ignored; incompletely measured splits retain
    /// their proportions. This runs after edits, never during rendering.
    pub fn preserve_region_sizes(
        &mut self,
        sizes: &std::collections::HashMap<u64, (f64, f64)>,
        gutter: f64,
    ) {
        fn reconcile(
            node: &mut Node,
            sizes: &std::collections::HashMap<u64, (f64, f64)>,
            gutter: f64,
        ) {
            let Node::Split {
                id,
                axis,
                children,
                boundaries,
            } = node
            else {
                return;
            };
            let dimension = |size: &(f64, f64)| {
                if *axis == Axis::Horizontal {
                    size.0
                } else {
                    size.1
                }
            };
            let content = sizes
                .get(id)
                .map(|size| dimension(size) - gutter * children.len().saturating_sub(1) as f64);
            if content.is_some_and(|length| length.is_finite() && length > 0.0) {
                let total = children.iter().try_fold(0.0, |sum, child| {
                    let length = dimension(sizes.get(&child.id())?);
                    (length.is_finite() && length > 0.0).then_some(sum + length)
                });
                if let Some(total) = total.filter(|total| total.is_finite() && *total > 0.0) {
                    let mut cumulative = 0.0;
                    let mut previous = 0.0;
                    let representable =
                        children
                            .iter()
                            .take(children.len().saturating_sub(1))
                            .all(|child| {
                                cumulative += dimension(&sizes[&child.id()]);
                                let fraction = cumulative / total;
                                let valid = fraction > previous && fraction < 1.0;
                                previous = fraction;
                                valid
                            });
                    // Normalize the intended spans together to absorb rounding
                    // differences between parent and measured child extents.
                    if representable {
                        boundaries.clear();
                        boundaries.reserve(children.len().saturating_sub(1));
                        cumulative = 0.0;
                        for child in children.iter().take(children.len().saturating_sub(1)) {
                            cumulative += dimension(&sizes[&child.id()]);
                            boundaries.push(cumulative / total);
                        }
                    }
                }
            }
            for child in children {
                reconcile(child, sizes, gutter);
            }
        }
        if let Some(root) = &mut self.root {
            reconcile(root, sizes, gutter);
        }
    }

    pub fn activate(&mut self, panel_id: u64) -> bool {
        let Some(group) = self
            .root
            .as_ref()
            .and_then(|root| root.panel_group(panel_id))
            .map(Node::id)
        else {
            return false;
        };
        let Some(Node::Tabs { active, .. }) =
            self.root.as_mut().and_then(|root| root.find_mut(group))
        else {
            unreachable!("panel group");
        };
        *active = panel_id;
        true
    }

    fn panel_mut(&mut self, id: u64) -> Option<&mut Panel> {
        let group = self.root.as_ref()?.panel_group(id)?.id();
        let Node::Tabs { panels, .. } = self.root.as_mut()?.find_mut(group)? else {
            return None;
        };
        panels.iter_mut().find(|panel| panel.id == id)
    }

    pub fn set_tab_bar_visible(&mut self, panel_id: u64, visible: bool) -> bool {
        let Some(panel) = self.panel_mut(panel_id) else {
            return false;
        };
        panel.show_tab_bar = visible;
        true
    }

    pub fn initialize_audio_visualizers(&mut self, defaults: AudioVisualizerOptions) -> bool {
        fn initialize(node: &mut Node, defaults: AudioVisualizerOptions) -> bool {
            match node {
                Node::Split { children, .. } => {
                    let mut changed = false;
                    for child in children {
                        changed |= initialize(child, defaults);
                    }
                    changed
                }
                Node::Tabs { panels, .. } => {
                    let mut changed = false;
                    for panel in panels {
                        if panel.kind == "audio_visualizer" && panel.audio_visualizer.is_none() {
                            panel.audio_visualizer = Some(defaults);
                            changed = true;
                        }
                    }
                    changed
                }
            }
        }
        self.root
            .as_mut()
            .is_some_and(|root| initialize(root, defaults))
    }

    pub fn audio_visualizer_options(&self, id: u64) -> Option<AudioVisualizerOptions> {
        let Node::Tabs { panels, .. } = self.root.as_ref()?.panel_group(id)? else {
            return None;
        };
        panels
            .iter()
            .find(|panel| panel.id == id && panel.kind == "audio_visualizer")?
            .audio_visualizer
    }

    pub fn set_audio_visualizer_options(
        &mut self,
        id: u64,
        options: AudioVisualizerOptions,
    ) -> bool {
        let Some(panel) = self
            .panel_mut(id)
            .filter(|panel| panel.kind == "audio_visualizer")
        else {
            return false;
        };
        panel.audio_visualizer = Some(options);
        true
    }

    pub fn initialize_album_artworks(&mut self, default: AlbumArtworkOptions) -> bool {
        fn initialize(node: &mut Node, default: AlbumArtworkOptions) -> bool {
            match node {
                Node::Split { children, .. } => {
                    let mut changed = false;
                    for child in children {
                        changed |= initialize(child, default);
                    }
                    changed
                }
                Node::Tabs { panels, .. } => {
                    let mut changed = false;
                    for panel in panels {
                        if panel.kind == "album_artwork" && panel.album_artwork.is_none() {
                            panel.album_artwork = Some(default);
                            changed = true;
                        }
                    }
                    changed
                }
            }
        }
        self.root
            .as_mut()
            .is_some_and(|root| initialize(root, default))
    }

    pub fn album_artwork_options(&self, id: u64) -> Option<AlbumArtworkOptions> {
        let Node::Tabs { panels, .. } = self.root.as_ref()?.panel_group(id)? else {
            return None;
        };
        panels
            .iter()
            .find(|panel| panel.id == id && panel.kind == "album_artwork")?
            .album_artwork
    }

    pub fn set_album_artwork_options(&mut self, id: u64, options: AlbumArtworkOptions) -> bool {
        if !options.period_seconds.is_finite() || options.period_seconds <= 0.0 {
            return false;
        }
        let Some(panel) = self
            .panel_mut(id)
            .filter(|panel| panel.kind == "album_artwork")
        else {
            return false;
        };
        panel.album_artwork = Some(options);
        true
    }

    pub fn contains_panel(&self, panel_id: u64) -> bool {
        self.root
            .as_ref()
            .is_some_and(|root| root.panel_group(panel_id).is_some())
    }

    pub(super) fn is_panel_active(&self, panel_id: u64) -> bool {
        matches!(
            self.root.as_ref().and_then(|root| root.panel_group(panel_id)),
            Some(Node::Tabs { active, .. }) if *active == panel_id
        )
    }

    pub fn active_panels(&self) -> Vec<&Panel> {
        let mut panels = Vec::new();
        if let Some(root) = &self.root {
            root.visit_panels(true, &mut |panel| panels.push(panel));
        }
        panels
    }

    pub fn panels(&self) -> Vec<&Panel> {
        let mut panels = Vec::new();
        if let Some(root) = &self.root {
            root.visit_panels(false, &mut |panel| panels.push(panel));
        }
        panels
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        fn record(
            id: u64,
            next_id: u64,
            ids: &mut HashSet<u64>,
        ) -> std::result::Result<(), String> {
            if id == 0 || id >= next_id {
                return Err(format!("invalid layout ID {id} for next_id {next_id}"));
            }
            if !ids.insert(id) {
                return Err(format!("duplicate layout ID {id}"));
            }
            Ok(())
        }
        fn visit(
            node: &Node,
            depth: usize,
            next_id: u64,
            ids: &mut HashSet<u64>,
        ) -> std::result::Result<(), String> {
            record(node.id(), next_id, ids)?;
            match node {
                Node::Split {
                    axis,
                    children,
                    boundaries,
                    ..
                } => {
                    if depth >= MAX_SPLIT_DEPTH {
                        return Err(format!("split depth exceeds {MAX_SPLIT_DEPTH}"));
                    }
                    if children.len() < 2 {
                        return Err("split must have at least two children".into());
                    }
                    if !valid_boundaries(boundaries, children.len()) {
                        return Err("invalid split boundaries".into());
                    }
                    for child in children {
                        if matches!(child, Node::Split { axis: child_axis, .. } if child_axis == axis)
                        {
                            return Err("same-axis split children must be flattened".into());
                        }
                        visit(child, depth + 1, next_id, ids)?;
                    }
                }
                Node::Tabs { panels, active, .. } => {
                    if panels.is_empty() {
                        return Err("tab group must not be empty".into());
                    }
                    for panel in panels {
                        record(panel.id, next_id, ids)?;
                        if panel.audio_visualizer.is_some() && panel.kind != "audio_visualizer" {
                            return Err("visualizer options on a non-visualizer panel".into());
                        }
                    }
                    if !panels.iter().any(|panel| panel.id == *active) {
                        return Err("invalid active panel ID".into());
                    }
                }
            }
            Ok(())
        }
        if self.next_id == 0 {
            return Err("next_id must be nonzero".into());
        }
        if let Some(root) = &self.root {
            visit(root, 0, self.next_id, &mut HashSet::new())?;
        }
        Ok(())
    }
}

impl Default for LayoutSet {
    fn default() -> Self {
        Self {
            active: "Default".into(),
            layouts: Arc::new(
                LayoutTemplate::ALL
                    .into_iter()
                    .map(|template| (template.name().into(), Arc::new(template.layout())))
                    .collect(),
            ),
        }
    }
}

impl LayoutSet {
    /// Compare only file content; equality does not replace snapshot validation.
    fn persisted_eq(&self, other: &Self) -> bool {
        self.active == other.active
            && (Arc::ptr_eq(&self.layouts, &other.layouts)
                || (self.layouts.len() == other.layouts.len()
                    && self.layouts.iter().zip(other.layouts.iter()).all(
                        |((name, layout), (other_name, other_layout))| {
                            name == other_name
                                && (Arc::ptr_eq(layout, other_layout)
                                    || persistence::roots_eq(
                                        layout.root.as_ref(),
                                        other_layout.root.as_ref(),
                                    ))
                        },
                    )))
    }

    pub fn current(&self) -> &Layout {
        self.layouts
            .get(self.active.as_ref())
            .expect("active layout exists")
    }
    pub fn current_mut(&mut self) -> &mut Layout {
        Arc::make_mut(
            Arc::make_mut(&mut self.layouts)
                .get_mut(self.active.as_ref())
                .expect("active layout exists"),
        )
    }
    pub fn active_name(&self) -> &str {
        &self.active
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.layouts.keys().map(String::as_str)
    }

    /// Edit mode is window-local, just as a fresh load starts every tree locked.
    pub(super) fn reset_edit_mode(&mut self) {
        if self.layouts.values().any(|layout| !layout.locked) {
            for layout in Arc::make_mut(&mut self.layouts).values_mut() {
                if !layout.locked {
                    Arc::make_mut(layout).locked = true;
                }
            }
        }
    }

    pub(super) fn unused_name<'a>(&self, name: &'a str) -> Result<&'a str> {
        let name = checked_name(name)?;
        if self.layouts.contains_key(name) {
            return Err(anyhow!("layout {name:?} already exists"));
        }
        Ok(name)
    }

    pub(super) fn selectable_name<'a>(&self, name: &'a str) -> Result<&'a str> {
        let name = checked_name(name)?;
        if !self.layouts.contains_key(name) {
            return Err(anyhow!("layout {name:?} does not exist"));
        }
        Ok(name)
    }

    pub fn select(&mut self, name: &str) -> Result<()> {
        let name = self.selectable_name(name)?;
        if self.active.as_ref() != name {
            self.active = name.into();
        }
        Ok(())
    }

    pub fn duplicate(&mut self, name: &str) -> Result<()> {
        let name = self.unused_name(name)?.to_owned();
        let copy = Arc::clone(
            self.layouts
                .get(self.active.as_ref())
                .expect("active layout exists"),
        );
        Arc::make_mut(&mut self.layouts).insert(name.clone(), copy);
        self.active = name.into();
        Ok(())
    }

    pub fn rename(&mut self, name: &str) -> Result<()> {
        let name = checked_name(name)?;
        if name == self.active.as_ref() {
            return Ok(());
        }
        let name = self.unused_name(name)?.to_owned();
        let layouts = Arc::make_mut(&mut self.layouts);
        let layout = layouts
            .remove(self.active.as_ref())
            .expect("active layout exists");
        layouts.insert(name.clone(), layout);
        self.active = name.into();
        Ok(())
    }

    fn removable_name<'a>(&self, name: &'a str) -> Result<&'a str> {
        let name = checked_name(name)?;
        if name == self.active.as_ref() {
            return Err(anyhow!("cannot remove the current layout"));
        }
        if !self.layouts.contains_key(name) {
            return Err(anyhow!("layout {name:?} does not exist"));
        }
        Ok(name)
    }

    pub fn remove(&mut self, name: &str) -> Result<()> {
        let name = self.removable_name(name)?;
        Arc::make_mut(&mut self.layouts).remove(name);
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.layouts.is_empty() {
            return Err(anyhow!("layout collection is empty"));
        }
        if !self.layouts.contains_key(self.active.as_ref()) {
            return Err(anyhow!("active layout does not exist"));
        }
        for (name, layout) in self.layouts.iter() {
            if checked_name(name)? != name {
                return Err(anyhow!("layout name is not trimmed"));
            }
            layout.validate().map_err(|error| anyhow!(error))?;
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let file = rivu_format::input::open_regular_file(path)
            .with_context(|| format!("opening layout {}", path.display()))?;
        let input: persistence::Input = serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("decoding layout {}", path.display()))?;
        input
            .into_set()
            .with_context(|| format!("invalid layout {}", path.display()))
    }

    /// Worker-only atomic replacement. Validation and serialization must never
    /// run in an interactive GUI callback. Failure preserves the previous file.
    fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("layout path has no filename"))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("creating layout directory {}", parent.display()))?;
        static TEMP_ID: AtomicU64 = AtomicU64::new(1);
        let (temporary, file) = loop {
            let mut temporary_name = name.to_os_string();
            temporary_name.push(format!(
                ".{}.{}.tmp",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            let temporary = parent.join(temporary_name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => break (temporary, file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("creating temporary layout"),
            }
        };
        let result = (|| -> Result<()> {
            let mut writer = BufWriter::new(file);
            serde_json::to_writer_pretty(&mut writer, &persistence::View::from(self))
                .context("encoding layouts")?;
            writer.write_all(b"\n")?;
            writer.flush().context("flushing layouts")?;
            drop(writer);
            fs::rename(&temporary, path)
                .with_context(|| format!("replacing layout {}", path.display()))?;
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
    use serde_json::{Value, json};

    fn persisted_fixture() -> LayoutSet {
        let artwork = artwork_options(crate::config::AlbumArtworkStyle::Plain);
        decoded(one_layout(json!({
            "type":"split", "axis":"horizontal", "boundaries":[0.4],
            "children":[
                {"type":"tabs", "active":1, "panels":[
                    {"kind":"audio_visualizer", "style":"bars", "grid":true, "label":false},
                    {"kind":"album_artwork", "album_artwork":artwork}
                ]},
                {"type":"tabs", "panels":[{"kind":"library"}]}
            ]
        })))
        .unwrap()
    }

    #[test]
    fn persisted_equality_ignores_runtime_identity_and_edit_state() {
        fn reidentify(node: &mut Node) {
            match node {
                Node::Split { id, children, .. } => {
                    *id += 100;
                    for child in children {
                        reidentify(child);
                    }
                }
                Node::Tabs { id, panels, active } => {
                    *id += 100;
                    *active += 100;
                    for panel in panels {
                        panel.id += 100;
                    }
                }
            }
        }
        let original = persisted_fixture();
        let mut independent = persisted_fixture();
        reidentify(independent.current_mut().root.as_mut().unwrap());
        independent.current_mut().next_id += 1000;
        independent.current_mut().locked = !original.current().locked;
        assert_ne!(original, independent);
        assert!(original.persisted_eq(&independent));
        assert!(independent.persisted_eq(&original));
        assert!(original.persisted_eq(&original.clone()));
    }

    #[test]
    fn persisted_equality_detects_each_file_content_change() {
        let original = persisted_fixture();
        let value = encoded(&original);
        let root = "/layouts/Work/root";
        let changes = [
            (format!("{root}/axis"), json!("vertical")),
            (format!("{root}/boundaries/0"), json!(0.6)),
            (format!("{root}/boundaries"), json!([])),
            (format!("{root}/children/0/active"), json!(0)),
            (format!("{root}/children/0/panels/0/kind"), json!("custom")),
            (format!("{root}/children/0/panels/1/show_tab_bar"), json!(false)),
            (format!("{root}/children/0/panels/0/style"), json!("rings")),
            (format!("{root}/children/0/panels/0/grid"), json!(false)),
            (format!("{root}/children/0/panels/0/label"), json!(true)),
            (format!("{root}/children/0/panels/1/album_artwork/period_seconds"), json!(value["layouts"]["Work"]["root"]["children"][0]["panels"][1]["album_artwork"]["period_seconds"].as_f64().unwrap() + 1.0)),
            (format!("{root}/children/0/panels/1/album_artwork/style"), json!("vinyl")),
            (root.to_owned(), Value::Null),
        ];
        for (path, replacement) in changes {
            let mut changed = value.clone();
            // The tab-bar flag is omitted when true.
            if path.ends_with("/show_tab_bar") {
                changed["layouts"]["Work"]["root"]["children"][0]["panels"][1]["show_tab_bar"] =
                    replacement;
            } else {
                *changed.pointer_mut(&path).unwrap() = replacement;
            }
            // Changing the kind also removes options that do not belong to it.
            if path.ends_with("/kind") {
                let panel = changed["layouts"]["Work"]["root"]["children"][0]["panels"][0]
                    .as_object_mut()
                    .unwrap();
                for key in ["style", "grid", "label"] {
                    panel.remove(key);
                }
            }
            let changed = decoded(changed).unwrap();
            assert!(!original.persisted_eq(&changed), "{path}");
            assert!(!changed.persisted_eq(&original), "{path}");
        }
        let artwork_path = format!("{root}/children/0/panels/1/album_artwork");
        for (field, first, second) in [
            ("mode", "static", "rotating"),
            ("direction", "clockwise", "counterclockwise"),
        ] {
            let path = format!("{artwork_path}/{field}");
            let mut changed = value.clone();
            let replacement = if changed.pointer(&path).unwrap() == first {
                second
            } else {
                first
            };
            *changed.pointer_mut(&path).unwrap() = json!(replacement);
            assert!(
                !original.persisted_eq(&decoded(changed).unwrap()),
                "{field}"
            );
        }
        for (panel_index, fields) in [
            (0, &["style", "grid", "label"][..]),
            (1, &["album_artwork"][..]),
        ] {
            let mut changed = value.clone();
            let panel = changed["layouts"]["Work"]["root"]["children"][0]["panels"][panel_index]
                .as_object_mut()
                .unwrap();
            for field in fields {
                panel.remove(*field);
            }
            assert!(!original.persisted_eq(&decoded(changed).unwrap()));
        }
        for path in [
            format!("{root}/children"),
            format!("{root}/children/0/panels"),
        ] {
            let mut changed = value.clone();
            changed
                .pointer_mut(&path)
                .unwrap()
                .as_array_mut()
                .unwrap()
                .swap(0, 1);
            assert!(!original.persisted_eq(&decoded(changed).unwrap()), "{path}");
        }
        let mut renamed = original.clone();
        renamed.rename("Renamed").unwrap();
        assert!(!original.persisted_eq(&renamed));
        let mut selected = original.clone();
        selected.duplicate("Other").unwrap();
        let mut other_selection = selected.clone();
        other_selection.select("Work").unwrap();
        assert!(!selected.persisted_eq(&other_selection));
        assert!(!original.persisted_eq(&selected));
    }

    fn empty() -> Layout {
        Layout {
            root: None,
            next_id: 1,
            locked: false,
        }
    }

    fn single_layout_set() -> LayoutSet {
        LayoutSet {
            active: "Default".into(),
            layouts: Arc::new(BTreeMap::from([("Default".into(), Arc::new(empty()))])),
        }
    }

    fn tabs(id: u64, kind: &str) -> Node {
        Node::Tabs {
            id,
            panels: vec![Panel {
                id: id + 1,
                kind: kind.into(),
                show_tab_bar: true,
                audio_visualizer: None,
                album_artwork: None,
            }],
            active: id + 1,
        }
    }

    fn split(id: u64, axis: Axis, children: Vec<Node>, boundaries: &[f64]) -> Node {
        Node::Split {
            id,
            axis,
            children,
            boundaries: boundaries.to_vec(),
        }
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

    fn boundaries(layout: &Layout) -> &[f64] {
        let Some(Node::Split { boundaries, .. }) = &layout.root else {
            panic!("expected split");
        };
        boundaries
    }

    fn assert_proportions(layout: &Layout, expected: &[u32]) {
        let Some(Node::Split {
            children,
            boundaries,
            ..
        }) = &layout.root
        else {
            panic!("expected split");
        };
        assert_eq!(children.len(), expected.len());
        let total: u32 = expected.iter().sum();
        for (index, &expected) in expected.iter().enumerate() {
            let actual = boundary_at(boundaries, children.len(), index + 1)
                - boundary_at(boundaries, children.len(), index);
            assert!((actual - f64::from(expected) / f64::from(total)).abs() < 1e-14);
        }
    }

    fn nested_layout(depth: usize) -> Layout {
        let mut node = tabs(1, "destination");
        for level in 0..depth {
            let id = 3 + 3 * level as u64;
            node = Node::Split {
                id,
                axis: if level % 2 == 0 {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                },
                children: vec![node, tabs(id + 1, "side")],
                boundaries: Vec::new(),
            };
        }
        Layout {
            root: Some(node),
            next_id: 3 + 3 * depth as u64,
            locked: false,
        }
    }

    fn split_depth(node: &Node) -> usize {
        match node {
            Node::Tabs { .. } => 0,
            Node::Split { children, .. } => 1 + children.iter().map(split_depth).max().unwrap(),
        }
    }

    fn encoded(set: &LayoutSet) -> Value {
        serde_json::to_value(persistence::View::from(set)).unwrap()
    }

    fn decoded(value: Value) -> Result<LayoutSet> {
        serde_json::from_value::<persistence::Input>(value)?.into_set()
    }

    fn one_layout(root: Value) -> Value {
        json!({"active":"Work", "layouts":{"Work":{"root":root}}})
    }

    #[test]
    fn reopening_layouts_resets_edit_mode_to_locked() {
        let mut set = LayoutSet::default();
        assert!(set.current().locked);
        set.current_mut().locked = false;
        let value = encoded(&set);
        assert!(value["layouts"]["Default"].get("locked").is_none());
        let reopened = decoded(value).unwrap();
        assert!(reopened.current().locked);
    }

    #[test]
    fn save_snapshots_share_then_isolate_edited_trees_and_named_operations() {
        let mut set = LayoutSet::default();
        set.current_mut().locked = false;
        let snapshot = set.clone();
        assert!(Arc::ptr_eq(&snapshot.layouts, &set.layouts));
        assert!(Arc::ptr_eq(&snapshot.active, &set.active));
        let previous = encoded(&snapshot);
        set.duplicate("Work").unwrap();
        assert!(Arc::ptr_eq(
            set.layouts.get("Default").unwrap(),
            set.layouts.get("Work").unwrap(),
        ));
        set.current_mut().add("custom", None);
        assert!(!Arc::ptr_eq(
            set.layouts.get("Default").unwrap(),
            set.layouts.get("Work").unwrap(),
        ));
        set.rename("Music").unwrap();
        set.remove("Default").unwrap();
        assert_eq!(encoded(&snapshot), previous);
        assert_eq!(snapshot.active_name(), "Default");
        let mut reopened = snapshot.clone();
        reopened.reset_edit_mode();
        assert!(reopened.current().locked);
        assert!(!snapshot.current().locked);
        assert_eq!(encoded(&reopened), previous);
    }

    fn wire_tabs(kind: &str) -> Value {
        json!({"type":"tabs", "panels":[{"kind":kind}]})
    }

    struct TemporaryDirectory(std::path::PathBuf);

    impl TemporaryDirectory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let path = std::env::temp_dir().join(format!(
                "rivu-layout-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn panel_queries_preserve_tree_order_and_active_tab_visibility() {
        let mut layout = empty();
        let albums = layout.add("albums", None);
        let library = layout.add("library", None);
        let queue = layout.add("queue", None);
        assert!(layout.move_panel(queue, group(&layout, albums), Edge::Right, None));
        let root = layout.root.as_ref().unwrap();
        assert_eq!(
            root.find_panel(&|panel| panel.kind == "albums")
                .map(|panel| panel.id),
            Some(albums)
        );
        assert!(!layout.is_panel_active(albums));
        let mut active = Vec::new();
        root.visit_panels(true, &mut |panel| active.push(panel.id));
        assert_eq!(active, vec![library, queue]);
        let mut all = Vec::new();
        root.visit_panels(false, &mut |panel| all.push(panel.id));
        assert_eq!(all, vec![albums, library, queue]);

        assert!(layout.activate(albums));
        assert!(layout.is_panel_active(albums));
        assert!(layout.remove(albums));
        assert!(
            layout
                .root
                .as_ref()
                .unwrap()
                .find_panel(&|panel| panel.kind == "albums")
                .is_none()
        );
        assert!(!layout.is_panel_active(albums));
    }

    #[test]
    fn same_axis_docking_adds_siblings_without_resizing_other_regions() {
        let mut layout = empty();
        let first = layout.add("first", None);
        let second = layout.add("second", None);
        assert!(layout.move_panel(second, group(&layout, first), Edge::Right, None));
        let split = layout.root.as_ref().unwrap().id();
        assert!(layout.set_boundary(split, 0, 0.75));
        let third = layout.add("third", Some(group(&layout, second)));
        assert!(layout.move_panel(third, group(&layout, second), Edge::Left, None));
        assert_proportions(&layout, &[6, 1, 1]);
        let fourth = layout.add("fourth", Some(group(&layout, third)));
        assert!(layout.move_panel(fourth, group(&layout, third), Edge::Top, None));
        assert_proportions(&layout, &[6, 1, 1]);
        assert_eq!(
            layout
                .panels()
                .iter()
                .map(|panel| panel.id)
                .collect::<Vec<_>>(),
            vec![first, fourth, third, second]
        );
        let Some(Node::Split { children, .. }) = &layout.root else {
            unreachable!();
        };
        assert!(matches!(
            children[1],
            Node::Split {
                axis: Axis::Vertical,
                ..
            }
        ));
        layout.validate().unwrap();
    }

    #[test]
    fn removal_collapses_and_affinely_flattens_without_quantization() {
        let mut layout = Layout {
            next_id: 100,
            locked: false,
            root: Some(split(
                1,
                Axis::Horizontal,
                vec![
                    split(
                        2,
                        Axis::Vertical,
                        vec![
                            tabs(3, "remove"),
                            split(
                                5,
                                Axis::Horizontal,
                                vec![tabs(6, "left"), tabs(8, "middle")],
                                &[2.0 / 3.0],
                            ),
                        ],
                        &[],
                    ),
                    tabs(10, "right"),
                ],
                &[0.6],
            )),
        };
        layout.validate().unwrap();
        assert!(layout.remove(4));
        assert_proportions(&layout, &[2, 1, 2]);
        assert_eq!(boundaries(&layout)[1], 0.6);
        assert_eq!(layout.root.as_ref().unwrap().id(), 1);
        layout.validate().unwrap();
        assert!(layout.remove(7));
        assert_eq!(boundaries(&layout), &[0.6]);
        assert!(layout.remove(9));
        assert_eq!(layout.root.as_ref().unwrap().id(), 10);
        assert!(layout.remove(11));
        assert!(layout.root.is_none());
        let replacement = layout.add("replacement", None);
        assert_eq!(layout.active_panels()[0].id, replacement);
        layout.validate().unwrap();
    }

    #[test]
    fn deletion_gives_span_to_following_or_preceding_last_without_moving_other_boundaries() {
        let original = Layout {
            next_id: 20,
            locked: false,
            root: Some(split(
                1,
                Axis::Horizontal,
                vec![tabs(2, "a"), tabs(4, "b"), tabs(6, "c"), tabs(8, "d")],
                &[0.1, 0.3, 0.7],
            )),
        };
        for (panel, expected) in [
            (3, vec![0.3, 0.7]),
            (5, vec![0.1, 0.7]),
            (7, vec![0.1, 0.3]),
            (9, vec![0.1, 0.3]),
        ] {
            let mut layout = original.clone();
            assert!(layout.remove(panel));
            assert_eq!(boundaries(&layout), expected);
            assert_eq!(layout.next_id, original.next_id);
            layout.validate().unwrap();
        }
        let mut equal = Layout {
            root: Some(split(
                1,
                Axis::Horizontal,
                vec![tabs(2, "a"), tabs(4, "b"), tabs(6, "c")],
                &[],
            )),
            ..empty()
        };
        equal.next_id = 8;
        assert!(equal.remove(5));
        assert_eq!(boundaries(&equal), &[1.0 / 3.0]);
        equal.validate().unwrap();
    }

    #[test]
    fn boundary_resize_is_absolute_and_rejected_edits_are_exact_noops() {
        let mut layout = Layout {
            next_id: 20,
            locked: false,
            root: Some(split(
                1,
                Axis::Horizontal,
                vec![tabs(2, "a"), tabs(4, "b"), tabs(6, "c"), tabs(8, "d")],
                &[0.1, 0.3, 0.7],
            )),
        };
        assert!(layout.set_boundary(1, 1, 0.25));
        assert_eq!(boundaries(&layout), &[0.1, 0.25, 0.7]);
        assert_eq!(layout.next_id, 20);
        let before = layout.clone();
        for fraction in [
            0.0,
            1.0,
            -0.1,
            1.1,
            0.1,
            0.7,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            assert!(!layout.set_boundary(1, 1, fraction));
            assert_eq!(layout, before);
        }
        for (split, index) in [(1, 3), (1, usize::MAX), (2, 0), (999, 0)] {
            assert!(!layout.set_boundary(split, index, 0.5));
            assert_eq!(layout, before);
        }
        assert!(layout.set_boundary(1, 0, f64::MIN_POSITIVE));
        assert_eq!(boundaries(&layout), &[f64::MIN_POSITIVE, 0.25, 0.7]);
        layout.validate().unwrap();
    }

    #[test]
    fn tabs_reorder_transfer_and_preserve_instance_preferences() {
        let mut layout = empty();
        let a = layout.add("a", None);
        let b = layout.add("audio_visualizer", None);
        let c = layout.add("c", None);
        let options = AudioVisualizerOptions {
            style: AudioVisualizerStyle::Bars,
            grid: true,
            label: false,
        };
        assert!(layout.set_audio_visualizer_options(b, options));
        assert!(layout.set_tab_bar_visible(b, false));
        let source = group(&layout, a);
        assert!(layout.move_panel(b, source, Edge::Center, Some(0)));
        assert_eq!(
            layout
                .panels()
                .iter()
                .map(|panel| panel.id)
                .collect::<Vec<_>>(),
            vec![b, a, c]
        );
        assert_eq!(layout.active_panels()[0].id, b);
        assert!(layout.move_panel(c, source, Edge::Right, None));
        let destination = group(&layout, c);
        assert!(layout.move_panel(b, destination, Edge::Center, Some(999)));
        let Node::Tabs { panels, active, .. } =
            layout.root.as_ref().unwrap().find(destination).unwrap()
        else {
            unreachable!();
        };
        assert_eq!(
            panels.iter().map(|panel| panel.id).collect::<Vec<_>>(),
            vec![c, b]
        );
        assert_eq!(*active, b);
        assert!(!panels[1].show_tab_bar);
        assert_eq!(panels[1].audio_visualizer, Some(options));
        assert!(layout.activate(c));
        layout.validate().unwrap();
    }

    #[test]
    fn lone_self_drops_are_noops_and_invalid_targets_are_unchanged() {
        let mut layout = empty();
        let panel = layout.add("a", None);
        let target = group(&layout, panel);
        let before = layout.clone();
        for edge in [
            Edge::Left,
            Edge::Right,
            Edge::Top,
            Edge::Bottom,
            Edge::Center,
        ] {
            assert!(layout.move_panel(panel, target, edge, None));
            assert_eq!(layout, before);
        }
        assert!(!layout.move_panel(panel, 999, Edge::Right, None));
        assert!(!layout.move_panel(999, target, Edge::Center, None));
        assert!(!layout.remove(999));
        assert_eq!(layout, before);
        let other = layout.add("b", None);
        assert!(layout.move_panel(other, target, Edge::Right, None));
        let split = layout.root.as_ref().unwrap().id();
        let before = layout.clone();
        assert!(!layout.move_panel(panel, split, Edge::Center, None));
        assert_eq!(layout, before);
    }

    #[test]
    fn ancestor_target_survives_removal_and_collapse() {
        for edge in [Edge::Right, Edge::Bottom] {
            let mut layout = empty();
            let a = layout.add("a", None);
            let b = layout.add("b", None);
            assert!(layout.move_panel(b, group(&layout, a), Edge::Right, None));
            let ancestor = layout.root.as_ref().unwrap().id();
            assert!(layout.move_panel(a, ancestor, edge, None));
            assert_eq!(
                layout
                    .panels()
                    .iter()
                    .map(|panel| panel.id)
                    .collect::<Vec<_>>(),
                vec![b, a]
            );
            assert_proportions(&layout, &[1, 1]);
            let Some(Node::Split { axis, .. }) = &layout.root else {
                unreachable!();
            };
            assert_eq!(*axis, edge.axis());
            layout.validate().unwrap();
        }
    }

    #[test]
    fn split_container_drop_preserves_existing_region_proportions() {
        let mut layout = Layout {
            next_id: 30,
            locked: false,
            root: Some(split(
                1,
                Axis::Vertical,
                vec![
                    tabs(2, "source"),
                    split(
                        4,
                        Axis::Horizontal,
                        vec![tabs(5, "a"), tabs(7, "b")],
                        &[0.75],
                    ),
                ],
                &[0.25],
            )),
        };
        assert!(layout.move_panel(3, 4, Edge::Right, None));
        assert_proportions(&layout, &[3, 1, 4]);
        assert_eq!(
            layout
                .panels()
                .iter()
                .map(|panel| panel.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "source"]
        );
        layout.validate().unwrap();
    }

    #[test]
    fn exhausted_ids_roll_back_exactly() {
        let mut layout = empty();
        let source = layout.add("source", None);
        let other = layout.add("other", None);
        let target = group(&layout, other);
        layout.next_id = u64::MAX;
        let before = layout.clone();
        assert!(!layout.move_panel(source, target, Edge::Bottom, None));
        assert_eq!(layout, before);
    }

    #[test]
    fn depth_33_docking_rolls_back_and_same_axis_depth_32_docking_succeeds() {
        let mut layout = nested_layout(32);
        let source_group = 4 + 3 * 31;
        let source = layout.add("audio_visualizer", Some(source_group));
        layout.add("spare", Some(source_group));
        let destination = layout.add("audio_visualizer", Some(1));
        let options = AudioVisualizerOptions {
            style: AudioVisualizerStyle::Rings,
            grid: false,
            label: true,
        };
        assert!(layout.set_audio_visualizer_options(source, options));
        assert!(layout.set_tab_bar_visible(source, false));
        assert!(layout.set_audio_visualizer_options(destination, options));
        assert!(layout.activate(source));
        assert!(layout.activate(2));
        layout.validate().unwrap();
        let before = layout.clone();
        assert!(!layout.move_panel(source, 1, Edge::Bottom, None));
        assert_eq!(layout, before);
        assert!(layout.move_panel(source, 1, Edge::Right, None));
        assert_eq!(split_depth(layout.root.as_ref().unwrap()), 32);
        assert_eq!(layout.audio_visualizer_options(source), Some(options));
        assert!(
            !layout
                .panels()
                .iter()
                .find(|panel| panel.id == source)
                .unwrap()
                .show_tab_bar
        );
        let Node::Split { children, .. } = layout.root.as_ref().unwrap().find(3).unwrap() else {
            unreachable!();
        };
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].id(), 1);
        let Node::Tabs { panels, active, .. } = &children[1] else {
            unreachable!();
        };
        assert_eq!(
            panels.iter().map(|panel| panel.id).collect::<Vec<_>>(),
            vec![source]
        );
        assert_eq!(*active, source);
        assert_eq!(children[2].id(), 4);
        layout.validate().unwrap();
    }

    #[test]
    fn depth_32_persists_and_depth_33_is_rejected_without_replacing_saved_bytes() {
        let directory = TemporaryDirectory::new();
        let path = directory.0.join("layout.json");
        let mut set = LayoutSet::default();
        *set.current_mut() = nested_layout(32);
        set.current().validate().unwrap();
        set.save(&path).unwrap();
        let reopened = LayoutSet::load(&path).unwrap();
        assert_eq!(split_depth(reopened.current().root.as_ref().unwrap()), 32);
        assert_eq!(encoded(&reopened), encoded(&set));
        let previous_bytes = fs::read(&path).unwrap();

        *set.current_mut() = nested_layout(33);
        assert!(set.current().validate().is_err());
        assert!(set.save(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), previous_bytes);
        let invalid_path = directory.0.join("too-deep.json");
        fs::write(&invalid_path, serde_json::to_vec(&encoded(&set)).unwrap()).unwrap();
        assert!(LayoutSet::load(&invalid_path).is_err());
    }

    #[test]
    fn invalid_runtime_boundaries_never_replace_saved_bytes() {
        let directory = TemporaryDirectory::new();
        let path = directory.0.join("layout.json");
        let mut set = LayoutSet::default();
        *set.current_mut() = Layout {
            next_id: 8,
            locked: false,
            root: Some(split(
                1,
                Axis::Horizontal,
                vec![tabs(2, "a"), tabs(4, "b"), tabs(6, "c")],
                &[0.2, 0.7],
            )),
        };
        set.save(&path).unwrap();
        let previous = fs::read(&path).unwrap();
        for invalid in [
            vec![0.5],
            vec![0.0, 0.7],
            vec![0.2, 1.0],
            vec![0.5, 0.5],
            vec![0.8, 0.2],
            vec![f64::NAN, 0.7],
            vec![0.2, f64::INFINITY],
            vec![f64::NEG_INFINITY, 0.7],
        ] {
            let Some(Node::Split { boundaries, .. }) = &mut set.current_mut().root else {
                unreachable!();
            };
            *boundaries = invalid;
            assert!(set.save(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), previous);
        }
        assert_eq!(
            boundaries(LayoutSet::load(&path).unwrap().current()),
            &[0.2, 0.7]
        );
    }

    #[test]
    fn more_than_1024_equal_siblings_persist_without_a_sibling_limit() {
        let count = 1025_u64;
        let layout = Layout {
            root: Some(Node::Split {
                id: 1,
                axis: Axis::Horizontal,
                boundaries: Vec::new(),
                children: (0..count)
                    .map(|index| tabs(2 + 2 * index, "panel"))
                    .collect(),
            }),
            next_id: 2 + 2 * count,
            locked: false,
        };
        layout.validate().unwrap();
        let directory = TemporaryDirectory::new();
        let path = directory.0.join("wide.json");
        let mut set = LayoutSet::default();
        *set.current_mut() = layout;
        set.save(&path).unwrap();
        let reopened = LayoutSet::load(&path).unwrap();
        assert!(boundaries(reopened.current()).is_empty());
        assert_eq!(reopened.current().panels().len(), count as usize);
        assert_eq!(encoded(&reopened), encoded(&set));
    }

    #[test]
    fn equal_boundaries_materialize_only_for_valid_edits() {
        let mut layout = Layout {
            next_id: 20,
            locked: false,
            root: Some(split(
                1,
                Axis::Horizontal,
                vec![tabs(2, "a"), tabs(4, "b"), tabs(6, "c")],
                &[],
            )),
        };
        let before = layout.clone();
        assert!(!layout.set_boundary(1, 0, 2.0 / 3.0));
        assert_eq!(layout, before);
        assert!(layout.set_boundary(1, 0, 0.123456789012345));
        assert_eq!(boundaries(&layout), &[0.123456789012345, 2.0 / 3.0]);
        let mut set = LayoutSet::default();
        *set.current_mut() = layout;
        assert_eq!(encoded(&decoded(encoded(&set)).unwrap()), encoded(&set));
    }

    #[test]
    fn saved_visualizer_stack_retains_its_geometry() {
        for boundaries in [json!([0.66, 0.83]), json!([])] {
            let saved = one_layout(json!({
                "type": "split",
                "axis": "vertical",
                "boundaries": boundaries,
                "children": [
                    wire_tabs("queue"),
                    {"type": "tabs", "panels": [{"kind": "audio_visualizer", "style": "spectrum", "grid": false, "label": false}]},
                    {"type": "tabs", "panels": [{"kind": "audio_visualizer", "style": "spectrogram", "grid": false, "label": false}]}
                ]
            }));
            let reopened = decoded(saved.clone()).unwrap();
            let written = encoded(&reopened);
            let expected = &saved["layouts"]["Work"]["root"];
            let actual = &written["layouts"]["Work"]["root"];
            assert_eq!(actual["children"], expected["children"]);
            assert_eq!(
                actual["boundaries"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                expected["boundaries"].as_array().unwrap()
            );
        }
    }

    #[test]
    fn unrepresentable_flattening_rejects_structural_edits_with_exact_rollback() {
        let upper = f64::from_bits(0.5_f64.to_bits() + 1);
        let mut docking = Layout {
            next_id: 20,
            locked: false,
            root: Some(split(
                1,
                Axis::Horizontal,
                vec![tabs(2, "left"), tabs(4, "target"), tabs(6, "right")],
                &[0.5, upper],
            )),
        };
        let incoming = docking.add("incoming", Some(4));
        let before = docking.clone();
        assert!(!docking.move_panel(incoming, 4, Edge::Right, None));
        assert_eq!(docking, before);
        docking.validate().unwrap();

        let mut collapse = Layout {
            next_id: 20,
            locked: false,
            root: Some(split(
                1,
                Axis::Horizontal,
                vec![
                    tabs(2, "left"),
                    split(
                        4,
                        Axis::Vertical,
                        vec![
                            tabs(5, "remove"),
                            split(7, Axis::Horizontal, vec![tabs(8, "a"), tabs(10, "b")], &[]),
                        ],
                        &[],
                    ),
                    tabs(12, "right"),
                ],
                &[0.5, upper],
            )),
        };
        collapse.validate().unwrap();
        let before = collapse.clone();
        assert!(!collapse.remove(6));
        assert_eq!(collapse, before);
        assert!(!collapse.move_panel(6, 12, Edge::Center, None));
        assert_eq!(collapse, before);
    }

    #[test]
    fn geometry_rounds_cumulative_content_and_covers_the_container() {
        for scale in [1.0, 1.25, 2.0] {
            for count in [2, 3, 7, 1025] {
                let length = 4107.0 / scale;
                let gutter = 2.0 / scale;
                let spans: Vec<_> = split_geometry(&[], count, length, gutter, scale).collect();
                let minimum = spans
                    .iter()
                    .map(|span| span.1)
                    .fold(f64::INFINITY, f64::min);
                let maximum = spans.iter().map(|span| span.1).fold(0.0, f64::max);
                assert!((maximum - minimum) * scale <= 1.0 + 1e-10);
                assert_eq!(spans[0].0, 0.0);
                assert!((spans.last().unwrap().0 + spans.last().unwrap().1 - length).abs() < 1e-10);
                for pair in spans.windows(2) {
                    assert!((pair[1].0 - pair[0].0 - pair[0].1 - gutter).abs() < 1e-10);
                }
            }
        }
        assert_eq!(
            split_geometry(&[0.2, 0.7], 3, 1010.0, 5.0, 1.0).collect::<Vec<_>>(),
            vec![(0.0, 200.0), (205.0, 500.0), (710.0, 300.0)]
        );
        assert_eq!(split_geometry(&[], 0, 100.0, 5.0, 1.0).len(), 0);
    }

    #[test]
    fn measured_reconciliation_preserves_untouched_sibling_pixels_after_local_insertion() {
        let mut layout = Layout {
            next_id: 20,
            locked: false,
            root: Some(split(
                1,
                Axis::Horizontal,
                vec![tabs(2, "a"), tabs(4, "b"), tabs(6, "c")],
                &[0.6, 0.8],
            )),
        };
        let incoming = layout.add("incoming", Some(4));
        let incoming_tabs = layout.next_id;
        assert!(layout.move_panel(incoming, 4, Edge::Right, None));
        let sizes = std::collections::HashMap::from([
            (1, (1010.0, 500.0)),
            (2, (600.0, 500.0)),
            (4, (97.5, 500.0)),
            (incoming_tabs, (97.5, 500.0)),
            (6, (200.0, 500.0)),
        ]);
        layout.preserve_region_sizes(&sizes, 5.0);
        let spans: Vec<_> = split_geometry(boundaries(&layout), 4, 1010.0, 5.0, 2.0).collect();
        assert_eq!(
            spans.iter().map(|span| span.1).collect::<Vec<_>>(),
            vec![600.0, 97.5, 97.5, 200.0]
        );
        layout.validate().unwrap();
    }

    #[test]
    fn loading_does_not_restore_deleted_templates_or_replace_user_layouts() {
        let directory = TemporaryDirectory::new();
        let path = directory.0.join("layout.json");
        let mut set = LayoutSet::default();
        set.current_mut().add("custom", None);
        let removed: Vec<_> = set
            .names()
            .filter(|name| *name != set.active_name())
            .map(str::to_owned)
            .collect();
        for name in &removed {
            set.remove(name).unwrap();
        }
        let expected = encoded(&set);
        set.save(&path).unwrap();
        let loaded = LayoutSet::load(&path).unwrap();
        assert_eq!(encoded(&loaded), expected);
        assert!(
            removed
                .iter()
                .all(|name| !loaded.names().any(|actual| actual == name))
        );
    }

    #[test]
    fn named_layouts_have_distinct_ownership_and_atomic_name_operations() {
        let mut set = single_layout_set();
        let original = set.current().clone();
        set.duplicate("  Work  ").unwrap();
        assert_eq!(set.active_name(), "Work");
        let custom = set.current_mut().add("custom", None);
        set.current_mut().locked = true;
        set.select(" Default ").unwrap();
        assert_eq!(set.current(), &original);
        set.select("Work").unwrap();
        assert!(
            set.current()
                .panels()
                .iter()
                .any(|panel| panel.id == custom)
        );
        set.rename("  Music  ").unwrap();
        set.rename("Music").unwrap();
        assert_eq!(set.names().collect::<Vec<_>>(), vec!["Default", "Music"]);
        let before = encoded(&set);
        for invalid in ["", "  ", "Default"] {
            assert!(set.duplicate(invalid).is_err());
            assert!(set.rename(invalid).is_err());
            assert_eq!(encoded(&set), before);
        }
        assert!(set.select("missing").is_err());
        assert!(set.remove("Music").is_err());
        assert!(set.remove("missing").is_err());
        assert_eq!(encoded(&set), before);
        set.remove(" Default ").unwrap();
        assert_eq!(set.names().collect::<Vec<_>>(), vec!["Music"]);
        assert!(set.remove("Music").is_err());
    }

    #[test]
    fn instance_preferences_roundtrip_flat_and_initialization_keeps_saved_choices() {
        let mut set = LayoutSet::default();
        *set.current_mut() = empty();
        let first = set.current_mut().add("audio_visualizer", None);
        let second = set.current_mut().add("audio_visualizer", None);
        let saved = AudioVisualizerOptions {
            style: AudioVisualizerStyle::Oscilloscope,
            grid: false,
            label: true,
        };
        let defaults = AudioVisualizerOptions {
            style: AudioVisualizerStyle::FractalFlow,
            grid: true,
            label: false,
        };
        assert!(set.current_mut().set_audio_visualizer_options(first, saved));
        assert!(set.current_mut().set_tab_bar_visible(first, false));
        assert!(set.current_mut().initialize_audio_visualizers(defaults));
        assert!(!set.current_mut().initialize_audio_visualizers(saved));
        assert_eq!(set.current().audio_visualizer_options(first), Some(saved));
        assert_eq!(
            set.current().audio_visualizer_options(second),
            Some(defaults)
        );
        set.current_mut().locked = true;
        set.duplicate("Another").unwrap();
        let value = encoded(&set);
        let panels = &value["layouts"]["Default"]["root"]["panels"];
        assert_eq!(panels[0]["style"], "oscilloscope");
        assert_eq!(panels[1]["style"], "fractal_flow");
        assert_eq!(panels[0]["grid"], false);
        assert_eq!(panels[0]["label"], true);
        assert!(panels[0].get("audio_visualizer").is_none());
        let reopened = decoded(value.clone()).unwrap();
        assert_eq!(encoded(&reopened), value);
        reopened.validate().unwrap();
    }

    fn artwork_options(style: crate::config::AlbumArtworkStyle) -> AlbumArtworkOptions {
        AlbumArtworkOptions {
            style,
            ..AlbumArtworkOptions::from(&crate::config::Config::default())
        }
    }

    #[test]
    fn artwork_options_are_independent_and_survive_duplication_and_reload() {
        use crate::config::{AlbumArtworkDirection, AlbumArtworkMode, AlbumArtworkStyle};
        let mut set = LayoutSet::default();
        *set.current_mut() = empty();
        let first = set.current_mut().add("album_artwork", None);
        let second = set.current_mut().add("album_artwork", None);
        let other = set.current_mut().add("library", None);
        let defaults = artwork_options(AlbumArtworkStyle::Plain);
        let custom = AlbumArtworkOptions {
            style: AlbumArtworkStyle::Vinyl,
            mode: AlbumArtworkMode::Rotating,
            direction: AlbumArtworkDirection::Counterclockwise,
            period_seconds: 7.5,
        };
        assert!(set.current_mut().initialize_album_artworks(defaults));
        assert!(set.current_mut().set_album_artwork_options(first, custom));
        assert_eq!(set.current().album_artwork_options(first), Some(custom));
        assert_eq!(set.current().album_artwork_options(second), Some(defaults));
        assert_eq!(set.current().album_artwork_options(other), None);
        assert!(!set.current_mut().set_album_artwork_options(other, custom));
        assert!(
            !set.current_mut()
                .set_album_artwork_options(u64::MAX, custom)
        );
        for period_seconds in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(!set.current_mut().set_album_artwork_options(
                first,
                AlbumArtworkOptions {
                    period_seconds,
                    ..custom
                }
            ));
        }
        assert_eq!(set.current().album_artwork_options(first), Some(custom));
        set.duplicate("Copy").unwrap();
        let copy_first = set.current().panels()[0].id;
        assert!(
            set.current_mut()
                .set_album_artwork_options(copy_first, defaults)
        );
        set.select("Default").unwrap();
        assert_eq!(set.current().album_artwork_options(first), Some(custom));
        let value = encoded(&set);
        assert_eq!(
            value["layouts"]["Default"]["root"]["panels"][0]["album_artwork"]["period_seconds"],
            7.5
        );
        let reopened = decoded(value.clone()).unwrap();
        assert_eq!(encoded(&reopened), value);
    }

    #[test]
    fn existing_artwork_layout_initialization_preserves_explicit_options() {
        use crate::config::AlbumArtworkStyle;
        let saved = artwork_options(AlbumArtworkStyle::Glass);
        let mut set = decoded(one_layout(json!({
            "type":"tabs", "panels":[
                {"kind":"album_artwork", "album_artwork":saved},
                {"kind":"album_artwork"},
                {"kind":"library"}
            ]
        })))
        .unwrap();
        let ids: Vec<_> = set
            .current()
            .panels()
            .iter()
            .map(|panel| panel.id)
            .collect();
        let defaults = artwork_options(AlbumArtworkStyle::Vinyl);
        assert!(set.current_mut().initialize_album_artworks(defaults));
        assert!(
            !set.current_mut()
                .initialize_album_artworks(artwork_options(AlbumArtworkStyle::Plain))
        );
        assert_eq!(set.current().album_artwork_options(ids[0]), Some(saved));
        assert_eq!(set.current().album_artwork_options(ids[1]), Some(defaults));
        assert_eq!(set.current().album_artwork_options(ids[2]), None);
        let value = encoded(&set);
        assert_eq!(encoded(&decoded(value.clone()).unwrap()), value);
    }

    #[test]
    fn artwork_options_survive_docking() {
        let mut layout = empty();
        let source = layout.add("album_artwork", None);
        let target = layout.add("library", None);
        let options = artwork_options(crate::config::AlbumArtworkStyle::Vinyl);
        assert!(layout.set_album_artwork_options(source, options));
        let destination = group(&layout, target);
        assert!(layout.move_panel(source, destination, Edge::Right, None));
        assert_eq!(layout.album_artwork_options(source), Some(options));
    }

    #[test]
    fn artwork_persistence_rejects_incomplete_invalid_and_misplaced_options() {
        let valid =
            serde_json::to_value(artwork_options(crate::config::AlbumArtworkStyle::Plain)).unwrap();
        for field in ["style", "mode", "direction", "period_seconds"] {
            let mut incomplete = valid.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(decoded(one_layout(json!({"type":"tabs", "panels":[{"kind":"album_artwork", "album_artwork":incomplete}]}))).is_err());
        }
        for period in [json!(0), json!(-1), json!(null), json!("20")] {
            let mut invalid = valid.clone();
            invalid["period_seconds"] = period;
            assert!(decoded(one_layout(json!({"type":"tabs", "panels":[{"kind":"album_artwork", "album_artwork":invalid}]}))).is_err());
        }
        for kind in ["library", "audio_visualizer"] {
            assert!(
                decoded(one_layout(
                    json!({"type":"tabs", "panels":[{"kind":kind, "album_artwork":valid}]})
                ))
                .is_err()
            );
        }
        let mut unknown = valid;
        unknown["fps"] = json!(60);
        assert!(
            decoded(one_layout(
                json!({"type":"tabs", "panels":[{"kind":"album_artwork", "album_artwork":unknown}]})
            ))
            .is_err()
        );
    }

    #[test]
    fn strict_format_rejects_unknown_fields_and_malformed_structures() {
        let leaf = wire_tabs("a");
        let split =
            json!({"type":"split", "axis":"horizontal", "children":[leaf.clone(), wire_tabs("b")]});
        for root in [
            json!({"type":"unknown"}),
            json!({"type":"tabs", "panels":[]}),
            json!({"type":"tabs", "panels":[{"kind":"a"}], "active":1}),
            json!({"type":"tabs", "panels":[{"kind":"a"}], "active":-1}),
            json!({"type":"tabs", "panels":[{"kind":"a", "show_tabs":true}]}),
            json!({"type":"tabs", "panels":[{}]}),
            json!({"type":"tabs", "panels":[{"kind":"audio_visualizer", "style":"bars"}]}),
            json!({"type":"tabs", "panels":[{"kind":"a", "style":"bars", "grid":true, "label":true}]}),
            json!({"type":"tabs", "panels":[{"kind":"audio_visualizer", "style":null}]}),
            json!({"type":"tabs", "panels":[{"kind":"album_artwork", "album_artwork":null}]}),
            json!({"type":"split", "axis":"horizontal", "children":[leaf.clone()]}),
            json!({"type":"split", "axis":"Horizontal", "children":[leaf.clone(), leaf.clone()]}),
            json!({"type":"split", "axis":"horizontal", "children":[split, leaf.clone()]}),
        ] {
            assert!(
                decoded(one_layout(root.clone())).is_err(),
                "accepted {root}"
            );
        }
        for bounds in [
            json!([0]),
            json!([1]),
            json!([-0.1]),
            json!([1.1]),
            json!([0.2, 0.3]),
            json!([null]),
            json!(["0.5"]),
            json!(null),
        ] {
            let root = json!({"type":"split", "axis":"horizontal", "children":[leaf.clone(), leaf.clone()], "boundaries":bounds});
            assert!(decoded(one_layout(root)).is_err());
        }
        for bounds in [json!([0.5, 0.5]), json!([0.7, 0.2])] {
            let root = json!({"type":"split", "axis":"horizontal", "children":[leaf.clone(), leaf.clone(), leaf.clone()], "boundaries":bounds});
            assert!(decoded(one_layout(root)).is_err());
        }
        let precise = one_layout(
            json!({"type":"split", "axis":"horizontal", "children":[leaf.clone(), leaf], "boundaries":[0.123456789012345]}),
        );
        assert_eq!(encoded(&decoded(precise.clone()).unwrap()), precise);
        let equal = decoded(one_layout(json!({"type":"split", "axis":"horizontal", "children":[wire_tabs("a"), wire_tabs("b")]}))).unwrap();
        assert!(
            encoded(&equal)["layouts"]["Work"]["root"]
                .get("boundaries")
                .is_none()
        );
        for value in [
            json!({"layouts":{"Work":{"root":null}}}),
            json!({"active":"Work", "layouts":{}}),
            json!({"active":"Missing", "layouts":{"Work":{"root":null}}}),
            json!({"active":"Work", "layouts":{"Work":{}}}),
            json!({"active":"Work", "layouts":{"Work":{"root":null,"lock":true}}}),
            json!({"active":"Work", "layouts":{"Work":{"root":null}}, "selected":"Work"}),
            json!({"active":"Work", "layouts":{"Work":{"root":null}, " Work ":{"root":null}}}),
            json!({"active":" ", "layouts":{" ":{"root":null}}}),
        ] {
            assert!(decoded(value.clone()).is_err(), "accepted {value}");
        }
        assert!(
            serde_json::from_str::<persistence::Input>(
                r#"{"active":"Work","layouts":{"Work":{"root":null},"Work":{"root":null}}}"#
            )
            .is_err()
        );
        let empty = decoded(one_layout(Value::Null)).unwrap();
        assert!(empty.current().root.is_none());
        assert!(decoded(one_layout(wire_tabs("extension-panel-kind"))).is_ok());
    }

    #[test]
    fn atomic_save_failure_preserves_disk_and_valid_named_ui_state() {
        let directory = TemporaryDirectory::new();
        let path = directory.0.join("layout.json");
        let mut set = single_layout_set();
        set.duplicate("Work").unwrap();
        set.current_mut().add("custom", None);
        set.save(&path).unwrap();
        assert_eq!(encoded(&LayoutSet::load(&path).unwrap()), encoded(&set));
        let previous_bytes = fs::read(&path).unwrap();
        let next_id = set.current().next_id;
        set.current_mut().next_id = 1;
        assert!(set.save(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), previous_bytes);
        set.current_mut().next_id = next_id;

        // Rename over a directory fails after the complete temporary write.
        let blocked = directory.0.join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("keep"), b"unchanged").unwrap();
        set.remove("Default").unwrap();
        let intended = encoded(&set);
        assert!(set.save(&blocked).is_err());
        assert_eq!(encoded(&set), intended);
        assert_eq!(fs::read(blocked.join("keep")).unwrap(), b"unchanged");
        assert_eq!(fs::read(&path).unwrap(), previous_bytes);
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 2);
        set.save(&path).unwrap();
        assert_eq!(
            LayoutSet::load(&path).unwrap().names().collect::<Vec<_>>(),
            vec!["Work"]
        );
    }

    #[test]
    fn mixed_transitions_keep_instances_and_normalized_structure() {
        let mut layout = Layout::default();
        let mut expected = layout
            .panels()
            .iter()
            .map(|panel| panel.id)
            .collect::<Vec<_>>();
        let edges = [
            Edge::Center,
            Edge::Left,
            Edge::Top,
            Edge::Right,
            Edge::Bottom,
        ];
        for step in 0..80 {
            let panels = layout
                .panels()
                .iter()
                .map(|panel| panel.id)
                .collect::<Vec<_>>();
            let source = panels[step % panels.len()];
            let target = group(&layout, panels[(step * 7 + 1) % panels.len()]);
            match step % 4 {
                0 => expected.push(layout.add("custom", Some(target))),
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
            layout.validate().unwrap();
            expected.sort_unstable();
            let mut actual = layout
                .panels()
                .iter()
                .map(|panel| panel.id)
                .collect::<Vec<_>>();
            actual.sort_unstable();
            assert_eq!(actual, expected);
        }
    }
}
