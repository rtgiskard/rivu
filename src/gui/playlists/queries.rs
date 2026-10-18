//! Playlist transport is owned by the feature and retains physical gate lifetimes.
use super::*;
use crate::gui::{chooser_path, chooser_result};
use ashpd::desktop::file_chooser::SelectedFiles;
use gpui::Stateful;
use requests::ViewRequest;
use std::time::Instant;
impl Playlists {
    pub(in crate::gui) fn apply_playlist_search(
        &mut self,
        query: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.browser.playlists().summaries().query == query {
            return;
        }
        self.cancel_selection(cx);
        cx.emit(PlaylistEvent::SearchChanged);
        if matches!(self.playlist_action, Some(PlaylistAction::Menu(..))) {
            self.playlist_action = None;
        }
        self.browser.search_playlists(query);
        self.playlist_entries_search
            .update(cx, |search, cx| search.reset(cx));
        if self.playlist_view_demand(ViewRequestKind::Summaries) {
            self.request_playlist_summaries(0, cx);
        }
        cx.notify();
    }

    pub(in crate::gui) fn apply_playlist_entries_search(
        &mut self,
        query: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.browser.playlists().entries().query == query {
            return;
        }
        self.cancel_selection(cx);
        cx.emit(PlaylistEvent::SearchChanged);
        self.browser.search_playlist_entries(query);
        if let Some(id) = self.browser.playlists().selected() {
            self.sync_entry_projection(cx);
            if self.panel_demand("playlists") {
                self.request_playlist_entries(id, 0, cx);
            }
        }
        cx.notify();
    }
    pub(in crate::gui) fn start_loading_text(
        &mut self,
        owner: ViewRequestKind,
        token: u64,
        cx: &mut Context<Self>,
    ) {
        self.loading_text.begin(owner, token, Instant::now());
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
    }

    pub(in crate::gui) fn finish_loading_text(
        &mut self,
        owner: &ViewRequestKind,
        token: u64,
        cx: &mut Context<Self>,
    ) {
        self.loading_text.finish(owner, token);
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
    }
    pub(in crate::gui) fn view_page_size(&self) -> usize {
        self.state.system.config.page_size as usize
    }

