//! Root input, keyboard-surface routing, and top-level composition.

pub(super) mod panels;
mod shortcuts;
mod view;

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyboardSurface {
    Menu(MenuSurface),
    Dialog(DialogSurface),
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuSurface {
    Playlist,
    Content(list_interaction::ContentMenuPage),
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
    scope: components::FocusScope,
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
            scope: components::FocusScope::new(cx),
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

    pub(super) fn scope(&self) -> &components::FocusScope {
        &self.scope
    }

    pub(super) fn keyboard_surface(&self) -> Option<KeyboardSurface> {
        self.keyboard_surface
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
            None => {
                if let Some(origin) = self.restore.take() {
                    if self.layout_focus.contains(&origin, window) {
                        origin.focus(window, cx);
                    } else {
                        self.layout_focus.focus(window, cx);
                    }
                }
                None
            }
        }
    }

    fn focus_surface(&self, epoch: u64, window: &mut Window, cx: &mut App) {
        if self.focus_epoch != epoch {
            return;
        }
        let Some(surface) = self.keyboard_surface else {
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

impl GuiApp {
    pub(super) fn menu_surface(&self, surface: gpui::Stateful<Div>) -> gpui::Stateful<Div> {
        if matches!(
            self.current_keyboard_surface(),
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

    fn current_keyboard_surface(&self) -> Option<KeyboardSurface> {
        if self.shell.help().is_some() {
            return Some(KeyboardSurface::Dialog(DialogSurface::KeyboardHelp));
        }
        if self.manual_confirmation_open() {
            return Some(KeyboardSurface::Dialog(DialogSurface::MetadataConfirmation));
        }
        if self.shell.is_open(shell::Surface::About) {
            return Some(KeyboardSurface::Dialog(DialogSurface::About));
        }
        if self.duplicate_review.open {
            return Some(KeyboardSurface::Dialog(DialogSurface::DuplicateReview));
        }
        if self.shell.is_open(shell::Surface::Settings) {
            return Some(KeyboardSurface::Settings);
        }
        if self.playlist_editor.is_some() {
            return Some(KeyboardSurface::Dialog(DialogSurface::PlaylistEditor));
        }
        if let Some(action) = &self.playlist_action {
            return Some(if matches!(action, playlists::PlaylistAction::Menu(..)) {
                KeyboardSurface::Menu(MenuSurface::Playlist)
            } else {
                KeyboardSurface::Dialog(DialogSurface::PlaylistDelete)
            });
        }
        if self.queue_replace.is_some() {
            return Some(KeyboardSurface::Dialog(DialogSurface::QueueReplacement));
        }
        if self.shell.is_open(shell::Surface::Layouts) {
            return Some(KeyboardSurface::Dialog(if self.layout_action.is_some() {
                DialogSurface::LayoutAction
            } else {
                DialogSurface::LayoutManager
            }));
        }
        if let Some(menu) = &self.content_menu {
            return Some(KeyboardSurface::Menu(MenuSurface::Content(
                menu.keyboard_page(),
            )));
        }
        if let Some(menu) = &self.panel_menu {
            return Some(KeyboardSurface::Menu(MenuSurface::Panel(menu.page)));
        }
        if self.shell.is_open(shell::Surface::AppMenu) {
            return Some(KeyboardSurface::Menu(MenuSurface::App));
        }
        if self.shell.is_open(shell::Surface::PanelCatalog) {
            return Some(KeyboardSurface::Menu(MenuSurface::PanelCatalog));
        }
        None
    }

    fn synchronize_keyboard_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let surface = self.current_keyboard_surface();
        let Some(epoch) = self.shell.synchronize(surface, window, cx) else {
            return;
        };
        let entity = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = entity.update(cx, |this, cx| {
                this.shell.focus_surface(epoch, window, cx);
            });
        });
    }

    pub(super) fn menu_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        match key {
            "down" | "up" => self.shell.scope().advance(key == "up", window, cx),
            "escape" | "left" => {
                if self.content_menu.is_some() {
                    self.back_content_menu(cx);
                } else if let Some(menu) = &mut self.panel_menu {
                    if matches!(menu.page, PanelMenuPage::Main) {
                        self.panel_menu = None;
                    } else {
                        menu.page = PanelMenuPage::Main;
                    }
                } else if self.playlist_action.is_some() {
                    self.playlist_action = None;
                } else {
                    self.shell.dismiss(shell::Surface::AppMenu);
                    self.shell.dismiss(shell::Surface::PanelCatalog);
                }
                cx.notify();
            }
            _ => {}
        }
    }
}
