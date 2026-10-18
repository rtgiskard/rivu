//! Window-local queue owner and its typed integration boundary.
use super::*;
use crate::gui::{components::search::new_search, feature::FeatureContext, loading::LoadingText};
use crate::projection::GuiSnapshot;
use rivu_ui::{
    list_row,
    search::{SearchEvent, SearchField},
};
use std::{cell::RefCell, rc::Rc};

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
    pub(super) context: FeatureContext,
    pub(super) state: Rc<GuiSnapshot>,
    pub(super) model: QueueSelection,
    pub(super) queue_search: Entity<SearchField>,
    pub(super) loading: LoadingText<()>,
    pub(super) visible: bool,
    pub(super) focused: bool,
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
            loading: LoadingText::default(),
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

    pub(super) fn can_resolve_library_drag(&self, drag: &LibraryDrag) -> bool {
        drag.can_resolve(&self.state, self.library_selection_pending)
    }

    pub(super) fn can_transfer_content(
        &self,
        value: &dyn std::any::Any,
        destination: DragDestination,
    ) -> bool {
        if let Some(drag) = value.downcast_ref::<ContentDrag>() {
            return match drag.source() {
                Some(TrackSource::Tracks(ids)) => !ids.is_empty(),
                Some(TrackSource::Album(_)) => !self.content_resolution,
                None => false,
            };
        }
        value
            .downcast_ref::<PlaylistDrag>()
            .is_some_and(|drag| drag.can_transfer(&self.state, destination))
    }

    pub(super) fn resolve_library_drag(
        &mut self,
        nodes: &[LibraryNode],
        _destination: DragDestination,
        cx: &mut Context<Self>,
    ) {
        if !self.library_selection_pending && !nodes.is_empty() {
            cx.emit(QueueEvent::DropLibrary(RefCell::new(Some(nodes.to_vec()))));
        }
    }

    pub(super) fn drop_content(
        &mut self,
        drag: &ContentDrag,
        destination: DragDestination,
        cx: &mut Context<Self>,
    ) {
        if self.can_transfer_content(drag, destination)
            && let Some(source) = drag.source()
        {
            cx.emit(QueueEvent::DropContent(RefCell::new(Some(source.clone()))));
        }
    }

    pub(super) fn transfer_playlist_drag(
        &mut self,
        drag: &PlaylistDrag,
        destination: DragDestination,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if drag.can_transfer(&self.state, destination) {
            cx.emit(QueueEvent::DropPlaylist(RefCell::new(Some(
                drag.track_ids(),
            ))));
        }
    }

    pub(super) fn panel_track_text(&self, track: &crate::model::LibraryRow) -> (String, String) {
        let detail = format!(
            "{}{}{} · {}{}",
            track.artist,
            if track.artist.is_empty() || track.album.is_empty() {
                ""
            } else {
                " / "
            },
            track.album,
            format_time(track.duration.unwrap_or(0.)),
            if track.missing {
                self.context.i18n.text("gl-file-missing-suffix")
            } else {
                ""
            }
        );
        (track.title.clone(), detail)
    }

    pub(super) fn track_favorite_toggle(
        &self,
        id: impl Into<gpui::ElementId>,
        group: &'static str,
        track_id: i64,
        favorite: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let theme = rivu_ui::theme::palette(cx);
        compact_icon_button(
            theme,
            icon_button(
                id,
                if favorite { "󰓎" } else { "󰓐" },
                self.tr(if favorite {
                    "gl-unfavorite"
                } else {
                    "gl-favorite"
                }),
                cx,
                move |this, _, cx| {
                    this.send(
                        Command::SetFavorite {
                            track_ids: vec![track_id],
                            favorite: !favorite,
                        },
                        cx,
                    )
                },
            ),
        )
        .when(favorite, |view| view.text_color(rgb(theme.accent)))
        .opacity(0.)
        .group_hover(group, |style| style.opacity(1.))
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }

    pub(super) fn paged_loading_row(
        &self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let theme = rivu_ui::theme::palette(cx);
        let error = self.model.rows.error(index);
        list_row(theme, ("loading-row", index), false)
            .child(
                caption(
                    theme,
                    self.tr(if error.is_some() {
                        "shell-loading-failed"
                    } else {
                        "shell-loading"
                    }),
                )
                .when(!self.loading.show(&(), error.is_some()), |text| {
                    text.invisible()
                }),
            )
            .when_some(error.map(str::to_owned), |row, error| {
                row.tooltip(move |_, cx| {
                    cx.new(|_| ButtonTooltip {
                        text: error.clone().into(),
                    })
                    .into()
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.retry_queue_page(cx);
                    cx.notify();
                }))
            })
    }
}
