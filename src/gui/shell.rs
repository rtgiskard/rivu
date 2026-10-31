//! Root input, keyboard-surface routing, and top-level composition.

mod notification;
pub(super) mod panels;
mod shortcuts;
mod view;

use crate::gui::window::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyboardSurface {
    Menu(MenuSurface),
    Dialog(DialogSurface),
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuSurface {
    Playlist,
    Content(coordination::content_menu::ContentMenuPage),
    Panel(PanelMenuPage),
    App,
    PanelCatalog,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DialogSurface {
    KeyboardHelp,
    MetadataConfirmation,
    About,
    DuplicateReview,
    PlaylistEditor,
    PlaylistDelete,
    QueueReplacement,
    LayoutManager,
    LayoutAction,
}

/// Mutually exclusive shell surfaces. Feature-owned confirmations may cover one
/// without destroying its draft (for example duplicate review over Settings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Surface {
    AppMenu,
    PanelCatalog,
    Settings,
    About,
    Layouts,
}

pub(super) struct WindowShell {
    surface: Option<Surface>,
    layout_focus: FocusHandle,
    scope: rivu_ui::FocusScope,
    keyboard_surface: Option<KeyboardSurface>,
    restore: Option<FocusHandle>,
    focus_epoch: u64,
    help: Option<crate::keyboard::Context>,
    help_scroll: std::rc::Rc<ScrollHandle>,
}

impl WindowShell {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            surface: None,
            layout_focus: cx.focus_handle(),
            scope: rivu_ui::FocusScope::new(cx),
            keyboard_surface: None,
            restore: None,
            focus_epoch: 0,
            help: None,
            help_scroll: std::rc::Rc::new(ScrollHandle::new()),
        }
    }

    pub(super) fn is_open(&self, surface: Surface) -> bool {
        self.surface == Some(surface)
    }

    pub(super) fn open(&mut self, surface: Surface) {
        if self.surface != Some(surface) {
            self.surface = Some(surface);
            self.focus_epoch = self.focus_epoch.wrapping_add(1);
        }
    }

    pub(super) fn dismiss(&mut self, surface: Surface) {
        if self.is_open(surface) {
            self.surface = None;
            self.focus_epoch = self.focus_epoch.wrapping_add(1);
        }
    }

    pub(super) fn toggle(&mut self, surface: Surface) {
        if self.is_open(surface) {
            self.dismiss(surface);
        } else {
            self.open(surface);
        }
    }

    pub(super) fn layout_focus(&self) -> &FocusHandle {
        &self.layout_focus
    }

    pub(super) fn scope(&self) -> &rivu_ui::FocusScope {
        &self.scope
    }

    pub(super) fn help(&self) -> Option<crate::keyboard::Context> {
        self.help
    }

    pub(super) fn help_scroll(&self) -> &std::rc::Rc<ScrollHandle> {
        &self.help_scroll
    }

    pub(super) fn open_help(&mut self, context: crate::keyboard::Context) {
        self.help = Some(context);
        self.help_scroll.set_offset(point(px(0.), px(0.)));
        self.focus_epoch = self.focus_epoch.wrapping_add(1);
    }

    pub(super) fn close_help(&mut self) {
        self.help = None;
        self.focus_epoch = self.focus_epoch.wrapping_add(1);
    }

    fn capture_origin(&mut self, window: &Window, cx: &App) {
        self.restore = window.focused(cx);
    }

    fn native_focus_frame_is_current(
        &self,
        epoch: u64,
        rendered: Option<KeyboardSurface>,
        live: Option<KeyboardSurface>,
    ) -> bool {
        self.focus_epoch == epoch && rendered == live
    }

    /// Return a deferred focus ticket; stale callbacks cannot focus a replacement
    /// surface, even if it has the same keyboard category as its predecessor.
    fn synchronize(
        &mut self,
        surface: Option<KeyboardSurface>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<u64> {
        if surface == self.keyboard_surface {
            return None;
        }
        if self.keyboard_surface.is_none() && surface.is_some() && self.restore.is_none() {
            self.restore = window.focused(cx);
        }
        self.keyboard_surface = surface;
        self.focus_epoch = self.focus_epoch.wrapping_add(1);
        match surface {
            Some(KeyboardSurface::Settings) => None,
            Some(_) => Some(self.focus_epoch),
            // Validate the origin after layout edits have reached the rendered tree.
            None => Some(self.focus_epoch),
        }
    }

    fn focus_surface(&mut self, epoch: u64, window: &mut Window, cx: &mut App) {
        if self.focus_epoch != epoch {
            return;
        }
        let Some(surface) = self.keyboard_surface else {
            let origin = self.restore.take();
            if let Some(origin) = origin.filter(|origin| self.layout_focus.contains(origin, window))
            {
                origin.focus(window, cx);
            } else {
                self.layout_focus.focus(window, cx);
            }
            return;
        };
        let menu = matches!(surface, KeyboardSurface::Menu(..));
        if menu
            || self.scope.handle().is_focused(window)
            || !self.scope.handle().contains_focused(window, cx)
        {
            self.scope.handle().focus(window, cx);
            if !menu {
                self.scope.advance(false, window, cx);
            }
        }
    }
}

