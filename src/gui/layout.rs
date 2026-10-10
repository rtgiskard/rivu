//! Normalized docking trees and their named desktop collection.
//!
//! IDs are runtime-only. Persistence borrows the collection, writes one strict
//! format, and atomically replaces the caller-selected file.

use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::config::{AudioVisualizerOptions, AudioVisualizerStyle};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Panel {
    pub id: u64,
    pub kind: String,
    pub show_tab_bar: bool,
    pub audio_visualizer: Option<AudioVisualizerOptions>,
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
pub struct Child {
    pub weight: f32,
    pub node: Node,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Split {
        id: u64,
        axis: Axis,
        children: Vec<Child>,
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

#[derive(Debug, PartialEq)]
pub struct LayoutSet {
    active: String,
    layouts: BTreeMap<String, Layout>,
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

    struct LayoutsRef<'a>(&'a BTreeMap<String, Layout>);
    struct NodeRef<'a>(&'a Node);
    struct ChildrenRef<'a>(&'a [Child]);
    struct PanelsRef<'a>(&'a [Panel]);

    #[derive(Serialize)]
    struct LayoutView<'a> {
        root: Option<NodeRef<'a>>,
        #[serde(skip_serializing_if = "is_false")]
        locked: bool,
    }

    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "lowercase")]
    enum NodeView<'a> {
        Split {
            axis: Axis,
            children: ChildrenRef<'a>,
        },
        Tabs {
            panels: PanelsRef<'a>,
            #[serde(skip_serializing_if = "is_zero")]
            active: usize,
        },
    }

    #[derive(Serialize)]
    struct ChildView<'a> {
        #[serde(skip_serializing_if = "is_one")]
        weight: f32,
        node: NodeRef<'a>,
    }

    #[derive(Serialize)]
    struct PanelView<'a> {
        kind: &'a str,
        #[serde(skip_serializing_if = "is_true")]
        show_tab_bar: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        style: Option<AudioVisualizerStyle>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reference_lines: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        labels: Option<bool>,
    }

    fn is_false(value: &bool) -> bool {
        !*value
    }
    fn is_true(value: &bool) -> bool {
        *value
    }
    fn is_zero(value: &usize) -> bool {
        *value == 0
    }
    fn is_one(value: &f32) -> bool {
        *value == 1.0
    }
    fn default_true() -> bool {
        true
    }
    fn default_weight() -> f32 {
        1.0
    }

    impl Serialize for LayoutsRef<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut map = serializer.serialize_map(Some(self.0.len()))?;
            for (name, layout) in self.0 {
                map.serialize_entry(
                    name,
                    &LayoutView {
                        root: layout.root.as_ref().map(NodeRef),
                        locked: layout.locked,
                    },
                )?;
            }
            map.end()
        }
    }

    impl Serialize for NodeRef<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let view = match self.0 {
                Node::Split { axis, children, .. } => NodeView::Split {
                    axis: *axis,
                    children: ChildrenRef(children),
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
                sequence.serialize_element(&ChildView {
                    weight: child.weight,
                    node: NodeRef(&child.node),
                })?;
            }
            sequence.end()
        }
    }

    impl Serialize for PanelsRef<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
            for panel in self.0 {
                sequence.serialize_element(&PanelView {
                    kind: &panel.kind,
                    show_tab_bar: panel.show_tab_bar,
                    style: panel.audio_visualizer.map(|options| options.style),
                    reference_lines: panel
                        .audio_visualizer
                        .map(|options| options.reference_lines),
                    labels: panel.audio_visualizer.map(|options| options.labels),
                })?;
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
        #[serde(default)]
        locked: bool,
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
            children: Vec<ChildInput>,
        },
        Tabs {
            panels: Vec<PanelInput>,
            #[serde(default)]
            active: usize,
        },
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ChildInput {
        #[serde(default = "default_weight")]
        weight: f32,
        node: NodeInput,
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
        reference_lines: Option<bool>,
        #[serde(default, deserialize_with = "present")]
        labels: Option<bool>,
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
        fn into_node(self, next_id: &mut u64) -> Result<Node> {
            let id = allocate(next_id)?;
            Ok(match self {
                Self::Split { axis, children } => Node::Split {
                    id,
                    axis,
                    children: children
                        .into_iter()
                        .map(|child| {
                            Ok(Child {
                                weight: child.weight,
                                node: child.node.into_node(next_id)?,
                            })
                        })
                        .collect::<Result<_>>()?,
                },
                Self::Tabs { panels, active } => {
                    let panels = panels.into_iter().map(|panel| {
                        let audio_visualizer = match (panel.style, panel.reference_lines, panel.labels) {
                            (None, None, None) => None,
                            (Some(style), Some(reference_lines), Some(labels)) if panel.kind == "audio_visualizer" =>
                                Some(AudioVisualizerOptions { style, reference_lines, labels }),
                            _ => return Err(anyhow!("visualizer options must be complete and belong to an audio_visualizer")),
                        };
                        Ok(Panel { id: allocate(next_id)?, kind: panel.kind,
                            show_tab_bar: panel.show_tab_bar, audio_visualizer })
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
                    .map(|root| root.into_node(&mut next_id))
                    .transpose()?;
                let layout = Layout {
                    root,
                    next_id,
                    locked: input.locked,
                };
                layout.validate().map_err(|error| anyhow!(error))?;
                if layouts.insert(name, layout).is_some() {
                    return Err(anyhow!("duplicate trimmed layout name"));
                }
            }
            let active = checked_name(&self.active)?.to_owned();
            let set = LayoutSet { active, layouts };
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

fn weight_sum(weights: impl IntoIterator<Item = f32>) -> Result<f64> {
    let mut sum = 0.0_f64;
    for weight in weights {
        if !weight.is_finite() || weight <= 0.0 {
            return Err(anyhow!("split weights must be finite and positive"));
        }
        sum += f64::from(weight);
    }
    if sum <= 0.0 || sum > f64::from(f32::MAX) {
        return Err(anyhow!("split weight sum is not representable"));
    }
    Ok(sum)
}

fn scaled_weight(outer: f32, inner: f32, sum: f64) -> Result<f32> {
    let weight = (f64::from(outer) * f64::from(inner) / sum) as f32;
    if !weight.is_finite() || weight <= 0.0 {
        return Err(anyhow!("normalized weight is not representable"));
    }
    Ok(weight)
}

// Raw edits keep every original container until preflight passes. This borrowed
// projection checks the exact post-collapse/flatten weights without allocating
// a second tree, so even floating-point limits can roll back losslessly.
fn effective_node(node: &Node) -> Option<&Node> {
    match node {
        Node::Tabs { panels, .. } => (!panels.is_empty()).then_some(node),
        Node::Split { children, .. } => {
            let mut survivor = None;
            for child in children {
                if let Some(effective) = effective_node(&child.node) {
                    if survivor.is_some() {
                        return Some(node);
                    }
                    survivor = Some(effective);
                }
            }
            survivor
        }
    }
}

fn visit_normalized_weights(node: &Node, visitor: &mut dyn FnMut(f32) -> Result<()>) -> Result<()> {
    let Node::Split { axis, children, .. } = node else {
        unreachable!("effective split");
    };
    for child in children {
        let Some(effective) = effective_node(&child.node) else {
            continue;
        };
        if matches!(effective, Node::Split { axis: child_axis, .. } if child_axis == axis) {
            let sum = normalized_weight_sum(effective)?;
            visit_normalized_weights(effective, &mut |inner| {
                visitor(scaled_weight(child.weight, inner, sum)?)
            })?;
        } else {
            visitor(child.weight)?;
        }
    }
    Ok(())
}

fn normalized_weight_sum(node: &Node) -> Result<f64> {
    let mut sum = 0.0;
    visit_normalized_weights(node, &mut |weight| {
        if !weight.is_finite() || weight <= 0.0 {
            return Err(anyhow!("split weights must be finite and positive"));
        }
        sum += f64::from(weight);
        Ok(())
    })?;
    if sum <= 0.0 || sum > f64::from(f32::MAX) {
        return Err(anyhow!("normalized weight sum is not representable"));
    }
    Ok(sum)
}

fn check_normalization(node: &Node) -> Result<()> {
    // Normalized subtrees need one visit. Only same-axis flattening needs to
    // inspect projected weights again to preserve the exact f32 rounding.
    fn summarize(node: &Node) -> Result<Option<(&Node, f64)>> {
        let Node::Split { axis, children, .. } = node else {
            return Ok(match node {
                Node::Tabs { panels, .. } if panels.is_empty() => None,
                _ => Some((node, 0.0)),
            });
        };
        let mut first: Option<((&Node, f64), f32)> = None;
        let mut multiple = false;
        let mut sum = 0.0;
        let mut add = |effective: &Node, inner_sum: f64, outer: f32| -> Result<()> {
            if !outer.is_finite() || outer <= 0.0 {
                return Err(anyhow!("split weights must be finite and positive"));
            }
            if matches!(effective, Node::Split { axis: child_axis, .. } if child_axis == axis) {
                visit_normalized_weights(effective, &mut |inner| {
                    sum += f64::from(scaled_weight(outer, inner, inner_sum)?);
                    Ok(())
                })?;
            } else {
                sum += f64::from(outer);
            }
            Ok(())
        };
        for child in children {
            let Some(summary) = summarize(&child.node)? else {
                continue;
            };
            if let Some((previous, weight)) = first {
                if !multiple {
                    add(previous.0, previous.1, weight)?;
                    multiple = true;
                }
                add(summary.0, summary.1, child.weight)?;
            } else {
                first = Some((summary, child.weight));
            }
        }
        if !multiple {
            return Ok(first.map(|(summary, _)| summary));
        }
        if sum <= 0.0 || sum > f64::from(f32::MAX) {
            return Err(anyhow!("normalized weight sum is not representable"));
        }
        Ok(Some((node, sum)))
    }
    summarize(node).map(|_| ())
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
            Self::Split { children, .. } => {
                children.iter().find_map(|child| child.node.find(target))
            }
            Self::Tabs { .. } => None,
        }
    }

    fn find_mut(&mut self, target: u64) -> Option<&mut Self> {
        if self.id() == target {
            return Some(self);
        }
        match self {
            Self::Split { children, .. } => children
                .iter_mut()
                .find_map(|child| child.node.find_mut(target)),
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
                .find_map(|child| child.node.panel_group(panel_id)),
        }
    }

    fn first_tabs(&self) -> u64 {
        match self {
            Self::Tabs { id, .. } => *id,
            Self::Split { children, .. } => children[0].node.first_tabs(),
        }
    }

    fn collect_panels<'a>(&'a self, output: &mut Vec<&'a Panel>, active_only: bool) {
        match self {
            Self::Tabs { panels, active, .. } => output.extend(
                panels
                    .iter()
                    .filter(|panel| !active_only || panel.id == *active),
            ),
            Self::Split { children, .. } => {
                for child in children {
                    child.node.collect_panels(output, active_only);
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
                child.node =
                    std::mem::replace(&mut child.node, placeholder).map_node(target, update);
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
            } => {
                children.retain_mut(|child| {
                    let placeholder = Self::Tabs {
                        id: 0,
                        panels: Vec::new(),
                        active: 0,
                    };
                    let node = std::mem::replace(&mut child.node, placeholder);
                    if let Some(node) = node.normalize() {
                        child.node = node;
                        true
                    } else {
                        false
                    }
                });
                if children.is_empty() {
                    return None;
                }
                if children.len() == 1 {
                    return Some(children.pop().expect("one child").node);
                }
                let flattened_len = children
                    .iter()
                    .map(|child| match &child.node {
                        Self::Split {
                            axis: child_axis,
                            children,
                            ..
                        } if *child_axis == axis => children.len(),
                        _ => 1,
                    })
                    .sum();
                if flattened_len == children.len() {
                    return Some(Self::Split { id, axis, children });
                }
                let mut flattened = Vec::with_capacity(flattened_len);
                for child in children {
                    match child.node {
                        Self::Split {
                            axis: child_axis,
                            children: inner,
                            ..
                        } if child_axis == axis => {
                            let sum = weight_sum(inner.iter().map(|child| child.weight))
                                .expect("preflight weights");
                            for inner in inner {
                                flattened.push(Child {
                                    weight: scaled_weight(child.weight, inner.weight, sum)
                                        .expect("preflight scaling"),
                                    node: inner.node,
                                });
                            }
                        }
                        node => flattened.push(Child {
                            weight: child.weight,
                            node,
                        }),
                    }
                }
                Some(Self::Split {
                    id,
                    axis,
                    children: flattened,
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
            children: vec![
                Child {
                    weight: 1.0,
                    node: first,
                },
                Child {
                    weight: 1.0,
                    node: second,
                },
            ],
        }
    }
}

impl Default for Layout {
    fn default() -> Self {
        fn tabs(id: u64, panel_id: u64, kind: &str) -> Node {
            Node::Tabs {
                id,
                panels: vec![Panel {
                    id: panel_id,
                    kind: kind.into(),
                    show_tab_bar: true,
                    audio_visualizer: None,
                }],
                active: panel_id,
            }
        }
        Self {
            root: Some(Node::Split {
                id: 1,
                axis: Axis::Vertical,
                children: vec![
                    Child {
                        weight: 1.0,
                        node: tabs(2, 3, "transport"),
                    },
                    Child {
                        weight: 3.0,
                        node: Node::Split {
                            id: 4,
                            axis: Axis::Horizontal,
                            children: vec![
                                Child {
                                    weight: 0.65,
                                    node: tabs(5, 6, "library"),
                                },
                                Child {
                                    weight: 0.35,
                                    node: Node::Split {
                                        id: 7,
                                        axis: Axis::Vertical,
                                        children: vec![
                                            Child {
                                                weight: 0.45,
                                                node: tabs(8, 9, "queue"),
                                            },
                                            Child {
                                                weight: 0.275,
                                                node: tabs(10, 11, "spectrum"),
                                            },
                                            Child {
                                                weight: 0.275,
                                                node: tabs(12, 13, "spectrogram"),
                                            },
                                        ],
                                    },
                                },
                            ],
                        },
                    },
                ],
            }),
            next_id: 14,
            locked: false,
        }
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
        if check_normalization(self.root.as_ref().expect("source root")).is_err() {
            self.insert_panel(group, panel, Some(index), active);
            return false;
        }
        self.root = self.root.take().and_then(Node::normalize);
        true
    }

    /// Center indices refer to the destination after source removal and clamp
    /// to its end. Split targets accept edge drops. A lone self-drop is a no-op.
    /// All invalid edits preserve both the tree and allocator exactly.
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
        let destination_active = match target {
            Node::Tabs { active, .. } => *active,
            _ => 0,
        };
        let (panel, old_index, old_active) = self.take_panel(group, panel_id);
        let tabs_id = self.next_id;
        let split_id = tabs_id.saturating_add(1);
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
        if check_normalization(self.root.as_ref().expect("destination root")).is_err() {
            let panel = if edge == Edge::Center {
                let (panel, _, _) = self.take_panel(target_node, panel_id);
                if let Some(Node::Tabs { active, .. }) = self
                    .root
                    .as_mut()
                    .and_then(|root| root.find_mut(target_node))
                {
                    *active = destination_active;
                }
                panel
            } else {
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
                            let Node::Tabs { mut panels, .. } = children.remove(incoming).node
                            else {
                                unreachable!("incoming tabs");
                            };
                            recovered = panels.pop();
                            children.pop().expect("original target").node
                        }),
                    )
                });
                recovered.expect("incoming panel")
            };
            self.insert_panel(group, panel, Some(old_index), old_active);
            return false;
        }
        self.root = self.root.take().and_then(Node::normalize);
        self.next_id = next_id;
        true
    }

    /// Resize only the adjacent pair. The fraction is measured within that
    /// pair, and neither the other weights nor the allocator change.
    pub fn set_boundary(&mut self, split_id: u64, index: usize, fraction: f32) -> bool {
        if !fraction.is_finite() || fraction <= 0.0 || fraction >= 1.0 {
            return false;
        }
        let Some(Node::Split { children, .. }) =
            self.root.as_mut().and_then(|root| root.find_mut(split_id))
        else {
            return false;
        };
        let Some(right) = index.checked_add(1).filter(|right| *right < children.len()) else {
            return false;
        };
        let sum = f64::from(children[index].weight) + f64::from(children[right].weight);
        let first = (sum * f64::from(fraction)) as f32;
        let second = (sum - f64::from(first)) as f32;
        if !first.is_finite() || !second.is_finite() || first <= 0.0 || second <= 0.0 {
            return false;
        }
        let total: f64 = children
            .iter()
            .enumerate()
            .map(|(i, child)| {
                f64::from(if i == index {
                    first
                } else if i == right {
                    second
                } else {
                    child.weight
                })
            })
            .sum();
        if total > f64::from(f32::MAX) {
            return false;
        }
        children[index].weight = first;
        children[right].weight = second;
        true
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
                        changed |= initialize(&mut child.node, defaults);
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
            next_id: u64,
            ids: &mut HashSet<u64>,
        ) -> std::result::Result<(), String> {
            record(node.id(), next_id, ids)?;
            match node {
                Node::Split { axis, children, .. } => {
                    if children.len() < 2 {
                        return Err("split must have at least two children".into());
                    }
                    weight_sum(children.iter().map(|child| child.weight))
                        .map_err(|error| error.to_string())?;
                    for child in children {
                        if matches!(&child.node, Node::Split { axis: child_axis, .. } if child_axis == axis)
                        {
                            return Err("same-axis split children must be flattened".into());
                        }
                        visit(&child.node, next_id, ids)?;
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
            visit(root, self.next_id, &mut HashSet::new())?;
        }
        Ok(())
    }
}

impl Default for LayoutSet {
    fn default() -> Self {
        Self {
            active: "Default".into(),
            layouts: BTreeMap::from([("Default".into(), Layout::default())]),
        }
    }
}

impl LayoutSet {
    pub fn current(&self) -> &Layout {
        self.layouts
            .get(&self.active)
            .expect("active layout exists")
    }
    pub fn current_mut(&mut self) -> &mut Layout {
        self.layouts
            .get_mut(&self.active)
            .expect("active layout exists")
    }
    pub fn active_name(&self) -> &str {
        &self.active
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.layouts.keys().map(String::as_str)
    }

    fn unused_name<'a>(&self, name: &'a str) -> Result<&'a str> {
        let name = checked_name(name)?;
        if self.layouts.contains_key(name) {
            return Err(anyhow!("layout {name:?} already exists"));
        }
        Ok(name)
    }

    pub fn select(&mut self, name: &str) -> Result<()> {
        let name = checked_name(name)?;
        if !self.layouts.contains_key(name) {
            return Err(anyhow!("layout {name:?} does not exist"));
        }
        if self.active != name {
            self.active = name.into();
        }
        Ok(())
    }

    pub fn duplicate(&mut self, name: &str) -> Result<()> {
        let name = self.unused_name(name)?.to_owned();
        let copy = self.current().clone();
        self.layouts.insert(name.clone(), copy);
        self.active = name;
        Ok(())
    }

    pub fn rename(&mut self, name: &str) -> Result<()> {
        let name = checked_name(name)?;
        if name == self.active {
            return Ok(());
        }
        let name = self.unused_name(name)?.to_owned();
        let layout = self
            .layouts
            .remove(&self.active)
            .expect("active layout exists");
        self.layouts.insert(name.clone(), layout);
        self.active = name;
        Ok(())
    }

    fn removable_name<'a>(&self, name: &'a str) -> Result<&'a str> {
        let name = checked_name(name)?;
        if name == self.active {
            return Err(anyhow!("cannot remove the current layout"));
        }
        if !self.layouts.contains_key(name) {
            return Err(anyhow!("layout {name:?} does not exist"));
        }
        Ok(name)
    }

    pub fn remove(&mut self, name: &str) -> Result<()> {
        let name = self.removable_name(name)?;
        self.layouts.remove(name);
        Ok(())
    }

    /// Delete and persist as one operation, restoring the owned entry on save
    /// failure. GUI deletion cannot use a compensating public rename/select.
    pub fn remove_saved(&mut self, name: &str, path: &Path) -> Result<()> {
        let name = self.removable_name(name)?.to_owned();
        let removed = self.layouts.remove(&name).expect("checked layout");
        if let Err(error) = self.save(path) {
            self.layouts.insert(name, removed);
            return Err(error);
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.layouts.is_empty() {
            return Err(anyhow!("layout collection is empty"));
        }
        if !self.layouts.contains_key(&self.active) {
            return Err(anyhow!("active layout does not exist"));
        }
        for (name, layout) in &self.layouts {
            if checked_name(name)? != name {
                return Err(anyhow!("layout name is not trimmed"));
            }
            layout.validate().map_err(|error| anyhow!(error))?;
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("opening layout {}", path.display()))?;
        let input: persistence::Input = serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("decoding layout {}", path.display()))?;
        input
            .into_set()
            .with_context(|| format!("invalid layout {}", path.display()))
    }

    /// Validate, write a unique sibling temporary file, then atomically replace
    /// the destination. Failed writes never replace the previous collection.
    pub fn save(&self, path: &Path) -> Result<()> {
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

    fn empty() -> Layout {
        Layout {
            root: None,
            next_id: 1,
            locked: false,
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
            }],
            active: id + 1,
        }
    }

    fn child(weight: f32, node: Node) -> Child {
        Child { weight, node }
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

    fn weights(layout: &Layout) -> Vec<f32> {
        let Some(Node::Split { children, .. }) = &layout.root else {
            panic!("expected split");
        };
        children.iter().map(|child| child.weight).collect()
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
    fn same_axis_docking_adds_siblings_without_resizing_other_regions() {
        let mut layout = empty();
        let first = layout.add("first", None);
        let second = layout.add("second", None);
        assert!(layout.move_panel(second, group(&layout, first), Edge::Right, None));
        let split = layout.root.as_ref().unwrap().id();
        assert!(layout.set_boundary(split, 0, 0.75));
        let third = layout.add("third", Some(group(&layout, second)));
        assert!(layout.move_panel(third, group(&layout, second), Edge::Left, None));
        assert_eq!(weights(&layout), vec![1.5, 0.25, 0.25]);
        let fourth = layout.add("fourth", Some(group(&layout, third)));
        assert!(layout.move_panel(fourth, group(&layout, third), Edge::Top, None));
        assert_eq!(weights(&layout), vec![1.5, 0.25, 0.25]);
        let Some(Node::Split { children, .. }) = &layout.root else {
            unreachable!();
        };
        assert!(matches!(
            children[1].node,
            Node::Split {
                axis: Axis::Vertical,
                ..
            }
        ));
        layout.validate().unwrap();
    }

    #[test]
    fn removal_flattens_exposed_same_axis_children_proportionally() {
        let mut layout = Layout {
            next_id: 100,
            locked: false,
            root: Some(Node::Split {
                id: 1,
                axis: Axis::Horizontal,
                children: vec![
                    child(
                        6.0,
                        Node::Split {
                            id: 2,
                            axis: Axis::Vertical,
                            children: vec![
                                child(1.0, tabs(3, "remove")),
                                child(
                                    1.0,
                                    Node::Split {
                                        id: 5,
                                        axis: Axis::Horizontal,
                                        children: vec![
                                            child(2.0, tabs(6, "left")),
                                            child(1.0, tabs(8, "middle")),
                                        ],
                                    },
                                ),
                            ],
                        },
                    ),
                    child(4.0, tabs(10, "right")),
                ],
            }),
        };
        layout.validate().unwrap();
        assert!(layout.remove(4));
        assert_eq!(weights(&layout), vec![4.0, 2.0, 4.0]);
        assert_eq!(layout.root.as_ref().unwrap().id(), 1);
        layout.validate().unwrap();
        assert!(layout.remove(7));
        assert!(layout.remove(9));
        assert_eq!(layout.root.as_ref().unwrap().id(), 10);
        assert!(layout.remove(11));
        assert!(layout.root.is_none());
        let replacement = layout.add("replacement", None);
        assert_eq!(layout.active_panels()[0].id, replacement);
        layout.validate().unwrap();
    }

    #[test]
    fn boundary_resize_only_changes_its_adjacent_pair() {
        let mut layout = Layout {
            next_id: 20,
            locked: false,
            root: Some(Node::Split {
                id: 1,
                axis: Axis::Horizontal,
                children: vec![
                    child(2.0, tabs(2, "a")),
                    child(3.0, tabs(4, "b")),
                    child(5.0, tabs(6, "c")),
                    child(7.0, tabs(8, "d")),
                ],
            }),
        };
        assert!(layout.set_boundary(1, 1, 0.25));
        assert_eq!(weights(&layout), vec![2.0, 2.0, 6.0, 7.0]);
        let before = layout.clone();
        for fraction in [0.0, 1.0, -0.1, 1.1, f32::NAN, f32::INFINITY] {
            assert!(!layout.set_boundary(1, 1, fraction));
            assert_eq!(layout, before);
        }
        for (split, index) in [(1, 3), (1, usize::MAX), (2, 0), (999, 0)] {
            assert!(!layout.set_boundary(split, index, 0.5));
            assert_eq!(layout, before);
        }
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
            reference_lines: true,
            labels: false,
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
            assert_eq!(weights(&layout), vec![1.0, 1.0]);
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
            root: Some(Node::Split {
                id: 1,
                axis: Axis::Vertical,
                children: vec![
                    child(1.0, tabs(2, "source")),
                    child(
                        3.0,
                        Node::Split {
                            id: 4,
                            axis: Axis::Horizontal,
                            children: vec![child(3.0, tabs(5, "a")), child(1.0, tabs(7, "b"))],
                        },
                    ),
                ],
            }),
        };
        assert!(layout.move_panel(3, 4, Edge::Right, None));
        assert_eq!(weights(&layout), vec![0.75, 0.25, 1.0]);
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
    fn unrepresentable_edits_and_exhausted_ids_roll_back_exactly() {
        let tiny = f32::from_bits(1);
        let mut layout = Layout {
            next_id: 20,
            locked: false,
            root: Some(Node::Split {
                id: 1,
                axis: Axis::Horizontal,
                children: vec![
                    child(tiny, tabs(2, "target")),
                    child(tiny, tabs(4, "other")),
                    child(1.0, tabs(6, "source")),
                ],
            }),
        };
        let before = layout.clone();
        assert!(!layout.move_panel(7, 2, Edge::Right, None));
        assert_eq!(layout, before);
        assert!(!layout.set_boundary(1, 0, tiny));
        assert_eq!(layout, before);
        layout.next_id = u64::MAX;
        let before = layout.clone();
        assert!(!layout.move_panel(7, 2, Edge::Bottom, None));
        assert_eq!(layout, before);

        let mut layout = Layout {
            next_id: 30,
            locked: false,
            root: Some(Node::Split {
                id: 1,
                axis: Axis::Horizontal,
                children: vec![
                    child(
                        tiny,
                        Node::Split {
                            id: 2,
                            axis: Axis::Vertical,
                            children: vec![
                                child(1.0, tabs(3, "remove")),
                                child(
                                    1.0,
                                    Node::Split {
                                        id: 5,
                                        axis: Axis::Horizontal,
                                        children: vec![
                                            child(1.0, tabs(6, "a")),
                                            child(1.0, tabs(8, "b")),
                                        ],
                                    },
                                ),
                            ],
                        },
                    ),
                    child(1.0, tabs(10, "other")),
                ],
            }),
        };
        layout.validate().unwrap();
        let before = layout.clone();
        assert!(!layout.remove(4));
        assert_eq!(layout, before);
    }

    #[test]
    fn named_layouts_have_distinct_ownership_and_atomic_name_operations() {
        let mut set = LayoutSet::default();
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
            style: AudioVisualizerStyle::Rings,
            reference_lines: false,
            labels: true,
        };
        let defaults = AudioVisualizerOptions {
            style: AudioVisualizerStyle::Bars,
            reference_lines: true,
            labels: false,
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
        assert_eq!(panels[0]["style"], "rings");
        assert_eq!(panels[0]["reference_lines"], false);
        assert_eq!(panels[0]["labels"], true);
        assert!(panels[0].get("audio_visualizer").is_none());
        let reopened = decoded(value.clone()).unwrap();
        assert_eq!(encoded(&reopened), value);
        reopened.validate().unwrap();
    }

    #[test]
    fn strict_format_rejects_unknown_fields_and_malformed_structures() {
        let leaf = wire_tabs("a");
        let split = json!({"type":"split", "axis":"horizontal", "children":[
            {"node":leaf.clone()}, {"node":wire_tabs("b")}
        ]});
        for root in [
            json!({"type":"unknown"}),
            json!({"type":"tabs", "panels":[]}),
            json!({"type":"tabs", "panels":[{"kind":"a"}], "active":1}),
            json!({"type":"tabs", "panels":[{"kind":"a"}], "active":-1}),
            json!({"type":"tabs", "panels":[{"kind":"a", "show_tabs":true}]}),
            json!({"type":"tabs", "panels":[{}]}),
            json!({"type":"tabs", "panels":[{"kind":"audio_visualizer", "style":"bars"}]}),
            json!({"type":"tabs", "panels":[{"kind":"a", "style":"bars", "reference_lines":true, "labels":true}]}),
            json!({"type":"tabs", "panels":[{"kind":"audio_visualizer", "style":null}]}),
            json!({"type":"split", "axis":"horizontal", "children":[{"node":leaf.clone()}]}),
            json!({"type":"split", "axis":"Horizontal", "children":[{"node":leaf.clone()}, {"node":leaf.clone()}]}),
            json!({"type":"split", "axis":"horizontal", "children":[{"node":split}, {"node":leaf.clone()}]}),
            json!({"type":"split", "axis":"vertical", "children":[{"weigth":2, "node":leaf.clone()}, {"node":leaf.clone()}]}),
        ] {
            assert!(
                decoded(one_layout(root.clone())).is_err(),
                "accepted {root}"
            );
        }
        for weight in [
            json!(0),
            json!(-1),
            json!(null),
            json!("1"),
            json!(f64::MAX),
        ] {
            let root = json!({"type":"split", "axis":"horizontal", "children":[
                {"weight":weight, "node":leaf.clone()}, {"node":leaf.clone()}
            ]});
            assert!(decoded(one_layout(root)).is_err());
        }
        let overflowing = json!({"type":"split", "axis":"horizontal", "children":[
            {"weight":f32::MAX, "node":leaf.clone()}, {"weight":f32::MAX, "node":leaf}
        ]});
        assert!(decoded(one_layout(overflowing)).is_err());
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
    fn atomic_save_and_failed_deletion_preserve_previous_collection() {
        let directory = TemporaryDirectory::new();
        let path = directory.0.join("layout.json");
        let mut set = LayoutSet::default();
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
        let before = encoded(&set);
        assert!(set.remove_saved("Default", &blocked).is_err());
        assert_eq!(encoded(&set), before);
        assert_eq!(fs::read(blocked.join("keep")).unwrap(), b"unchanged");
        assert_eq!(fs::read(&path).unwrap(), previous_bytes);
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 2);
        set.remove_saved("Default", &path).unwrap();
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
