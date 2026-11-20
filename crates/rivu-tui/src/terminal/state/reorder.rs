use super::{
    Command, ContainerIdentity, KeyAction, KeyCode, KeyEvent, NoticeKind, PagedList, TuiSnapshot,
    UiState, View,
};
use crate::terminal::paged::LookupStep;
use rivu_presentation::keyboard::Action;

#[derive(Clone, Copy)]
enum Target {
    Queue(u64),
    Playlist(i64),
    Entry { playlist_id: i64, entry_id: i64 },
}

impl Target {
    fn revision(self, state: &TuiSnapshot) -> Option<u64> {
        match self {
            Self::Queue(_) => Some(state.queue.revision),
            Self::Playlist(_) => Some(state.library.playlist_revision),
            Self::Entry { playlist_id, .. } => state
                .library
                .playlist_entry_revisions
                .get(&playlist_id)
                .copied(),
        }
    }
}

pub(super) struct Reorder {
    target: Target,
    revision: u64,
    phase: Phase,
}

enum Phase {
    Preview,
    Failed(String),
    Saving,
    SavingAfterChange,
    Saved,
    Changed,
}

fn begin<T, I>(page: &mut PagedList<T>, id: impl FnOnce(&T) -> I) -> Option<I> {
    let id = id(page.selected()?);
    page.begin_reorder();
    Some(id)
}

impl UiState {
    pub(super) fn begin_reorder(&mut self, state: &TuiSnapshot) {
        if self.search.active {
            self.set_notice(
                self.i18n.text("terminal-reorder-filtered").into(),
                NoticeKind::Warning,
            );
            return;
        }
        let target = match &mut self.view {
            View::Queue if self.queue.anchor.is_none() => {
                begin(&mut self.queue, |row| row.id).map(Target::Queue)
            }
            View::Playlists(rows) if rows.identity_lookup.is_none() => {
                begin(rows, |row| row.id).map(Target::Playlist)
            }
            View::PlaylistDetail { detail, .. }
                if detail.entry_anchor.is_none()
                    && state
                        .library
                        .playlist_entry_revisions
                        .contains_key(&detail.playlist_id) =>
            {
                begin(&mut detail.entries, |row| row.id).map(|entry_id| Target::Entry {
                    playlist_id: detail.playlist_id,
                    entry_id,
                })
            }
            _ => None,
        };
        if let Some(target) = target {
            self.reorder = Some(Reorder {
                target,
                revision: target.revision(state).expect("loaded reorder target"),
                phase: Phase::Preview,
            });
            self.startup_hint_deadline = None;
            self.mark_local_change();
        }
    }

    fn end_preview(&mut self, target: Target, lookup: Option<LookupStep>) {
        match (&mut self.view, target) {
            (View::Queue, Target::Queue(id)) => {
                self.queue.end_reorder();
                self.queue.selected_id = Some(id);
                if lookup.is_some() {
                    self.queue.anchor = Some(id);
                }
            }
            (View::Playlists(rows), Target::Playlist(id)) => {
                rows.end_reorder();
                if let Some(step) = lookup {
                    rows.identity_lookup = Some((
                        ContainerIdentity::Playlist(id),
                        step,
                        rows.selection.offset(),
                    ));
                }
            }
            (View::PlaylistDetail { detail, .. }, Target::Entry { entry_id, .. }) => {
                detail.entries.end_reorder();
                detail.selected_entry_id = Some(entry_id);
                if lookup.is_some() {
                    detail.entry_anchor = Some(entry_id);
                }
            }
            _ => unreachable!("reorder owns the active list"),
        }
    }

    pub(super) fn sync_reorder(&mut self, next: &TuiSnapshot) {
        let Some(reorder) = &self.reorder else { return };
        if matches!(reorder.phase, Phase::Changed | Phase::SavingAfterChange)
            || reorder.target.revision(next) == Some(reorder.revision)
        {
            return;
        }
        let mut reorder = self.reorder.take().expect("active reorder");
        // The watch may precede the Ack. The destination chooses only the
        // first page to check; selection still requires a fresh matching ID.
        let lookup = match (&self.view, &reorder.phase) {
            (View::Playlists(rows), Phase::Saving | Phase::Saved) => rows
                .reorder_indices()
                .map_or(LookupStep::Scan(0), |(_, to)| LookupStep::Hint(to)),
            _ => LookupStep::Scan(0),
        };
        self.end_preview(reorder.target, Some(lookup));
        reorder.phase = match reorder.phase {
            Phase::Saving => Phase::SavingAfterChange,
            Phase::Saved => {
                self.mark_local_change();
                return;
            }
            _ => Phase::Changed,
        };
        // Keep input modal until dismissal: an Enter already in flight must
        // not turn a discarded move into a playback command.
        self.reorder = Some(reorder);
        self.mark_local_change();
    }

