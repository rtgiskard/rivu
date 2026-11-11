use super::*;
use crate::{
    gui::{
        WindowView,
        layout::{
            panel_menu::{PanelMenu, PanelMenuPage},
            persistence::{LayoutPersistence, LoadedLayouts},
        },
    },
    test_support::CoreRuntime,
};
use gpui::{AnyWindowHandle, InputEvent, TestAppContext, VisualTestContext, point, px};
use rivu_protocol::{
    model::{Query, QueueRow},
    response::ViewResponse,
};
use std::sync::Arc;

struct Fixture {
    app: Entity<WindowView>,
    window: AnyWindowHandle,
    runtime: CoreRuntime,
    _save: LayoutPersistence,
    _directory: tempfile::TempDir,
    rows: Vec<QueueRow>,
}

struct QueueWindow(Entity<WindowView>);

impl Render for QueueWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.0.read(cx);
        let queue = app.queue.clone();
        let menu = app.panel_menu;
        let mut surface = gpui::div()
            .relative()
            .size_full()
            .child(queue.update(cx, |queue, cx| queue.render_panel(1, window, cx)));
        if let Some(menu) = menu {
            let theme = rivu_ui::theme::palette(cx);
            surface = surface.child(
                self.0
                    .update(cx, |app, cx| app.render_panel_menu(menu, theme, window, cx)),
            );
        }
        surface
    }
}

fn fixture(cx: &mut TestAppContext) -> Fixture {
    cx.executor().allow_parking();
    cx.update(rivu_ui::input::Input::init);
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    rivu_format::config::Config {
        volume: 0.0,
        ffmpeg_enabled: false,
        mpris_enabled: false,
        ..Default::default()
    }
    .save_fixture(&config)
    .unwrap();
    rivu::test_support::catalog::seed_playlist_targets(directory.path(), 1).unwrap();
    let runtime = CoreRuntime::start(directory.path(), &config).unwrap();
    assert!(
        futures::executor::block_on(runtime.handle.request_ack(Command::Enqueue {
            track_ids: vec![1, 1, 1],
        }))
        .ack
        .is_success()
    );
    let rows = queue_rows(&runtime);
    let save = LayoutPersistence::new(LoadedLayouts::load(directory.path().join("layouts.json")));
    let handle = runtime.handle.clone();
    let save_handle = save.handle();
    let artwork = Arc::new(rivu_metadata::artwork::ArtworkManager::new(
        directory.path(),
    ));
    let window = cx.update(|cx| {
        cx.open_window(
            gpui::WindowOptions {
                show: false,
                ..Default::default()
            },
            move |window, cx| {
                cx.new(|cx| {
                    let mut app = WindowView::new(
                        handle,
                        save_handle,
                        artwork,
                        Default::default(),
                        Arc::new(rivu_presentation::i18n::Localizer::new("en-US")),
                        window,
                        cx,
                    );
                    app.handle.set_wakeup(|| {});
                    app.window_visible = false;
                    app.reconcile_feature_demand(cx);
                    app
                })
            },
        )
        .unwrap()
    });
    let app = window.root(cx).unwrap();
    app.update(cx, |app, cx| {
        app.queue.update(cx, |queue, _| {
            queue.model.rows.begin(0, 20);
            queue.model.rows.accept(rivu_protocol::model::Page {
                total: rows.len(),
                rows: rows.clone(),
            });
            queue
                .model
                .select(QueueIdentity::from(rows.last().unwrap()), false);
        });
    });
    let root = app.clone();
    cx.update_window(window.into(), |_, window, cx| {
        window.replace_root(cx, |_, cx| {
            cx.observe(&root, |_, _, cx| cx.notify()).detach();
            let queue = root.read(cx).queue.clone();
            cx.observe(&queue, |_, _, cx| cx.notify()).detach();
            QueueWindow(root)
        });
    })
    .unwrap();
    Fixture {
        app,
        window: window.into(),
        runtime,
        _save: save,
        _directory: directory,
        rows,
    }
}

