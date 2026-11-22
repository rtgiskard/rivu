//! Accepted media-task progress shared by its owning Library, Queue or Playlists surface.
use crate::gui::feature::FeatureContext;
use crate::projection::GuiSystemSnapshot;
use gpui::{ClipboardItem, Context, Div, SharedString, Window, div, prelude::*, px, relative, rgb};
use rivu_presentation::i18n::Localizer;
use rivu_protocol::model::{Command, MediaTaskOwner, Query, ScanPhase};
use rivu_protocol::response::ViewResponse;
use rivu_ui::{
    IconButton, IconButtonDensity, IconButtonStyle, StatusKind, caption, column,
    panel_header_actions, panel_heading_row, status_message,
};

#[derive(Default)]
struct TaskDetails {
    outcome: Option<SharedString>,
    error: Option<SharedString>,
}

impl TaskDetails {
    fn from_reply(
        id: u64,
        phase: ScanPhase,
        importing: bool,
        reply: Option<ViewResponse>,
        i18n: &Localizer,
    ) -> Self {
        let status = match reply {
            Some(ViewResponse::ScanTask(status))
                if status.id == id && status.progress.phase == phase =>
            {
                status
            }
            _ => {
                return Self {
                    outcome: Some(i18n.text("gl-task-details-unavailable").into()),
                    error: None,
                };
            }
        };
        let outcome = importing.then(|| {
            let library = i18n.text(if status.library_committed {
                "gl-task-library-saved"
            } else {
                "gl-task-library-unconfirmed"
            });
            let playlist = status.playlist_id.map_or_else(
                || i18n.text("gl-task-playlist-unconfirmed").to_owned(),
                |id| {
                    i18n.format(
                        "core-scan-playlist-created",
                        &[("id", id.to_string().into())],
                    )
                },
            );
            SharedString::from(format!("{library} · {playlist}"))
        });
        Self {
            outcome,
            error: if phase == ScanPhase::Failed {
                status.error.map(Into::into)
            } else {
                None
            },
        }
    }
}

struct TaskDetailsSlot {
    details: Option<TaskDetails>,
}

impl TaskDetailsSlot {
    fn load(
        context: &FeatureContext,
        id: u64,
        phase: ScanPhase,
        importing: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let handle = context.handle().clone();
        let i18n = context.i18n.clone();
        // Drain the read even after unmount. The weak handle can only update
        // the keyed slot that requested it, never its replacement.
        cx.spawn(async move |slot, cx| {
            let publish = |reply: rivu_protocol::response::QueryResponse,
                           cx: &mut gpui::AsyncApp| {
                let _ = slot.update(cx, |slot, cx| {
                    if slot.publish(|| {
                        TaskDetails::from_reply(id, phase, importing, reply.result.ok(), &i18n)
                    }) {
                        cx.notify();
                    }
                });
            };
            let (reply, _) = crate::gui::query::run(
                handle,
                cx,
                |handle| async move { handle.query_for_gui(Query::ScanTask { id }).await },
                &publish,
            )
            .await;
            publish(reply, cx);
        })
        .detach();
        Self { details: None }
    }

    fn publish(&mut self, details: impl FnOnce() -> TaskDetails) -> bool {
        if self.details.is_some() {
            return false;
        }
        self.details = Some(details());
        true
    }
}

