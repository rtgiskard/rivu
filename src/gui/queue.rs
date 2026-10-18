//! Queue pages and compact selections use Query-owned stamps. Filtered views also
//! depend on metadata and retain separate matching offsets and occurrence positions.
//! Page demand, logical generations and physical worker gates have separate lifetimes: retiring
//! work never releases its gate, and only authoritative identity lookups delete a
//! selected occurrence. Hidden state sync does not demand metadata pages; failures
//! remain parked until explicit retry. Now-playing locate never enters selection
//! or metadata intent handling.
//! Queue owns search, input, rendering, queries and selection lifetimes; the window
//! coordinator receives typed cross-feature intents for confirmation and metadata.
//! Physical page retirement precedes snapshot reconciliation, then publication.

mod model;
mod queries;
mod view;

use crate::{
    gui::{
        components::search::new_search,
        coordination::manual_actions::ManualIntentId,
        feature::FeatureContext,
        library::LibraryNode,
        list_interaction::{ContentMenuRequest, TrackSource},
    },
    model::Command,
    projection::GuiSnapshot,
};
use gpui::{Context, Entity, SharedString, Window, prelude::*};
use model::QueueSelection;
pub(in crate::gui) use model::{QueueCommit, QueueIdentity};
use rivu_ui::loading::LoadingText;
use rivu_ui::search::{SearchEvent, SearchField};
use std::{cell::RefCell, rc::Rc};
pub(in crate::gui) use view::QueueDrag;

#[derive(Clone, Copy)]
pub(in crate::gui) enum QueueSelectionRequest {
    Row { id: u64, shift: bool, multi: bool },
    Offset { offset: usize, all: bool },
}

pub(in crate::gui) enum QueueEvent {
    SelectionRequested(QueueSelectionRequest),
    SelectionReady {
        intent: ManualIntentId,
        commit: RefCell<Option<QueueCommit>>,
    },
    SelectionFailed(ManualIntentId),
    SelectionCommitted {
        track_id: i64,
    },
    SearchChanged,
    FocusRequested,
    ContentMenu {
        request: RefCell<Option<ContentMenuRequest>>,
        revision: u64,
        remove: RefCell<Option<Command>>,
    },
    DropLibrary(RefCell<Option<Vec<LibraryNode>>>),
    DropContent(RefCell<Option<TrackSource>>),
    DropPlaylist(RefCell<Option<Vec<i64>>>),
    Error(String),
}

pub(in crate::gui) struct Queue {
    context: FeatureContext,
    state: Rc<GuiSnapshot>,
    model: QueueSelection,
    queue_search: Entity<SearchField>,
    loading: LoadingText<()>,
    visible: bool,
    focused: bool,
    library_selection_pending: bool,
    content_resolution: bool,
    _subscription: gpui::Subscription,
}

impl gpui::EventEmitter<QueueEvent> for Queue {}

impl Queue {
    pub(in crate::gui) fn new(context: FeatureContext, cx: &mut Context<Self>) -> Self {
        let state = context.snapshot();
        let queue_search = cx.new(|cx| new_search(&context.i18n, cx));
        let subscription = cx.subscribe(&queue_search, |this, _, event, cx| {
            let SearchEvent::Changed(query) = event;
            this.apply_queue_search(query.clone(), cx);
        });
        let mut model = QueueSelection::default();
        model.sync(state.query_revisions(None), state.queue.total);
        Self {
            context,
            state,
            model,
            queue_search,
            loading: LoadingText::new(std::time::Duration::from_millis(150)),
            visible: false,
            focused: false,
            library_selection_pending: false,
            content_resolution: false,
            _subscription: subscription,
        }
    }

