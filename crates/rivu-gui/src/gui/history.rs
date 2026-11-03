//! Window-lifetime history owner shared by every history panel.

mod model;
mod panel;

use super::{
    components::search::{SearchScope, new_search},
    coordination::manual_actions::ManualIntentId,
    feature::FeatureContext,
    list_interaction::ContentMenuRequest,
};
use crate::projection::GuiSnapshot;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, Font, SharedString, Subscription, Window,
};
use model::HistoryBrowser;
pub(super) use model::HistoryCommit;
use panel::HistoryColumnWidths;
use rivu_protocol::model::{Command, PlaybackStatus};
use rivu_ui::search::{SearchEvent, SearchField};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum HistorySort {
    Recent,
    Title,
    Plays,
}

pub(super) enum HistoryEvent {
    SelectionRequested {
        id: i64,
        shift: bool,
        additive: bool,
    },
    SelectionReady {
        intent: ManualIntentId,
        commit: RefCell<Option<HistoryCommit>>,
        structure_revision: u64,
    },
    SelectionCommitted {
        track_id: i64,
    },
    SearchChanged,
    InvalidateManualIntent,
    FocusRequested {
        panel_id: u64,
    },
    ContentMenu(RefCell<Option<ContentMenuRequest>>),
    Error(String),
}

pub(super) struct History {
    context: FeatureContext,
    state: Rc<GuiSnapshot>,
    model: HistoryBrowser,
    search: Entity<SearchField>,
    ui_font: Font,
    column_widths: Option<HistoryColumnWidths>,
    _search_subscription: Subscription,
}

impl EventEmitter<HistoryEvent> for History {}

impl History {
    pub(super) fn new(context: FeatureContext, ui_font: Font, cx: &mut Context<Self>) -> Self {
        let state = context.snapshot();
        let search = cx.new(|cx| new_search(&context.i18n, SearchScope::Recent, cx));
        let subscription = cx.subscribe(&search, |this, _, event, cx| {
            let SearchEvent::Changed(query) = event;
            this.model.set_query(query.clone());
            this.context
                .scroll
                .borrow_mut()
                .scroll_list_to("history", 0);
            cx.emit(HistoryEvent::SearchChanged);
            cx.notify();
        });
        let mut model = HistoryBrowser::default();
        model.sync(&state.library.history, Self::playback(&state));
        Self {
            context,
            state,
            model,
            search,
            ui_font,
            column_widths: None,
            _search_subscription: subscription,
        }
    }

    fn current_playback(state: &GuiSnapshot) -> Option<(i64, PlaybackStatus)> {
        state
            .current_track()
            .map(|track| (track.id, state.playback.status))
    }

    fn playback(state: &GuiSnapshot) -> Option<(i64, i64)> {
        (state.playback.status != PlaybackStatus::Stopped)
            .then(|| {
                state
                    .current_track()
                    .map(|track| track.id)
                    .zip(state.playback.last_heard_at)
            })
            .flatten()
    }

    pub(super) fn reconcile(&mut self, state: Rc<GuiSnapshot>, cx: &mut Context<Self>) {
        let changed = self
            .model
            .sync(&state.library.history, Self::playback(&state));
        // Status/loaded-track changes affect labels independently of visible order.
        let labels_changed = Self::current_playback(&self.state) != Self::current_playback(&state);
        let typography_changed = self.state.system.config.ui_font_size_pt
            != state.system.config.ui_font_size_pt
            || self.state.system.config.ui_scale != state.system.config.ui_scale;
        if typography_changed
            || !std::sync::Arc::ptr_eq(&self.state.library.history, &state.library.history)
        {
            self.column_widths = None;
        }
        self.state = state;
        if changed || labels_changed || typography_changed {
            cx.notify();
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reconcile(self.context.snapshot(), cx);
    }

    pub(super) fn set_ui_font(&mut self, font: Font, cx: &mut Context<Self>) {
        if self.ui_font != font {
            self.ui_font = font;
            self.column_widths = None;
            cx.notify();
        }
    }

    pub(super) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.search.read(cx).input_focused(window, cx)
    }

    pub(super) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.focus(window, cx));
    }

    pub(super) fn escape_search(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.search
            .update(cx, |search, cx| search.escape(window, cx))
    }

    pub(super) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.model.clear_selection();
        cx.notify();
    }

    pub(super) fn select_all(&mut self, cx: &mut Context<Self>) {
        self.model.select_all();
        cx.emit(HistoryEvent::InvalidateManualIntent);
        cx.notify();
    }

    fn request_selection(&self, id: i64, shift: bool, additive: bool, cx: &mut Context<Self>) {
        cx.emit(HistoryEvent::SelectionRequested {
            id,
            shift,
            additive,
        });
    }

    pub(super) fn prepare_selection(
        &mut self,
        id: i64,
        shift: bool,
        additive: bool,
        intent: ManualIntentId,
        cx: &mut Context<Self>,
    ) -> bool {
        self.refresh(cx);
        let Some(commit) = self.model.prepare_selection(id, shift, additive) else {
            return false;
        };
        cx.emit(HistoryEvent::SelectionReady {
            intent,
            commit: RefCell::new(Some(commit)),
            structure_revision: self.state.library.structure_revision,
        });
        true
    }

    // There is no in-flight history selection worker: the coordinator owns
    // cancellation of synchronous prepared commits through ManualIntentId.
    pub(super) fn commit_valid(&self, commit: &HistoryCommit, structure_revision: u64) -> bool {
        structure_revision == self.state.library.structure_revision
            && self.model.commit_valid(commit)
    }

    pub(super) fn commit_selection(
        &mut self,
        commit: HistoryCommit,
        cx: &mut Context<Self>,
    ) -> bool {
        let track_id = commit.track_id;
        if !self.model.commit_selection(commit) {
            return false;
        }
        cx.emit(HistoryEvent::SelectionCommitted { track_id });
        cx.notify();
        true
    }

    pub(super) fn favorite_actions(&self, ids: &[i64]) -> (bool, bool) {
        self.model.favorite_actions(ids)
    }

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(error) = self.context.handle().send(command) {
            cx.emit(HistoryEvent::Error(format!("{error:#}")));
        }
        cx.notify();
    }

    fn tr(&self, id: &str) -> SharedString {
        self.context.tr(id)
    }
}
