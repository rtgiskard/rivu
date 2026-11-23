use std::{collections::HashMap, hash::Hash};

pub(super) fn ordered_by_position<T>(
    items: impl Iterator<Item = T>,
    position: impl FnMut(&T) -> usize,
) -> Vec<T> {
    let mut ordered: Vec<_> = items.collect();
    ordered.sort_unstable_by_key(position);
    ordered
}

pub(super) struct ReconciledSelection<K, Row> {
    pub(super) active: Option<Row>,
    pub(super) anchor: Option<Row>,
    pub(super) selected: HashMap<K, Row>,
}

/// Restore occurrence identities from validated rows, preserving the anchor independently
/// of selected membership.
pub(super) fn reconcile<K: Copy + Eq + Hash, Row: Copy>(
    rows: Vec<Row>,
    active: Option<Row>,
    anchor: Option<Row>,
    selected: &HashMap<K, Row>,
    occurrence_id: impl Fn(&Row) -> K,
) -> ReconciledSelection<K, Row> {
    let active = active.as_ref().map(&occurrence_id);
    let anchor = anchor.as_ref().map(&occurrence_id);
    let mut restored = ReconciledSelection {
        active: None,
        anchor: None,
        selected: HashMap::new(),
    };
    for row in rows {
        let id = occurrence_id(&row);
        if restored.active.is_none() && active == Some(id) {
            restored.active = Some(row);
        }
        if restored.anchor.is_none() && anchor == Some(id) {
            restored.anchor = Some(row);
        }
        if selected.contains_key(&id) {
            restored.selected.insert(id, row);
        }
    }
    restored
}
