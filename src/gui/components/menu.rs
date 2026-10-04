#![allow(dead_code)]
/// State for a keyboard-navigable context menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuItem {
    pub(crate) label: String,
    pub(crate) enabled: bool,
}

impl MenuItem {
    pub(crate) fn enabled(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            enabled: true,
        }
    }

    pub(crate) fn disabled(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            enabled: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ContextMenuState {
    open: bool,
    items: Vec<MenuItem>,
    selected: Option<usize>,
}

impl ContextMenuState {
    pub(crate) fn open(&mut self, items: impl IntoIterator<Item = MenuItem>) {
        self.items = items.into_iter().collect();
        self.open = true;
        self.selected = self.first_enabled();
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
        self.selected = None;
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }
    pub(crate) fn items(&self) -> &[MenuItem] {
        &self.items
    }
    pub(crate) fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub(crate) fn move_next(&mut self) {
        self.move_enabled(1);
    }

    pub(crate) fn move_previous(&mut self) {
        self.move_enabled(-1);
    }

    pub(crate) fn choose(&self) -> Option<usize> {
        self.selected.filter(|&index| self.items[index].enabled)
    }

    fn first_enabled(&self) -> Option<usize> {
        self.items.iter().position(|item| item.enabled)
    }

    fn move_enabled(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let start = self.selected.unwrap_or(0) as isize;
        for offset in 1..=self.items.len() {
            let index =
                (start + delta * offset as isize).rem_euclid(self.items.len() as isize) as usize;
            if self.items[index].enabled {
                self.selected = Some(index);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_skips_disabled_items() {
        let mut menu = ContextMenuState::default();
        menu.open([
            MenuItem::enabled("first"),
            MenuItem::disabled("separator"),
            MenuItem::enabled("last"),
        ]);
        assert_eq!(menu.selected(), Some(0));
        menu.move_next();
        assert_eq!(menu.selected(), Some(2));
        menu.move_previous();
        assert_eq!(menu.selected(), Some(0));
    }
}
