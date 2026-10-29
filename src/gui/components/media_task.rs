//! Accepted media-task progress shared by its owning Library, Queue or Playlists surface.
use crate::{core::state::MediaTaskOwner, i18n::Localizer, projection::GuiSystemSnapshot};
use gpui::{Context, Div, SharedString, div, prelude::*, px, relative, rgb};
use rivu_protocol::model::{Command, ScanPhase};
use rivu_ui::{
    IconButton, IconButtonDensity, IconButtonStyle, StatusKind, caption, column,
    panel_header_actions, panel_heading_row, status_message,
};

pub(in crate::gui) fn media_task_status<T: 'static>(
    system: &GuiSystemSnapshot,
    i18n: &Localizer,
    panel_id: u64,
    cx: &mut Context<T>,
    send: fn(&mut T, Command, &mut Context<T>),
) -> Div {
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
    let dismissible = !system.scanning
        && progress.is_some_and(|progress| {
            matches!(
                progress.phase,
                ScanPhase::Completed | ScanPhase::Failed | ScanPhase::Cancelled
            )
        });
    let actions = panel_header_actions()
        .min_w_8()
        .min_h_8()
        .when(system.scanning, |slot| {
            slot.child(
                IconButton::new(
                    ("media-task-cancel", panel_id),
                    "",
                    i18n.text("library-cancel-task").to_owned(),
                )
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
        .when(dismissible, |slot| {
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
    let path: SharedString = progress
        .and_then(|progress| progress.path.as_ref())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
        .into();
    let message: SharedString = system.scan_message.clone().into();
    // Keep phases in fixed cells; only dismissal removes the accepted-task surface.
    column()
        .gap_1()
        .min_w_0()
        .flex_shrink_0()
        .child(panel_heading_row(caption(theme, heading).truncate(), actions).min_h_8())
        .child(bar)
        .child(
            caption(theme, path.clone())
                .id(("media-task-path", panel_id))
                .min_h_5()
                .flex_shrink_0()
                .min_w_0()
                .truncate()
                .tooltip(rivu_ui::text_tooltip(path)),
        )
        .child(
            status_message(
                theme,
                message.clone(),
                if failed {
                    StatusKind::Error
                } else {
                    StatusKind::Info
                },
            )
            .id(("media-task-message", panel_id))
            .min_h_5()
            .flex_shrink_0()
            .min_w_0()
            .truncate()
            .tooltip(rivu_ui::text_tooltip(message)),
        )
}
