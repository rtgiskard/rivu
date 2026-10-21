//! Persisted, bounded audio DAG. Positions are presentation only; ports define signal flow.
use super::{DenoiseConfig, EqualizerConfig, NeuralConfig};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_GRAPH_NODES: usize = 64;
pub const MAX_GRAPH_EDGES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoiseSelection {
    pub start_seconds: f64,
    pub end_seconds: f64,
}
impl NoiseSelection {
    pub fn validate(self) -> Result<()> {
        ensure!(
            self.start_seconds.is_finite()
                && self.end_seconds.is_finite()
                && self.start_seconds >= 0.0
                && self.end_seconds > self.start_seconds,
            "Noise profile must be a finite nonempty source-relative range"
        );
        Ok(())
    }
}
pub use rivu_dsp::fir_eq::EqImplementation;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphExecutionMode {
    #[default]
    Auto,
    Offline,
    Realtime,
    Hybrid,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphPort {
    pub node: u64,
    pub port: u16,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphEdge {
    pub from: GraphPort,
    pub to: GraphPort,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphNodeKind {
    Source,
    Equalizer {
        config: EqualizerConfig,
        implementation: EqImplementation,
    },
    Denoise {
        config: DenoiseConfig,
        profile: Option<NoiseSelection>,
    },
    Speech {
        config: NeuralConfig,
    },
    Separate {
        config: NeuralConfig,
    },
    Mix {
        gains_db: Vec<f32>,
        muted: Vec<bool>,
        solo: Vec<bool>,
    },
    Output,
}
impl GraphNodeKind {
    pub fn inputs(&self) -> usize {
        match self {
            Self::Source => 0,
            Self::Mix { gains_db, .. } => gains_db.len(),
            _ => 1,
        }
    }
    pub fn outputs(&self) -> usize {
        if matches!(self, Self::Separate { .. }) {
            2
        } else {
            1
        }
    }
    fn validate(&self) -> Result<()> {
        match self {
            Self::Equalizer {
                config,
                implementation,
            } => {
                config.validate()?;
                implementation.validate()?;
            }
            Self::Denoise { config, profile } => {
                config.validate()?;
                if let Some(selection) = profile {
                    selection.validate()?;
                }
            }
            Self::Speech { config } | Self::Separate { config } => config.validate()?,
            Self::Mix {
                gains_db,
                muted,
                solo,
            } => {
                ensure!(
                    (2..=16).contains(&gains_db.len())
                        && muted.len() == gains_db.len()
                        && solo.len() == gains_db.len(),
                    "Mix requires 2..16 gains with matching mute/solo arrays"
                );
                ensure!(
                    gains_db
                        .iter()
                        .all(|g| g.is_finite() && (-120.0..=24.0).contains(g)),
                    "Mix gains must be finite and between -120 and 24 dB"
                );
            }
            Self::Source | Self::Output => {}
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphNode {
    pub id: u64,
    pub position: [f32; 2],
    pub kind: GraphNodeKind,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessingGraph {
    pub revision: u64,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub next_id: u64,
}
impl Default for ProcessingGraph {
    fn default() -> Self {
        Self {
            revision: 0,
            next_id: 3,
            nodes: vec![
                GraphNode {
                    id: 1,
                    position: [40.0, 120.0],
                    kind: GraphNodeKind::Source,
                },
                GraphNode {
                    id: 2,
                    position: [360.0, 120.0],
                    kind: GraphNodeKind::Output,
                },
            ],
            edges: vec![GraphEdge {
                from: GraphPort { node: 1, port: 0 },
                to: GraphPort { node: 2, port: 0 },
            }],
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum GraphEdit {
    Add {
        kind: GraphNodeKind,
        position: [f32; 2],
    },
    Remove {
        id: u64,
    },
    Replace {
        id: u64,
        kind: GraphNodeKind,
    },
    Move {
        id: u64,
        position: [f32; 2],
    },
    Connect {
        from: GraphPort,
        to: GraphPort,
    },
    Disconnect {
        to: GraphPort,
    },
    Duplicate {
        id: u64,
        position: [f32; 2],
    },
    ReplaceGraph {
        graph: ProcessingGraph,
    },
}
impl ProcessingGraph {
    /// Audio identity ignores layout, optimistic edit revision and allocation history.
    pub fn audio_eq(&self, other: &Self) -> bool {
        self.nodes.len() == other.nodes.len()
            && self.edges.len() == other.edges.len()
            && self.nodes.iter().all(|node| {
                other
                    .nodes
                    .iter()
                    .any(|n| n.id == node.id && n.kind == node.kind)
            })
            && self.edges.iter().all(|edge| other.edges.contains(edge))
    }
    pub fn node(&self, id: u64) -> Result<&GraphNode> {
        self.nodes
            .iter()
            .find(|n| n.id == id)
            .context("Unknown graph node")
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.nodes.is_empty() && self.nodes.len() <= MAX_GRAPH_NODES,
            "Graph requires 1..64 nodes"
        );
        ensure!(
            self.edges.len() <= MAX_GRAPH_EDGES,
            "Graph has more than 128 edges"
        );
        let mut ids = BTreeSet::new();
        for node in &self.nodes {
            ensure!(
                node.id > 0 && node.id < self.next_id && ids.insert(node.id),
                "Invalid or duplicate node id"
            );
            ensure!(
                node.position.iter().all(|p| p.is_finite()),
                "Graph positions must be finite"
            );
            node.kind.validate()?;
        }
        ensure!(
            self.nodes
                .iter()
                .filter(|n| matches!(n.kind, GraphNodeKind::Source))
                .count()
                == 1,
            "Graph requires exactly one source"
        );
        let mut inputs = BTreeSet::new();
        for edge in &self.edges {
            ensure!(
                (edge.from.port as usize) < self.node(edge.from.node)?.kind.outputs(),
                "Invalid output port"
            );
            ensure!(
                (edge.to.port as usize) < self.node(edge.to.node)?.kind.inputs(),
                "Invalid input port"
            );
            ensure!(
                inputs.insert(edge.to),
                "Input already connected; use an explicit mix node"
            );
        }
        self.topological()?;
        Ok(())
    }
    fn topological(&self) -> Result<Vec<u64>> {
        let mut indegree: BTreeMap<u64, usize> = self.nodes.iter().map(|n| (n.id, 0)).collect();
        for edge in &self.edges {
            *indegree
                .get_mut(&edge.to.node)
                .context("Unknown edge node")? += 1;
        }
        let mut ready: BTreeSet<u64> = indegree
            .iter()
            .filter_map(|(&id, &n)| (n == 0).then_some(id))
            .collect();
        let mut ordered = Vec::with_capacity(self.nodes.len());
        while let Some(id) = ready.pop_first() {
            ordered.push(id);
            for edge in self.edges.iter().filter(|e| e.from.node == id) {
                let n = indegree
                    .get_mut(&edge.to.node)
                    .context("Unknown edge node")?;
                *n -= 1;
                if *n == 0 {
                    ready.insert(edge.to.node);
                }
            }
        }
        ensure!(ordered.len() == self.nodes.len(), "Graph contains a cycle");
        Ok(ordered)
    }
    /// Source-first closure only. Disconnected required inputs fail at execution admission.
    pub fn ancestors(&self, target: GraphPort) -> Result<Vec<u64>> {
        self.validate()?;
        ensure!(
            (target.port as usize) < self.node(target.node)?.kind.outputs(),
            "Invalid target output port"
        );
        let mut needed = BTreeSet::from([target.node]);
        let order = self.topological()?;
        for &id in order.iter().rev() {
            if needed.contains(&id) {
                let node = self.node(id)?;
                for port in 0..node.kind.inputs() {
                    let edge = self
                        .edges
                        .iter()
                        .find(|e| {
                            e.to == GraphPort {
                                node: id,
                                port: port as u16,
                            }
                        })
                        .context("Required graph input is disconnected")?;
                    needed.insert(edge.from.node);
                }
            }
        }
        Ok(order.into_iter().filter(|id| needed.contains(id)).collect())
    }
    pub fn edit(&mut self, revision: u64, edit: GraphEdit) -> Result<()> {
        ensure!(
            revision == self.revision,
            "Graph changed; refresh before editing"
        );
        let mut next = self.clone();
        match edit {
            GraphEdit::Add { kind, position } => {
                let id = next.next_id;
                next.next_id = id.checked_add(1).context("Graph node ids exhausted")?;
                next.nodes.push(GraphNode { id, position, kind });
            }
            GraphEdit::Duplicate { id, position } => {
                let kind = next.node(id)?.kind.clone();
                let id = next.next_id;
                next.next_id = id.checked_add(1).context("Graph node ids exhausted")?;
                next.nodes.push(GraphNode { id, position, kind });
            }
            GraphEdit::Remove { id } => {
                next.node(id)?;
                next.nodes.retain(|n| n.id != id);
                next.edges.retain(|e| e.from.node != id && e.to.node != id);
            }
            GraphEdit::Replace { id, kind } => {
                let node = next
                    .nodes
                    .iter_mut()
                    .find(|n| n.id == id)
                    .context("Unknown graph node")?;
                node.kind = kind;
            }
            GraphEdit::Move { id, position } => {
                next.nodes
                    .iter_mut()
                    .find(|n| n.id == id)
                    .context("Unknown graph node")?
                    .position = position;
            }
            GraphEdit::Connect { from, to } => next.edges.push(GraphEdge { from, to }),
            GraphEdit::Disconnect { to } => next.edges.retain(|e| e.to != to),
            GraphEdit::ReplaceGraph { graph } => next = graph,
        }
        next.revision = self
            .revision
            .checked_add(1)
            .context("Graph revision exhausted")?;
        // Undo/replacement must never reuse identities allocated in a previous graph revision.
        next.next_id = next.next_id.max(self.next_id);
        next.validate()?;
        *self = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(node: u64, port: u16) -> GraphPort {
        GraphPort { node, port }
    }
    #[test]
    fn stale_and_invalid_edits_are_atomic() {
        let mut graph = ProcessingGraph::default();
        let before = graph.clone();
        assert!(
            graph
                .edit(
                    1,
                    GraphEdit::Move {
                        id: 1,
                        position: [0.0; 2]
                    }
                )
                .is_err()
        );
        assert!(
            graph
                .edit(
                    0,
                    GraphEdit::Connect {
                        from: p(2, 0),
                        to: p(2, 0)
                    }
                )
                .is_err()
        );
        assert_eq!(graph, before);
    }
    #[test]
    fn disconnected_siblings_do_not_block_preview() {
        let mut graph = ProcessingGraph::default();
        graph
            .edit(
                0,
                GraphEdit::Add {
                    kind: GraphNodeKind::Denoise {
                        config: DenoiseConfig::default(),
                        profile: None,
                    },
                    position: [0.0; 2],
                },
            )
            .unwrap();
        assert_eq!(graph.ancestors(p(2, 0)).unwrap(), vec![1, 2]);
        assert!(graph.ancestors(p(3, 0)).is_err());
    }
    #[test]
    fn removal_disconnects_without_rewiring() {
        let mut graph = ProcessingGraph::default();
        graph.edit(0, GraphEdit::Remove { id: 2 }).unwrap();
        assert!(graph.edges.is_empty());
        assert_eq!(graph.ancestors(p(1, 0)).unwrap(), vec![1]);
    }
    #[test]
    fn layout_and_vector_order_do_not_change_audio_identity() {
        let graph = ProcessingGraph::default();
        let mut moved = graph.clone();
        moved
            .edit(
                0,
                GraphEdit::Move {
                    id: 1,
                    position: [-500.0, 90.0],
                },
            )
            .unwrap();
        moved.nodes.reverse();
        moved.next_id += 100;
        assert!(graph.audio_eq(&moved));
        moved
            .edit(1, GraphEdit::Disconnect { to: p(2, 0) })
            .unwrap();
        assert!(!graph.audio_eq(&moved));
    }

    #[test]
    fn typed_ports_reject_implicit_fanin_cycles_and_unbounded_nodes() {
        let mut graph = ProcessingGraph::default();
        assert!(
            graph
                .edit(
                    0,
                    GraphEdit::Connect {
                        from: p(1, 0),
                        to: p(2, 0)
                    }
                )
                .is_err()
        );
        assert!(
            graph
                .edit(
                    0,
                    GraphEdit::Connect {
                        from: p(1, 1),
                        to: p(2, 0)
                    }
                )
                .is_err()
        );
        graph
            .edit(0, GraphEdit::Disconnect { to: p(2, 0) })
            .unwrap();
        assert!(
            graph
                .edit(
                    1,
                    GraphEdit::Connect {
                        from: p(2, 0),
                        to: p(2, 0)
                    }
                )
                .is_err()
        );
        for _ in 2..MAX_GRAPH_NODES {
            graph
                .edit(
                    graph.revision,
                    GraphEdit::Add {
                        kind: GraphNodeKind::Output,
                        position: [0.0; 2],
                    },
                )
                .unwrap();
        }
        let before = graph.clone();
        assert!(
            graph
                .edit(
                    graph.revision,
                    GraphEdit::Add {
                        kind: GraphNodeKind::Output,
                        position: [0.0; 2]
                    }
                )
                .is_err()
        );
        assert_eq!(graph, before);
    }
}