impl WindowView {
    pub(super) fn menu_surface(
        &self,
        surface: gpui::Stateful<Div>,
        cx: &App,
    ) -> gpui::Stateful<Div> {
        if matches!(
            self.current_keyboard_surface(cx),
            Some(KeyboardSurface::Menu(..))
        ) {
            self.shell.scope().menu_surface(surface)
        } else {
            surface
        }
    }

    pub(super) fn dialog_surface(&self, surface: gpui::Stateful<Div>) -> gpui::Stateful<Div> {
        if self.manual_confirmation_open() {
            surface
        } else {
            self.shell.scope().surface(surface)
        }
    }

    /// Leaving an editor must not leave the surface that still owns the keyboard.
    pub(super) fn focus_keyboard_surface(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.current_keyboard_surface(cx) {
            Some(KeyboardSurface::Settings) => {
                self.settings.read(cx).focus_handle(cx).focus(window, cx);
            }
            Some(_) => self.shell.scope().handle().focus(window, cx),
            None => self.focus_layout(window, cx),
        }
    }

    /// Bottom-to-top composition order is also the authority for input ownership.
    /// Lower surfaces remain mounted so confirmations do not discard their drafts.
    fn surface_layers(&self, cx: &App) -> impl DoubleEndedIterator<Item = KeyboardSurface> + use<> {
        use DialogSurface as D;
        use KeyboardSurface::{Dialog, Menu, Settings};
        use MenuSurface as M;
        [
            match self.shell.surface {
                Some(Surface::AppMenu) => Some(Menu(M::App)),
                Some(Surface::PanelCatalog) if !self.workspace.layouts.current().locked => {
                    Some(Menu(M::PanelCatalog))
                }
                _ => None,
            },
            self.panel_menu.map(|menu| Menu(M::Panel(menu.page))),
            self.content_menu
                .as_ref()
                .map(|menu| Menu(M::Content(menu.keyboard_page()))),
            self.playlists.read(cx).action().map(|action| {
                if matches!(action, playlists::PlaylistAction::Menu(..)) {
                    Menu(M::Playlist)
                } else {
                    Dialog(D::PlaylistDelete)
                }
            }),
            self.queue_replace
                .as_ref()
                .map(|_| Dialog(D::QueueReplacement)),
            self.playlists
                .read(cx)
                .editor_open()
                .then_some(Dialog(D::PlaylistEditor)),
            self.shell.is_open(Surface::Layouts).then_some(Dialog(
                if self.layout_action.is_some() {
                    D::LayoutAction
                } else {
                    D::LayoutManager
                },
            )),
            self.shell.is_open(Surface::Settings).then_some(Settings),
            self.duplicates
                .read(cx)
                .is_open()
                .then_some(Dialog(D::DuplicateReview)),
            self.shell
                .is_open(Surface::About)
                .then_some(Dialog(D::About)),
            self.shell.help().map(|_| Dialog(D::KeyboardHelp)),
            self.manual_confirmation_open()
                .then_some(Dialog(D::MetadataConfirmation)),
        ]
        .into_iter()
        .flatten()
    }