    pub(in crate::gui) fn reconcile(&mut self, state: Rc<GuiSnapshot>, cx: &mut Context<Self>) {
        // Queue query stamps depend on queue identity and, only while filtered,
        // library metadata. Playback clocks do not invalidate rows or demand.
        let projection_changed = self.state.queue.revision != state.queue.revision
            || (self.model.query.is_some()
                && self.state.library.revision != state.library.revision);
        let total_changed = self.state.queue.total != state.queue.total;
        let page_size_changed = self.state.system.config.page_size != state.system.config.page_size;
        let current_changed = self.state.queue.current_id != state.queue.current_id
            || self.state.queue.current_position != state.queue.current_position;
        self.state = state;
        if projection_changed || total_changed {
            self.sync_queue(cx);
            self.retain_loading(cx);
        }
        if (projection_changed || total_changed || page_size_changed) && self.visible {
            self.demand_visible_page(cx);
        }
        if projection_changed || total_changed || page_size_changed || current_changed {
            cx.notify();
        }
    }

    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reconcile(self.context.snapshot(), cx);
    }

    fn retain_loading(&mut self, cx: &mut Context<Self>) {
        let model = &self.model;
        let visible = self.visible;
        self.loading
            .retain(|_, token| visible && model.page_loading(token));
        self.loading.schedule(cx, |this| &mut this.loading);
    }

    pub(in crate::gui) fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        self.retain_loading(cx);
        if visible {
            self.demand_visible_page(cx);
        }
        cx.notify();
    }

    fn demand_visible_page(&mut self, cx: &mut Context<Self>) {
        let range = self.model.rows.visible();
        self.ensure_queue_range(if range.is_empty() { 0..1 } else { range }, cx);
    }

    /// Retire logical window demand without releasing either physical worker gate.
    pub(in crate::gui) fn cancel_window_reads(&mut self, cx: &mut Context<Self>) {
        self.model.cancel_page_demand();
        self.retain_loading(cx);
    }

    /// The shell owns global focus; this is presentation state only.
    pub(in crate::gui) fn set_focused(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.focused != focused {
            self.focused = focused;
            cx.notify();
        }
    }

    pub(in crate::gui) fn set_transfer_state(
        &mut self,
        library_selection_pending: bool,
        content_resolution: bool,
        cx: &mut Context<Self>,
    ) {
        if (self.library_selection_pending, self.content_resolution)
            != (library_selection_pending, content_resolution)
        {
            self.library_selection_pending = library_selection_pending;
            self.content_resolution = content_resolution;
            cx.notify();
        }
    }

    pub(in crate::gui) fn input_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        self.queue_search.read(cx).input_focused(window, cx)
    }

    pub(in crate::gui) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.queue_search
            .update(cx, |search, cx| search.focus(window, cx));
    }

    pub(in crate::gui) fn escape_search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.queue_search
            .update(cx, |search, cx| search.escape(window, cx))
    }

    pub(in crate::gui) fn cancel_selection(&mut self, cx: &mut Context<Self>) {
        self.model.cancel_manual();
        cx.notify();
    }

    pub(in crate::gui) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.model.clear();
        cx.notify();
    }

    pub(in crate::gui) fn active(&self) -> Option<QueueIdentity> {
        self.model.active()
    }

    pub(in crate::gui) fn prepare_selection(
        &mut self,
        request: QueueSelectionRequest,
        intent: ManualIntentId,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        let revisions = self.state.query_revisions(None);
        match request {
            QueueSelectionRequest::Row { id, shift, multi } => {
                if self.model.busy && (shift || multi) {
                    cx.emit(QueueEvent::SelectionFailed(intent));
                    return;
                }
                let Some(row) = self
                    .model
                    .rows
                    .iter()
                    .find(|row| row.id == id)
                    .map(QueueIdentity::from)
                else {
                    cx.emit(QueueEvent::SelectionFailed(intent));
                    return;
                };
                if let Some(commit) = self
                    .model
                    .begin_selection(intent, revisions, row, shift, multi)
                {
                    cx.emit(QueueEvent::SelectionReady {
                        intent,
                        commit: RefCell::new(Some(commit)),
                    });
                } else {
                    self.dispatch_queue_selection(cx);
                }
            }
            QueueSelectionRequest::Offset { offset, all } => {
                if offset >= self.model.rows.len() {
                    cx.emit(QueueEvent::SelectionFailed(intent));
                    return;
                }
                self.model.begin_navigation(intent, revisions, offset, all);
                self.dispatch_queue_selection(cx);
            }
        }
        cx.notify();
    }

    pub(super) fn tr(&self, id: &str) -> SharedString {
        self.context.tr(id)
    }

    pub(super) fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(error) = self.context.handle().send(command) {
            cx.emit(QueueEvent::Error(format!("{error:#}")));
        }
        self.refresh(cx);
    }
}