    pub(super) fn reorder_key(&mut self, key: KeyEvent) -> KeyAction {
        let Some(reorder) = &self.reorder else {
            return KeyAction::Ignored;
        };
        if !key.modifiers.is_empty() {
            return KeyAction::Ignored;
        }
        match reorder.phase {
            Phase::Saving | Phase::SavingAfterChange | Phase::Saved => return KeyAction::Ignored,
            Phase::Changed => {
                if key.code == KeyCode::Esc {
                    self.reorder = None;
                    self.mark_local_change();
                }
                return KeyAction::Ignored;
            }
            _ => {}
        }
        let target = reorder.target;
        match key.code {
            KeyCode::Esc => {
                self.reorder = None;
                self.end_preview(target, None);
                self.mark_local_change();
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k') => {
                let action = match key.code {
                    KeyCode::Up | KeyCode::Char('k') => Action::Up,
                    _ => Action::Down,
                };
                let changed = match &mut self.view {
                    View::Queue => self.queue.navigate_action(action, 1),
                    View::Playlists(rows) => rows.navigate_action(action, 1),
                    View::PlaylistDetail { detail, .. } => {
                        detail.entries.navigate_action(action, 1)
                    }
                    _ => unreachable!("reorder owns the active list"),
                };
                if changed {
                    self.reorder.as_mut().expect("active reorder").phase = Phase::Preview;
                    self.mark_local_change();
                }
            }
            KeyCode::Enter => return self.save_reorder(),
            _ => {}
        }
        KeyAction::Ignored
    }

    fn save_reorder(&mut self) -> KeyAction {
        let reorder = self.reorder.as_ref().expect("active reorder");
        let expected_revision = reorder.revision;
        let Some((from, to)) = (match &self.view {
            View::Queue => self.queue.reorder_indices(),
            View::Playlists(rows) => rows.reorder_indices(),
            View::PlaylistDetail { detail, .. } => detail.entries.reorder_indices(),
            _ => None,
        }) else {
            return KeyAction::Ignored;
        };
        let target = reorder.target;
        if from == to {
            self.reorder = None;
            self.end_preview(target, None);
            self.mark_local_change();
            return KeyAction::Ignored;
        }
        let after = from < to;
        let command = match (&self.view, target) {
            (View::Queue, Target::Queue(queue_id)) => Some(Command::MoveQueue {
                queue_id,
                index: to,
                expected_revision,
            }),
            (View::Playlists(rows), Target::Playlist(playlist_id)) => {
                rows.get(to).map(|anchor| Command::MovePlaylist {
                    playlist_id,
                    before_playlist_id: Some(anchor.id),
                    after,
                    expected_revision,
                })
            }
            (
                View::PlaylistDetail { detail, .. },
                Target::Entry {
                    playlist_id,
                    entry_id,
                },
            ) => detail
                .entries
                .get(to)
                .map(|anchor| Command::MovePlaylistEntries {
                    playlist_id,
                    entry_ids: vec![entry_id],
                    before_entry_id: Some(anchor.id),
                    after,
                    expected_revision,
                }),
            _ => unreachable!("reorder owns the active list"),
        };
        command.map_or_else(
            || {
                self.set_notice(
                    self.i18n.text("terminal-reorder-loading").into(),
                    NoticeKind::Warning,
                );
                KeyAction::Ignored
            },
            KeyAction::SaveOrder,
        )
    }

    pub(in crate::terminal) fn reorder_submitted(&mut self) {
        self.reorder.as_mut().expect("active reorder").phase = Phase::Saving;
        self.mark_local_change();
    }

    pub(in crate::terminal) fn reorder_busy(&mut self, message: String) {
        self.reorder.as_mut().expect("active reorder").phase = Phase::Failed(message);
        self.mark_local_change();
    }