pub(in crate::gui) fn media_task_status<T: 'static>(
    system: &GuiSystemSnapshot,
    context: &FeatureContext,
    panel_id: u64,
    window: &mut Window,
    cx: &mut Context<T>,
    send: fn(&mut T, Command, &mut Context<T>),
) -> Div {
    let i18n = &context.i18n;
    let theme = rivu_ui::theme::palette(cx);
    let progress = system.scan_progress.as_ref();
    let importing = system.scan_owner == Some(MediaTaskOwner::Playlist);
    let heading = progress.map_or_else(
        || {
            i18n.text(if importing {
                "gl-importing"
            } else {
                "gl-scanning"
            })
            .to_owned()
        },
        |progress| {
            let phase = i18n.text(if importing {
                match progress.phase {
                    ScanPhase::Discovering => "gl-import-discovering",
                    ScanPhase::ReadingMetadata => "gl-scan-metadata",
                    ScanPhase::Saving => "gl-import-saving",
                    ScanPhase::Completed => "gl-import-completed",
                    ScanPhase::Failed => "gl-import-failed",
                    ScanPhase::Cancelled => "gl-import-cancelled",
                }
            } else {
                match progress.phase {
                    ScanPhase::Discovering => "gl-scan-discovering",
                    ScanPhase::ReadingMetadata => "gl-scan-metadata",
                    ScanPhase::Saving => "gl-scan-saving",
                    ScanPhase::Completed => "gl-scan-completed",
                    ScanPhase::Failed => "gl-scan-failed",
                    ScanPhase::Cancelled => "gl-scan-cancelled",
                }
            });
            match progress.total {
                Some(total) => i18n.format(
                    "gl-scan-progress",
                    &[
                        ("phase", phase.into()),
                        ("processed", progress.processed.into()),
                        ("total", total.into()),
                        ("errors", progress.errors.into()),
                    ],
                ),
                None => i18n.format(
                    "gl-scan-found",
                    &[
                        ("phase", phase.into()),
                        ("processed", progress.processed.into()),
                        ("errors", progress.errors.into()),
                    ],
                ),
            }
        },
    );
    let failed = progress.is_some_and(|progress| progress.phase == ScanPhase::Failed);
    let task_id = system.scan_task_id;
    let cancellable = system.scanning
        && task_id.is_some()
        && progress.is_none_or(|progress| progress.phase != ScanPhase::Saving);
    let terminal = progress.map(|p| p.phase).filter(|phase| {
        !system.scanning
            && matches!(
                phase,
                ScanPhase::Completed | ScanPhase::Failed | ScanPhase::Cancelled
            )
    });
    // The window and its query handle are bound to one Core instance. The
    // keyed slot belongs only to this panel and task, not to the next result.
    let details = task_id
        .zip(terminal)
        .filter(|_| importing || failed)
        .map(|(id, phase)| {
            window.with_element_namespace(("media-task-result", panel_id), |window| {
                window.use_keyed_state(("task", id), cx, |_, cx| {
                    TaskDetailsSlot::load(context, id, phase, importing, cx)
                })
            })
        });
    let details = details
        .as_ref()
        .and_then(|slot| slot.read(cx).details.as_ref());
    let outcome = details.and_then(|details| details.outcome.clone());
    let error = details.and_then(|details| details.error.clone());
    let message = SharedString::from(system.scan_message.as_str());
    let actions = panel_header_actions()
        .min_w_8()
        .min_h_8()
        .when_some(error, |slot, error| {
            let snapshots = context.snapshots.clone();
            slot.child(
                IconButton::new(
                    ("media-task-copy-error", panel_id),
                    "",
                    i18n.text("shell-copy-message").to_owned(),
                )
                .surface(crate::gui::appearance::material::surface(cx))
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Regular,
                    cx,
                    move |_, _, cx| {
                        let current = snapshots.read();
                        if snapshots.handle().is_connected()
                            && !current.system.scanning
                            && current.system.scan_task_id == task_id
                        {
                            cx.write_to_clipboard(ClipboardItem::new_string(error.to_string()));
                        }
                    },
                ),
            )
        })
        .when(system.scanning, |slot| {
            slot.child(
                IconButton::new(
                    ("media-task-cancel", panel_id),
                    "",
                    i18n.text("library-cancel-task").to_owned(),
                )
                .surface(crate::gui::appearance::material::surface(cx))
                .enabled(cancellable)
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Regular,
                    cx,
                    move |this, _, cx| {
                        if let Some(id) = task_id {
                            send(this, Command::CancelScan { id }, cx);
                        }
                    },
                ),
            )
        })
        .when(terminal.is_some(), |slot| {
            slot.child(
                IconButton::new(
                    ("media-task-dismiss", panel_id),
                    "",
                    i18n.text(if importing {
                        "gl-dismiss-import"
                    } else {
                        "gl-dismiss-scan"
                    })
                    .to_owned(),
                )
                .surface(crate::gui::appearance::material::surface(cx))
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Regular,
                    cx,
                    move |this, _, cx| {
                        send(this, Command::DismissScanResult, cx);
                    },
                ),
            )
        });
    let mut bar = div()
        .w_full()
        .h(px(3.))
        .flex_shrink_0()
        .bg(rgb(theme.border));
    if let Some(progress) = progress
        && let Some(total) = progress.total.filter(|total| *total > 0)
    {
        let fraction = (progress.processed as f32 / total as f32).clamp(0., 1.);
        bar = bar.child(div().h_full().w(relative(fraction)).bg(rgb(if failed {
            theme.error
        } else {
            theme.accent
        })));
    }
    let path = outcome.unwrap_or_else(|| {
        progress
            .and_then(|progress| progress.path.as_ref())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into()
    });
    // Keep phases in fixed cells; only dismissal removes the accepted-task surface.
    column()
        .gap_1()
        .min_w_0()
        .flex_shrink_0()
        .child(panel_heading_row(caption(theme, heading).truncate(), actions).min_h_8())
        .child(bar)
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme.secondary))
                .id(("media-task-path", panel_id))
                .min_h_5()
                .flex_shrink_0()
                .min_w_0()
                .truncate()
                .child(rivu_ui::truncated_label("media-task-path-text", path)),
        )
        .child(
            status_message(
                ("media-task-message", panel_id),
                theme,
                message,
                if failed {
                    StatusKind::Error
                } else {
                    StatusKind::Info
                },
            )
            .min_h_5()
            .flex_shrink_0()
            .min_w_0()
            .truncate(),
        )
}