fn queue_rows(runtime: &CoreRuntime) -> Vec<QueueRow> {
    // Commands and queries use independent queues. The ACK fences preceding actions.
    assert!(
        futures::executor::block_on(runtime.handle.request_ack(Command::Volume { value: 0.0 }))
            .ack
            .is_success()
    );
    let ViewResponse::QueuePage(page) = runtime
        .handle
        .query(Query::QueuePage {
            query: None,
            offset: 0,
            limit: 20,
        })
        .result
        .unwrap()
    else {
        panic!("expected queue page");
    };
    page.rows
}

fn draw(window: AnyWindowHandle, cx: &mut TestAppContext) {
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    })
    .unwrap();
}

fn click(window: AnyWindowHandle, position: gpui::Point<gpui::Pixels>, cx: &mut TestAppContext) {
    cx.update_window(window, |_, window, cx| {
        window.dispatch_event(
            gpui::MouseDownEvent {
                button: gpui::MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            gpui::MouseUpEvent {
                button: gpui::MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    })
    .unwrap();
}

#[gpui::test]
fn rejected_delete_and_clear_keep_selected_occurrence(cx: &mut TestAppContext) {
    for clear in [false, true] {
        let mut fixture = fixture(cx);
        let selected = QueueIdentity::from(fixture.rows.last().unwrap());
        if clear {
            fixture.app.update(cx, |app, cx| {
                app.panel_menu = Some(PanelMenu {
                    panel_id: 1,
                    tab_bar_visible: true,
                    position: point(px(10.), px(10.)),
                    page: PanelMenuPage::ConfirmClearQueue,
                });
                cx.notify();
            });
            draw(fixture.window, cx);
        }
        fixture.runtime.disconnect_frontend();
        if clear {
            let confirm = VisualTestContext::from_window(fixture.window, cx)
                .debug_bounds("confirm-clear-queue")
                .unwrap()
                .center();
            click(fixture.window, confirm, cx);
        } else {
            fixture.app.update(cx, |app, cx| {
                app.remove_focused_queue(crate::gui::ListFocus::Queue, cx);
            });
        }
        fixture.app.read_with(cx, |app, cx| {
            assert!(app.error.is_some());
            let queue = app.queue.read(cx);
            assert_eq!(queue.model.ordered_ids(), [selected.id]);
            assert_eq!(queue.model.active(), Some(selected));
            assert_eq!(queue.model.anchor, Some(selected));
            assert_eq!(
                queue.model.rows.get(selected.offset).unwrap().id,
                selected.id
            );
        });
    }
}

#[gpui::test]
fn old_queue_frame_removes_stable_id_but_rejects_positional_move(cx: &mut TestAppContext) {
    for remove in [false, true] {
        let fixture = fixture(cx);
        fixture.app.update(cx, |app, cx| {
            app.queue.update(cx, |queue, _| queue.model.clear());
        });
        let selected = fixture.rows.last().unwrap().id;
        draw(fixture.window, cx);
        // Repeated action selectors resolve to the last rendered row.
        let selector = if remove {
            "queue-entry-remove"
        } else {
            "queue-entry-up"
        };
        let action = VisualTestContext::from_window(fixture.window, cx)
            .debug_bounds(selector)
            .unwrap()
            .center();
        // A valid old row remains the same occurrence after an unrelated append.
        assert!(
            futures::executor::block_on(
                fixture
                    .runtime
                    .handle
                    .request_ack(Command::Enqueue { track_ids: vec![1] })
            )
            .ack
            .is_success()
        );
        click(fixture.window, action, cx);
        let rows = queue_rows(&fixture.runtime);
        if remove {
            assert_eq!(rows.len(), 3);
            assert!(rows.iter().all(|row| row.id != selected));
        } else {
            assert_eq!(rows.len(), 4);
            assert_eq!(rows[fixture.rows.len() - 1].id, selected);
            assert!(fixture.app.read_with(cx, |app, _| app.error.is_some()));
        }
    }
}