    pub(in crate::terminal) fn complete_reorder(&mut self, result: Result<(), String>) {
        let reorder = self.reorder.as_mut().expect("submitted reorder");
        reorder.phase = match (&reorder.phase, result) {
            (Phase::Saving, Ok(())) => Phase::Saved,
            (Phase::Saving, Err(message)) => Phase::Failed(message),
            (Phase::SavingAfterChange, Ok(())) => {
                self.reorder = None;
                self.mark_local_change();
                return;
            }
            (Phase::SavingAfterChange, Err(_)) => Phase::Changed,
            _ => unreachable!("completion belongs to the only submitted reorder"),
        };
        self.mark_local_change();
    }

    pub(in crate::terminal) fn reorder_footer(&self) -> Option<(&str, Option<(&str, NoticeKind)>)> {
        let (key, notice) = match &self.reorder.as_ref()?.phase {
            Phase::Preview => (
                "terminal-reorder",
                self.message
                    .as_ref()
                    .map(|notice| (notice.text.as_str(), notice.kind)),
            ),
            Phase::Failed(error) => (
                "terminal-reorder-failed",
                Some((error.as_str(), NoticeKind::Error)),
            ),
            Phase::Saving | Phase::SavingAfterChange | Phase::Saved => {
                ("terminal-reorder-saving", None)
            }
            Phase::Changed => ("terminal-reorder-changed", None),
        };
        Some((self.i18n.text(key), notice))
    }
}

#[cfg(test)]
mod tests {
    use super::{super::tests::queue_row, *};
    use crate::terminal::state::TuiState;
    use crossterm::event::KeyModifiers;
    use rivu_presentation::i18n::Localizer;
    use std::sync::Arc;

    fn preview() -> TuiState {
        let mut snapshot = TuiSnapshot::default();
        snapshot.queue.total = 3;
        snapshot.queue.revision = 23;
        let mut tui = TuiState::new(snapshot, Arc::new(Localizer::for_terminal("en-US")));
        tui.ui.queue.set_viewport(3);
        tui.ui.queue.insert(
            0,
            3,
            3,
            (0..3).map(|i| queue_row(i, 100 + i as u64)).collect(),
        );
        input(&mut tui, KeyCode::Char('M'));
        input(&mut tui, KeyCode::Char('j'));
        tui
    }

    fn input(tui: &mut TuiState, key: KeyCode) -> KeyAction {
        tui.ui.key_action(KeyEvent::from(key), &tui.snapshot)
    }

    fn assert_save(tui: &mut TuiState) {
        assert!(matches!(
            input(tui, KeyCode::Enter),
            KeyAction::SaveOrder(Command::MoveQueue {
                queue_id: 100,
                index: 1,
                expected_revision: 23,
            })
        ));
        tui.ui.reorder_submitted();
    }

    #[test]
    fn queue_preview_tracks_occurrence_and_ignores_unrelated_refresh() {
        let mut tui = preview();
        let mut next = tui.snapshot.clone();
        next.playback.position = 1.0;
        next.library.playlist_revision = 9;
        tui.apply_snapshot(next);
        assert_eq!(tui.ui.queue.display(0).unwrap().id, 101);
        assert_eq!(tui.ui.queue.display(1).unwrap().id, 100);
        assert_eq!(tui.ui.queue.selected().unwrap().id, 100);
        assert!(matches!(
            input(&mut tui, KeyCode::Delete),
            KeyAction::Ignored
        ));
        input(&mut tui, KeyCode::Esc);
        assert_eq!(tui.ui.queue.display(0).unwrap().id, 100);
        assert!(matches!(
            input(&mut tui, KeyCode::Enter),
            KeyAction::Command(Command::PlayQueue { queue_id: 100 })
        ));
    }

    #[test]
    fn conflicting_insert_cannot_turn_enter_into_playback_or_a_second_save() {
        for submitted in [false, true] {
            let mut tui = preview();
            if submitted {
                assert_save(&mut tui);
            }
            let mut next = tui.snapshot.clone();
            next.queue.total += 1;
            next.queue.revision += 1;
            tui.apply_snapshot(next);
            if submitted {
                tui.ui.complete_reorder(Err("stale".into()));
            }
            assert!(matches!(
                input(&mut tui, KeyCode::Enter),
                KeyAction::Ignored
            ));
            assert!(tui.ui.queue.reorder_indices().is_none());
            assert_eq!(tui.ui.queue.anchor, Some(100));
            input(&mut tui, KeyCode::Esc);
            assert!(tui.ui.reorder_footer().is_none());
        }
    }