#[cfg(test)]
mod tests {
    use super::TaskDetails;
    use rivu_presentation::i18n::Localizer;
    use rivu_protocol::{
        model::{ScanPhase, ScanProgress, ScanTaskStatus},
        response::ViewResponse,
    };

    fn failed_import() -> ScanTaskStatus {
        ScanTaskStatus {
            id: 8,
            progress: ScanProgress {
                phase: ScanPhase::Failed,
                processed: 2,
                total: Some(2),
                errors: 1,
                path: None,
            },
            message: "Processed two files".into(),
            library_committed: true,
            playlist_id: None,
            error: Some("Unreadable entry\n/path/to/file".into()),
        }
    }

    #[test]
    fn failed_import_keeps_each_commit_and_the_complete_task_error() {
        let i18n = Localizer::new("en-US");
        for (library_committed, playlist_id) in [(false, None), (true, None), (true, Some(42))] {
            let status = ScanTaskStatus {
                library_committed,
                playlist_id,
                ..failed_import()
            };
            let details = TaskDetails::from_reply(
                status.id,
                status.progress.phase,
                true,
                Some(ViewResponse::ScanTask(status)),
                &i18n,
            );
            assert_eq!(
                details.error.as_deref(),
                Some("Unreadable entry\n/path/to/file")
            );
            let outcome = details.outcome.unwrap();
            assert!(outcome.contains(i18n.text(if library_committed {
                "gl-task-library-saved"
            } else {
                "gl-task-library-unconfirmed"
            })));
            if playlist_id.is_some() {
                assert!(outcome.contains("42"));
            } else {
                assert!(outcome.contains(i18n.text("gl-task-playlist-unconfirmed")));
            }
        }
    }

    #[test]
    fn non_failed_import_retains_outcome_but_not_error() {
        let i18n = Localizer::new("en-US");
        for phase in [ScanPhase::Completed, ScanPhase::Cancelled] {
            let mut status = failed_import();
            status.progress.phase = phase;
            let details = TaskDetails::from_reply(
                status.id,
                phase,
                true,
                Some(ViewResponse::ScanTask(status)),
                &i18n,
            );
            assert!(details.error.is_none());
            let outcome = details.outcome.unwrap();
            assert!(outcome.contains(i18n.text("gl-task-library-saved")));
            assert!(outcome.contains(i18n.text("gl-task-playlist-unconfirmed")));
        }
    }

    #[test]
    fn timeout_feedback_is_not_replaced_when_worker_finishes() {
        let i18n = Localizer::new("en-US");
        let mut slot = super::TaskDetailsSlot { details: None };
        assert!(slot.publish(|| TaskDetails::from_reply(8, ScanPhase::Failed, true, None, &i18n)));
        assert!(!slot.publish(|| panic!("late worker result published")));
        assert_eq!(
            slot.details.unwrap().outcome.as_deref(),
            Some(i18n.text("gl-task-details-unavailable"))
        );
    }

    #[test]
    fn stale_or_unavailable_result_never_supplies_copy_or_commit_evidence() {
        let i18n = Localizer::new("en-US");
        let status = failed_import();
        for (id, phase, reply) in [
            (
                9,
                ScanPhase::Failed,
                Some(ViewResponse::ScanTask(status.clone())),
            ),
            (
                8,
                ScanPhase::Completed,
                Some(ViewResponse::ScanTask(status.clone())),
            ),
            (8, ScanPhase::Failed, None),
        ] {
            let details = TaskDetails::from_reply(id, phase, true, reply, &i18n);
            assert!(details.error.is_none());
            assert_eq!(
                details.outcome.as_deref(),
                Some(i18n.text("gl-task-details-unavailable"))
            );
        }
        let details = TaskDetails::from_reply(
            status.id,
            status.progress.phase,
            false,
            Some(ViewResponse::ScanTask(ScanTaskStatus {
                error: None,
                ..status
            })),
            &i18n,
        );
        assert!(details.error.is_none(), "summary is not a copyable error");
        assert!(
            details.outcome.is_none(),
            "non-import has no playlist outcome"
        );
    }
}