    pub(super) fn current_keyboard_surface(&self, cx: &App) -> Option<KeyboardSurface> {
        self.surface_layers(cx).next_back()
    }

    fn synchronize_keyboard_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let surface = self.current_keyboard_surface(cx);
        let focus_ticket = self.shell.synchronize(surface, window, cx);
        let frame_epoch = self.shell.focus_epoch;
        let confirmation = self.manual_confirmation_id();
        let owner = cx.weak_entity();
        window.set_native_focus_guard(move |focus, window, cx| {
            let Some(owner) = owner.upgrade() else {
                return false;
            };
            let owner = owner.read(cx);
            if !owner.shell.native_focus_frame_is_current(
                frame_epoch,
                surface,
                owner.current_keyboard_surface(cx),
            ) || owner.manual_confirmation_id() != confirmation
            {
                return false;
            }
            match surface {
                Some(KeyboardSurface::Settings) => owner
                    .settings
                    .read(cx)
                    .focus_handle(cx)
                    .contains(focus, window),
                Some(_) => owner.shell.scope().handle().contains(focus, window),
                None => true,
            }
        });
        let Some(epoch) = focus_ticket else {
            return;
        };
        let focus_before_restore = window.focused(cx);
        let entity = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = entity.update(cx, |this, cx| {
                if this.shell.native_focus_frame_is_current(
                    epoch,
                    surface,
                    this.current_keyboard_surface(cx),
                ) && this.manual_confirmation_id() == confirmation
                {
                    if surface.is_none()
                        && window.focused(cx) != focus_before_restore
                        && this.shell.layout_focus().contains_focused(window, cx)
                    {
                        // A newer workspace choice supersedes restoration.
                        this.shell.restore = None;
                        return;
                    }
                    this.shell.focus_surface(epoch, window, cx);
                    if matches!(
                        surface,
                        Some(KeyboardSurface::Menu(MenuSurface::Content(_)))
                    ) && let Some(menu) = &mut this.content_menu
                    {
                        menu.focus_playlist_return(window, cx);
                    }
                }
            });
        });
    }

    pub(super) fn menu_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        match key {
            "down" | "up" => self.shell.scope().advance(key == "up", window, cx),
            "escape" | "left" => {
                match self.current_keyboard_surface(cx) {
                    Some(KeyboardSurface::Menu(MenuSurface::Content(_))) => {
                        self.back_content_menu(cx);
                    }
                    Some(KeyboardSurface::Menu(MenuSurface::Panel(_))) => {
                        let menu = self.panel_menu.as_mut().expect("live panel menu");
                        if matches!(menu.page, PanelMenuPage::Main) {
                            self.panel_menu = None;
                        } else {
                            menu.page = PanelMenuPage::Main;
                        }
                    }
                    Some(KeyboardSurface::Menu(MenuSurface::Playlist)) => {
                        self.playlists
                            .update(cx, |playlists, cx| playlists.clear_action(cx));
                    }
                    Some(KeyboardSurface::Menu(MenuSurface::App)) => {
                        self.shell.dismiss(Surface::AppMenu);
                    }
                    Some(KeyboardSurface::Menu(MenuSurface::PanelCatalog)) => {
                        self.shell.dismiss(Surface::PanelCatalog);
                    }
                    _ => return,
                }
                cx.notify();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[gpui::test]
    fn native_focus_publication_expires_before_replacement_render(cx: &mut gpui::TestAppContext) {
        let mut shell = cx.update(WindowShell::new);
        let window = cx.add_window(|_, _| gpui::EmptyView).into();
        let surfaces = [
            Some(KeyboardSurface::Settings),
            Some(KeyboardSurface::Dialog(DialogSurface::MetadataConfirmation)),
            Some(KeyboardSurface::Dialog(DialogSurface::About)),
            Some(KeyboardSurface::Menu(MenuSurface::App)),
            None,
        ];
        cx.update_window(window, |_, window, cx| {
            let mut previous = None;
            for surface in surfaces {
                let epoch = shell.focus_epoch;
                assert!(!shell.native_focus_frame_is_current(epoch, previous, surface));
                let ticket = shell.synchronize(surface, window, cx);
                assert!(shell.native_focus_frame_is_current(shell.focus_epoch, surface, surface));
                assert!(!shell.native_focus_frame_is_current(epoch, previous, surface));
                if surface == Some(KeyboardSurface::Settings) {
                    assert!(
                        ticket.is_none(),
                        "Settings keeps its independent focus owner"
                    );
                }
                previous = surface;
            }
            shell.open(Surface::Settings);
            let surface = Some(KeyboardSurface::Settings);
            shell.synchronize(surface, window, cx);
            let epoch = shell.focus_epoch;
            shell.dismiss(Surface::Settings);
            shell.open(Surface::Settings);
            assert!(
                !shell.native_focus_frame_is_current(epoch, surface, surface),
                "close/reopen of the same surface invalidates its published native focus"
            );
        })
        .unwrap();
    }

    struct FocusRestoreSurface {
        layout: FocusHandle,
        origin: FocusHandle,
        scope: rivu_ui::FocusScope,
        show_origin: bool,
        show_dialog: bool,
    }

    impl Render for FocusRestoreSurface {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("restore-layout")
                .track_focus(&self.layout)
                .when(self.show_origin, |view| {
                    view.child(div().id("origin").track_focus(&self.origin))
                })
                .when(self.show_dialog, |view| {
                    view.child(self.scope.surface(div().id("dialog")))
                })
        }
    }

    #[gpui::test]
    fn surface_exit_restores_only_an_origin_in_the_surviving_layout(cx: &mut gpui::TestAppContext) {
        for keep_origin in [true, false] {
            let mut shell = cx.update(WindowShell::new);
            let origin = cx.update(|cx| cx.focus_handle());
            let layout = shell.layout_focus().clone();
            let scope = shell.scope().clone();
            let window = cx.add_window({
                let origin = origin.clone();
                let layout = layout.clone();
                move |_, _| FocusRestoreSurface {
                    layout,
                    origin,
                    scope,
                    show_origin: true,
                    show_dialog: false,
                }
            });
            let handle = window.into();
            cx.update_window(handle, |_, window, cx| {
                window.draw(cx).clear(cx);
                origin.focus(window, cx);
                shell.synchronize(
                    Some(KeyboardSurface::Dialog(DialogSurface::LayoutManager)),
                    window,
                    cx,
                );
            })
            .unwrap();
            window
                .update(cx, |view, _, cx| {
                    view.show_dialog = true;
                    cx.notify();
                })
                .unwrap();
            cx.update_window(handle, |_, window, cx| {
                window.draw(cx).clear(cx);
                shell.focus_surface(shell.focus_epoch, window, cx);
            })
            .unwrap();
            let ticket = cx
                .update_window(handle, |_, window, cx| {
                    shell.synchronize(None, window, cx).unwrap()
                })
                .unwrap();
            window
                .update(cx, |view, _, cx| {
                    view.show_origin = keep_origin;
                    view.show_dialog = false;
                    cx.notify();
                })
                .unwrap();
            cx.update_window(handle, |_, window, cx| {
                window.draw(cx).clear(cx);
                shell.focus_surface(ticket, window, cx);
                assert_eq!(
                    window.focused(cx),
                    Some(if keep_origin {
                        origin.clone()
                    } else {
                        layout.clone()
                    })
                );
                // A delayed close cannot reclaim focus after a replacement opens.
                shell.synchronize(
                    Some(KeyboardSurface::Dialog(DialogSurface::About)),
                    window,
                    cx,
                );
                let newer = window.focused(cx);
                shell.focus_surface(ticket, window, cx);
                assert_eq!(window.focused(cx), newer);
            })
            .unwrap();
        }
    }
}