    #[test]
    fn failed_save_can_retry_and_success_orders_are_observed_in_either_sequence() {
        for ack_first in [true, false] {
            let mut tui = preview();
            assert_save(&mut tui);
            tui.ui.complete_reorder(Err("not saved".into()));
            assert_eq!(tui.ui.queue.display(1).unwrap().id, 100);
            assert_eq!(tui.ui.reorder_footer().unwrap().1.unwrap().0, "not saved");
            assert_save(&mut tui);
            if ack_first {
                tui.ui.complete_reorder(Ok(()));
                assert_eq!(tui.ui.queue.display(1).unwrap().id, 100);
            }
            let mut next = tui.snapshot.clone();
            next.queue.revision += 1;
            tui.apply_snapshot(next);
            if !ack_first {
                tui.ui.complete_reorder(Ok(()));
            }
            assert!(tui.ui.reorder_footer().is_none());
            assert_eq!(tui.ui.queue.anchor, Some(100));
        }
    }

    #[test]
    fn catalog_save_checks_destination_page_then_falls_back_by_id() {
        use crate::terminal::{state::tests::response, worker::QueryResult};
        use rivu_protocol::{
            model::{Page, PlaylistSummary},
            response::ViewResponse,
        };

        let summary = |id: i64| PlaylistSummary {
            id,
            name: id.to_string(),
            entry_count: 0,
        };
        for ack_first in [true, false] {
            for first_page_hit in [true, false] {
                let snapshot = TuiSnapshot::default();
                let limit = snapshot.configuration.query_batch_size as usize;
                let mut tui = TuiState::new(snapshot, Arc::new(Localizer::for_terminal("en-US")));
                let mut catalog = PagedList::default();
                catalog.selection.select(Some(limit));
                catalog.set_viewport(4);
                let total = 2 * limit + 2;
                catalog.insert(limit, limit, total, vec![summary(71), summary(72)]);
                tui.ui.view = View::Playlists(catalog);
                input(&mut tui, KeyCode::Char('M'));
                input(&mut tui, KeyCode::Down);
                assert!(matches!(
                    input(&mut tui, KeyCode::Enter),
                    KeyAction::SaveOrder(Command::MovePlaylist {
                        playlist_id: 71,
                        ..
                    })
                ));
                tui.ui.reorder_submitted();
                if ack_first {
                    tui.ui.complete_reorder(Ok(()));
                }
                let mut next = tui.snapshot.clone();
                // A coalesced watch update need not be the move's exact revision.
                next.library.playlist_revision += 2;
                tui.apply_snapshot(next);
                let request = tui.ui.query_request(&tui.snapshot).unwrap();
                assert_eq!(request.demand.offset, limit);
                let (page_total, rows) = if first_page_hit {
                    (total, vec![summary(72), summary(73), summary(71)])
                } else {
                    // An out-of-range hint is a miss, not proof of deletion.
                    (3, Vec::new())
                };
                assert!(tui.ui.apply_query(
                    &tui.snapshot,
                    &request,
                    Ok(QueryResult {
                        view: ViewResponse::PlaylistSummaries(Page {
                            total: page_total,
                            rows
                        }),
                        ..response(&request, total)
                    })
                ));
                if !ack_first {
                    tui.ui.complete_reorder(Ok(()));
                }
                if !first_page_hit {
                    let request = tui.ui.query_request(&tui.snapshot).unwrap();
                    assert_eq!(request.demand.offset, 0);
                    assert!(tui.ui.apply_query(
                        &tui.snapshot,
                        &request,
                        Ok(QueryResult {
                            view: ViewResponse::PlaylistSummaries(Page {
                                total: 3,
                                rows: vec![summary(70), summary(71), summary(72)]
                            }),
                            ..response(&request, 3)
                        })
                    ));
                }
                let View::Playlists(catalog) = &tui.ui.view else {
                    panic!("playlist catalog")
                };
                assert_eq!(catalog.selected().unwrap().id, 71);
                assert_eq!(
                    catalog.selection.selected(),
                    Some(if first_page_hit { limit + 2 } else { 1 })
                );
            }
        }
    }

    #[test]
    fn global_ctrl_c_still_owns_preview_input() {
        let mut tui = preview();
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            tui.ui.key_action(key, &tui.snapshot),
            KeyAction::Quit
        ));
    }
}