    fn request_view(&mut self, prepared: ViewRequest, cx: &mut Context<Self>) {
        let kind = prepared.kind();
        if kind != ViewRequestKind::Selection
            && self.panel_demand("playlists")
            && let Some(generation) = prepared.pending_generation()
        {
            self.start_loading_text(kind, generation, cx);
        }
        let Some((read, ticket)) = prepared.into_read() else {
            return;
        };
        let generation = read.generation;
        let query = read.query.clone();
        let handle = self.context.handle().clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(read.delay).await;
            if !this
                .update(cx, |this, cx| {
                    this.refresh(cx);
                    let revisions = this
                        .state
                        .query_revisions(this.browser.playlists().entries().playlist_id);
                    let current = this.playlist_view_demand(kind)
                        && this.browser.playlist_read_current(&read, revisions);
                    if !current {
                        this.finish_loading_text(&kind, generation, cx);
                        this.browser.abandon_playlist_read(&read);
                        this.dispatch_queued_view(kind, cx);
                        cx.notify();
                    }
                    current
                })
                .unwrap_or(false)
            {
                return;
            }
            let response = cx
                .background_executor()
                .spawn(async move { handle.query_if_current(query, ticket) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.finish_loading_text(&kind, generation, cx);
                this.refresh(cx);
                let revisions = this
                    .state
                    .query_revisions(this.browser.playlists().entries().playlist_id);
                if let Some(error) = this
                    .browser
                    .complete_playlist_read(&read, revisions, response)
                {
                    this.report_error(error, cx);
                }
                this.dispatch_queued_view(kind, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn dispatch_queued_view(&mut self, kind: ViewRequestKind, cx: &mut Context<Self>) {
        let revisions = self
            .state
            .query_revisions(self.browser.playlists().entries().playlist_id);
        let demanded = self.playlist_view_demand(kind);
        let prepared = self.browser.queued_playlist_read(kind, demanded, revisions);
        self.request_view(prepared, cx);
    }
    pub(in crate::gui) fn request_playlist_selection(&mut self, cx: &mut Context<Self>) {
        if !self.panel_demand("playlists") {
            return;
        }
        let prepared = self
            .browser
            .prepare_playlist_lookup(self.state.query_revisions(None));
        self.request_view(prepared, cx);
    }

    pub(in crate::gui) fn request_playlist_summaries(
        &mut self,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        let prepared = self.browser.request_playlist_summaries(
            offset,
            self.view_page_size(),
            self.state.query_revisions(None),
        );
        self.request_view(prepared, cx);
    }

    pub(in crate::gui) fn request_playlist_entries(
        &mut self,
        playlist_id: i64,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        let prepared = self.browser.request_playlist_entries(
            playlist_id,
            offset,
            self.view_page_size(),
            self.state.query_revisions(Some(playlist_id)),
        );
        self.request_view(prepared, cx);
    }

    pub(in crate::gui) fn ensure_playlist_range(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) {
        if !self.playlist_view_demand(ViewRequestKind::Summaries) {
            return;
        }
        let revisions = self
            .state
            .query_revisions(self.browser.playlists().entries().playlist_id);
        let prepared =
            self.browser
                .demand_playlist_summaries(range, self.view_page_size(), revisions);
        self.request_view(prepared, cx);
    }
    pub(in crate::gui) fn ensure_entry_range(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) {
        if !self.panel_demand("playlists") {
            return;
        }
        let revisions = self
            .state
            .query_revisions(self.browser.playlists().entries().playlist_id);
        let prepared =
            self.browser
                .demand_playlist_entries(range, self.view_page_size(), revisions);
        self.request_view(prepared, cx);
    }

    pub(in crate::gui) fn retry_view_page(
        &mut self,
        kind: ViewRequestKind,
        cx: &mut Context<Self>,
    ) {
        let page_size = self.view_page_size();
        match kind {
            ViewRequestKind::Summaries | ViewRequestKind::Entries => {
                let revisions = self
                    .state
                    .query_revisions(self.browser.playlists().entries().playlist_id);
                let prepared = if kind == ViewRequestKind::Entries {
                    self.browser.retry_playlist_entries(page_size, revisions)
                } else {
                    self.browser.retry_playlist_summaries(page_size, revisions)
                };
                self.request_view(prepared, cx);
            }
            _ => {}
        }
    }
    pub(super) fn choose_playlist_import(&mut self, cx: &mut Context<Self>) {
        let i18n = Arc::clone(&self.i18n);
        cx.spawn(async move |this, cx| {
            let result = SelectedFiles::open_file()
                .title(i18n.text("shell-import-playlist"))
                .accept_label(i18n.text("shell-import"))
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                match chooser_result(result) {
                    Ok(Some(files)) => {
                        if let Some(path) = chooser_path(&files) {
                            this.send(Command::ImportPlaylist { path, name: None }, cx);
                        }
                    }
                    Ok(None) => return,
                    Err(error) => this.report_error(
                        this.i18n.format(
                            "shell-open-playlist-error",
                            &[("error", error.to_string().into())],
                        ),
                        cx,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn choose_playlist_export(&mut self, playlist_id: i64, cx: &mut Context<Self>) {
        let i18n = Arc::clone(&self.i18n);
        cx.spawn(async move |this, cx| {
            let result = SelectedFiles::save_file()
                .title(i18n.text("shell-export-playlist"))
                .accept_label(i18n.text("shell-export"))
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                match chooser_result(result) {
                    Ok(Some(files)) => {
                        if let Some(path) = chooser_path(&files) {
                            this.send(Command::ExportPlaylist { playlist_id, path }, cx);
                        }
                    }
                    Ok(None) => return,
                    Err(error) => this.report_error(
                        this.i18n.format(
                            "shell-open-playlist-destination-error",
                            &[("error", error.to_string().into())],
                        ),
                        cx,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn paged_loading_row(
        &self,
        kind: ViewRequestKind,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let error = match kind {
            ViewRequestKind::Summaries => self.browser.playlists().summaries().rows.error(index),
            ViewRequestKind::Entries => self.browser.playlists().entries().rows.error(index),
            _ => None,
        };
        rivu_ui::loading::paged_loading_row(
            ("loading-row", index),
            error,
            self.loading_text.show(&kind, error.is_some()),
            self.tr(if error.is_some() {
                "shell-loading-failed"
            } else {
                "shell-loading"
            }),
            cx,
            move |this, _, cx| {
                this.retry_view_page(kind, cx);
                cx.notify();
            },
        )
    }
    pub(super) fn track_favorite_toggle(
        &self,
        id: impl Into<gpui::ElementId>,
        group: &'static str,
        track_id: i64,
        favorite: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        crate::gui::components::track::track_favorite_toggle(
            id,
            group,
            favorite,
            |key| self.tr(key),
            cx,
            move |this, _, cx| {
                this.send(
                    Command::SetFavorite {
                        track_ids: vec![track_id],
                        favorite: !favorite,
                    },
                    cx,
                );
            },
        )
    }
}
