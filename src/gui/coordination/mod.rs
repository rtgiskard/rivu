//! Window coordination of feature intents; feature models and workers remain owner-local.
pub(super) mod manual_actions;
pub(super) mod queue_replace;

use super::*;
use manual_actions::ManualAction;

impl GuiApp {
    pub(super) fn subscribe_features(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let mut duplicates_open = self.duplicates.read(cx).is_open();
        let mut playlist_overlay =
            self.playlists.read(cx).editor_open() || self.playlists.read(cx).action().is_some();
        vec![
            cx.subscribe_in(&self.library, window, |this, _, event, window, cx| {
                use library::LibraryEvent;
                match event {
                    LibraryEvent::Select(request) => {
                        let intent = this.begin_manual_intent(cx);
                        let request = library::selection::LibraryRowRequest {
                            node: request.node.clone(),
                            index: request.index,
                            shift: request.shift,
                            multi: request.multi,
                            scroll: request.scroll,
                        };
                        this.library.update(cx, |owner, cx| {
                            owner.prepare_library_row(intent, request, cx)
                        });
                    }
                    LibraryEvent::Navigate(index) => {
                        let intent = this.begin_manual_intent(cx);
                        this.library
                            .update(cx, |owner, cx| owner.prepare_tree_row(intent, *index, cx));
                    }
                    LibraryEvent::Ready(intent, commit) => {
                        if let Some(commit) = commit.borrow_mut().take() {
                            this.ready_manual_action(*intent, ManualAction::Library(commit), cx);
                        }
                    }
                    LibraryEvent::Failed(intent) => this.fail_manual_intent(*intent, cx),
                    LibraryEvent::SelectionChanged(id) => {
                        this.metadata.update(cx, |owner, cx| {
                            if let Some(id) = id {
                                owner.request_track(*id, cx);
                            } else if !owner.protected(cx) {
                                owner.clear_selection(cx);
                            }
                        });
                        this.sync_waveform(cx);
                    }
                    LibraryEvent::SearchChanged => this.search_changed(cx),
                    LibraryEvent::Focus => {
                        this.set_browser_focus(Some(BrowserFocus::List(ListFocus::Library)), cx);
                        this.focus_layout(window, cx);
                    }
                    LibraryEvent::Reveal => this.activate_library(window, cx, false),
                    LibraryEvent::ContentMenu(payload) => {
                        if let Some(menu) = payload.borrow_mut().take() {
                            this.open_object_menu(
                                menu.request,
                                menu.guard,
                                menu.remove,
                                menu.nodes,
                                menu.root,
                                cx,
                            );
                        }
                    }
                    LibraryEvent::Error(error) => this.feature_error(error, cx),
                }
            }),
            cx.subscribe_in(&self.queue, window, |this, _, event, window, cx| {
                use queue::QueueEvent;
                match event {
                    QueueEvent::SelectionRequested(request) => {
                        let intent = this.begin_manual_intent(cx);
                        this.queue.update(cx, |owner, cx| {
                            owner.prepare_selection(*request, intent, cx)
                        });
                    }
                    QueueEvent::SelectionReady { intent, commit } => {
                        if let Some(commit) = commit.borrow_mut().take() {
                            this.ready_manual_action(*intent, ManualAction::Queue(commit), cx);
                        }
                    }
                    QueueEvent::SelectionFailed(intent) => this.fail_manual_intent(*intent, cx),
                    QueueEvent::SelectionCommitted { track_id } => {
                        this.selected_track(*track_id, cx)
                    }
                    QueueEvent::SearchChanged => this.search_changed(cx),
                    QueueEvent::FocusRequested => {
                        this.set_browser_focus(Some(BrowserFocus::List(ListFocus::Queue)), cx);
                        this.focus_layout(window, cx);
                    }
                    QueueEvent::ContentMenu {
                        request,
                        revision,
                        remove,
                    } => {
                        if let Some(request) = request.borrow_mut().take() {
                            this.open_object_menu(
                                request,
                                list_interaction::ContentGuard::Queue(*revision),
                                remove.borrow_mut().take(),
                                None,
                                None,
                                cx,
                            );
                        }
                    }
                    QueueEvent::DropLibrary(nodes) => {
                        if let Some(nodes) = nodes.borrow_mut().take() {
                            this.library.update(cx, |owner, cx| {
                                owner.resolve_library_drag(&nodes, DragDestination::Queue, cx)
                            });
                        }
                    }
                    QueueEvent::DropContent(source) => {
                        if let Some(source) = source.borrow_mut().take() {
                            this.drop_content(source, DragDestination::Queue, cx);
                        }
                    }
                    QueueEvent::DropPlaylist(ids) => {
                        if let Some(ids) = ids.borrow_mut().take() {
                            this.send_content_tracks(ids, DragDestination::Queue, cx);
                        }
                    }
                    QueueEvent::Error(error) => this.feature_error(error, cx),
                }
            }),
            cx.subscribe_in(&self.playlists, window, |this, _, event, window, cx| {
                use playlists::PlaylistEvent;
                match event {
                    PlaylistEvent::Selection(request) => {
                        // Resolve while the feature's previous desired cursor still exists.
                        // Root cancellation retires it; the prepared target and scope survive.
                        let prepared = this.playlists.update(cx, |owner, cx| {
                            owner.prepare_selection_request(*request, cx)
                        });
                        if let Some(prepared) = prepared {
                            let intent = this.begin_manual_intent(cx);
                            this.playlists.update(cx, |owner, cx| {
                                owner.prepare_selection(prepared, intent, cx)
                            });
                        }
                    }
                    PlaylistEvent::Ready(intent, commit) => {
                        if let Some(commit) = commit.borrow_mut().take() {
                            this.ready_manual_action(*intent, ManualAction::Playlist(commit), cx);
                        }
                    }
                    PlaylistEvent::Failed(intent) => this.fail_manual_intent(*intent, cx),
                    PlaylistEvent::InvalidateManualIntent => {
                        this.invalidate_manual_intent(cx);
                    }
                    PlaylistEvent::SearchChanged => this.search_changed(cx),
                    PlaylistEvent::Metadata(id) => this.selected_track(*id, cx),
                    PlaylistEvent::Focus(focus) => {
                        if focus.is_some() {
                            this.set_browser_focus(*focus, cx);
                        }
                        this.focus_layout(window, cx);
                    }
                    PlaylistEvent::FocusName => this
                        .playlists
                        .update(cx, |owner, cx| owner.focus_name(window, cx)),
                    PlaylistEvent::ClearMenus => {
                        this.panel_menu = None;
                        this.replace_content_menu(None, cx);
                        this.queue_replace = None;
                        cx.notify();
                    }
                    PlaylistEvent::CancelInteraction => this.cancel_interaction(window, cx),
                    PlaylistEvent::QueueReplace(payload) => {
                        if let Some(replacement) = payload.borrow_mut().take() {
                            this.request_queue_replace(replacement, cx);
                        }
                    }
                    PlaylistEvent::AppendPlaylist(id) => this.append_playlist_to_queue(*id, cx),
                    PlaylistEvent::ResolveLibrary(nodes, destination) => {
                        if let Some(nodes) = nodes.borrow_mut().take() {
                            this.library.update(cx, |owner, cx| {
                                owner.resolve_library_drag(&nodes, *destination, cx)
                            });
                        }
                    }
                    PlaylistEvent::Transfer(source, destination) => {
                        if let Some(source) = source.borrow_mut().take() {
                            this.drop_content(source, *destination, cx);
                        }
                    }
                    PlaylistEvent::ContentMenu(payload) => {
                        if let Some(menu) = payload.borrow_mut().take() {
                            this.open_object_menu(
                                menu.request,
                                menu.guard,
                                menu.remove,
                                None,
                                None,
                                cx,
                            );
                        }
                    }
                    PlaylistEvent::Error(error) => this.feature_error(error, cx),
                }
            }),
            cx.subscribe_in(&self.history, window, |this, _, event, window, cx| {
                use history::HistoryEvent;
                match event {
                    HistoryEvent::SelectionRequested {
                        id,
                        shift,
                        additive,
                    } => {
                        let intent = this.begin_manual_intent(cx);
                        let prepared = this.history.update(cx, |owner, cx| {
                            owner.prepare_selection(*id, *shift, *additive, intent, cx)
                        });
                        if !prepared {
                            this.fail_manual_intent(intent, cx);
                        }
                    }
                    HistoryEvent::SelectionReady {
                        intent,
                        commit,
                        structure_revision,
                    } => {
                        if let Some(commit) = commit.borrow_mut().take() {
                            this.ready_manual_action(
                                *intent,
                                ManualAction::History {
                                    commit,
                                    structure_revision: *structure_revision,
                                },
                                cx,
                            );
                        }
                    }
                    HistoryEvent::SelectionCommitted { track_id } => {
                        this.selected_track(*track_id, cx)
                    }
                    HistoryEvent::SearchChanged => this.search_changed(cx),
                    HistoryEvent::InvalidateManualIntent => this.invalidate_manual_intent(cx),
                    HistoryEvent::FocusRequested { panel_id } => {
                        this.search_panel = Some(*panel_id);
                        this.set_browser_focus(Some(BrowserFocus::History), cx);
                        this.focus_layout(window, cx);
                    }
                    HistoryEvent::ContentMenu(payload) => {
                        if let Some(request) = payload.borrow_mut().take() {
                            this.open_content_menu(request, cx);
                        }
                    }
                    HistoryEvent::Error(error) => this.feature_error(error, cx),
                }
            }),
            cx.subscribe(&self.metadata, |this, _, event, cx| {
                use metadata::MetadataEvent;
                match event {
                    MetadataEvent::ReloadPlaybackRequested => {
                        let intent = this.begin_manual_intent(cx);
                        this.ready_manual_action(intent, ManualAction::ReloadPlayback, cx);
                    }
                    MetadataEvent::ResumeManualAction { resume_follow } => {
                        if this.manual_confirmation_open() {
                            this.resume_manual_action(false, cx);
                        } else if *resume_follow {
                            this.reconcile_manual_follow(cx);
                        }
                    }
                    MetadataEvent::TrackChanged => this.sync_waveform(cx),
                    MetadataEvent::Error(error) => this.feature_error(error, cx),
                }
            }),
            cx.subscribe(
                &self.metadata,
                |this, _, event: &metadata::SavedHintChanged, cx| {
                    this.full_waveform_request.saved_hint_changed(&event.0);
                    this.waveform
                        .update(cx, |waveform, cx| waveform.saved_hint_changed(&event.0, cx));
                    this.sync_waveform(cx);
                },
            ),
            cx.subscribe_in(&self.duplicates, window, |this, _, _, window, cx| {
                if this.shell.is_open(shell::Surface::Settings) {
                    this.settings.read(cx).focus_handle(cx).focus(window, cx);
                } else {
                    this.focus_layout(window, cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&self.playback, window, |this, _, event, window, cx| {
                use playback::PlaybackEvent;
                match event {
                    PlaybackEvent::LocateQueue => {
                        if this.reveal_queue_panel(window, cx) {
                            this.queue
                                .update(cx, |owner, cx| owner.locate_playing_queue(window, cx));
                        }
                    }
                    PlaybackEvent::SliderDown {
                        dragging,
                        position,
                        alt,
                    } => {
                        this.synchronize_interaction(window, cx);
                        let cancellation = this.workspace.interaction.begin_slider(
                            *dragging,
                            this.state.queue.current_id,
                            this.state.playback.duration,
                        );
                        if cancellation.docking {
                            cx.stop_active_drag(window);
                        }
                        this.apply_interaction_cancellation(cancellation, cx);
                        this.drag_position(*position, *alt, cx);
                    }
                    PlaybackEvent::Error(error) => this.feature_error(error, cx),
                }
            }),
            cx.observe(&self.library, |this, _, cx| this.sync_transfer_state(cx)),
            cx.observe(&self.duplicates, move |_, owner, cx| {
                let open = owner.read(cx).is_open();
                if open || duplicates_open {
                    cx.notify();
                }
                duplicates_open = open;
            }),
            cx.observe(&self.playlists, move |this, owner, cx| {
                let open = owner.read(cx).editor_open() || owner.read(cx).action().is_some();
                if open
                    || playlist_overlay
                    || this
                        .content_menu
                        .as_ref()
                        .is_some_and(|menu| menu.playlists)
                {
                    cx.notify();
                }
                playlist_overlay = open;
            }),
            cx.observe(&self.metadata, |this, _, cx| {
                if this.manual_confirmation_open() {
                    cx.notify();
                }
            }),
        ]
    }

    fn feature_error(&mut self, error: &str, cx: &mut Context<Self>) {
        self.error = Some(error.to_owned());
        cx.notify();
    }

    fn selected_track(&mut self, id: i64, cx: &mut Context<Self>) {
        self.metadata
            .update(cx, |owner, cx| owner.request_track(id, cx));
        self.sync_waveform(cx);
    }

    fn search_changed(&mut self, cx: &mut Context<Self>) {
        let intent = self.begin_manual_intent(cx);
        self.metadata.update(cx, |owner, cx| {
            if !owner.protected(cx) {
                owner.clear_selection(cx);
            }
        });
        self.settle_manual_intent(intent, cx);
        self.sync_waveform(cx);
    }

    pub(super) fn set_browser_focus(
        &mut self,
        focus: Option<BrowserFocus>,
        cx: &mut Context<Self>,
    ) {
        let changed = self.focus != focus;
        self.focus = focus;
        self.library.update(cx, |owner, cx| {
            owner.set_focused(focus == Some(BrowserFocus::List(ListFocus::Library)), cx)
        });
        self.queue.update(cx, |owner, cx| {
            owner.set_focused(focus == Some(BrowserFocus::List(ListFocus::Queue)), cx)
        });
        self.playlists.update(cx, |owner, cx| {
            owner.set_focused(
                matches!(
                    focus,
                    Some(BrowserFocus::Playlists | BrowserFocus::List(ListFocus::PlaylistTracks))
                ),
                cx,
            )
        });
        if changed {
            cx.notify();
        }
    }

    pub(super) fn sync_transfer_state(&self, cx: &mut Context<Self>) {
        let pending = self.library.read(cx).library_range_pending();
        self.queue.update(cx, |owner, cx| {
            owner.set_transfer_state(pending, self.content_resolution, cx)
        });
        self.playlists.update(cx, |owner, cx| {
            owner.set_transfer_state(pending, self.content_resolution, cx)
        });
    }

    pub(super) fn reconcile_features(&mut self, cx: &mut Context<Self>) {
        self.library
            .update(cx, |owner, cx| owner.reconcile(Rc::clone(&self.state), cx));
        self.queue
            .update(cx, |owner, cx| owner.reconcile(Rc::clone(&self.state), cx));
        self.playlists
            .update(cx, |owner, cx| owner.reconcile(Rc::clone(&self.state), cx));
        self.history.update(cx, |owner, cx| {
            owner.reconcile(Rc::clone(&self.state), cx);
            owner.set_ui_font(self.ui_font.clone(), cx);
        });
        self.metadata
            .update(cx, |owner, cx| owner.reconcile(Rc::clone(&self.state), cx));
        self.duplicates
            .update(cx, |owner, cx| owner.reconcile(Rc::clone(&self.state), cx));
        self.playback
            .update(cx, |owner, cx| owner.reconcile(Rc::clone(&self.state), cx));
        self.reconcile_feature_demand(cx);
    }

    pub(super) fn reconcile_feature_demand(&self, cx: &mut Context<Self>) {
        let library_visible = self.panel_demand("library");
        let available = !self.workspace.layouts.current().locked
            || self
                .workspace
                .layouts
                .current()
                .panels()
                .iter()
                .any(|panel| panel.kind == "library");
        self.library.update(cx, |owner, cx| {
            owner.set_available(available);
            owner.set_visible(library_visible, cx);
        });
        self.queue.update(cx, |owner, cx| {
            owner.set_visible(self.panel_demand("queue"), cx)
        });
        self.playlists.update(cx, |owner, cx| {
            owner.set_visibility(
                self.window_visible,
                self.panel_demand("playlists"),
                self.content_menu
                    .as_ref()
                    .is_some_and(|menu| menu.playlists),
                cx,
            )
        });
        self.metadata.update(cx, |owner, cx| {
            owner.set_visible(self.panel_demand("metadata"), cx)
        });
    }

    pub(super) fn cancel_window_reads(&self, cx: &mut Context<Self>) {
        self.library.update(cx, |owner, _| owner.cancel_reads());
        self.queue
            .update(cx, |owner, cx| owner.cancel_window_reads(cx));
        self.playlists
            .update(cx, |owner, cx| owner.cancel_window_reads(cx));
        self.metadata.update(cx, |owner, _| owner.cancel_read());
    }

    pub(super) fn sync_playback_interaction(&self, cx: &mut Context<Self>) {
        self.playback.update(cx, |owner, cx| {
            owner.set_interaction(
                self.workspace.interaction.seek_preview(),
                self.workspace.interaction.dragging(),
                cx,
            )
        });
    }
}
