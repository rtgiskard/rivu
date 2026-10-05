/// A candidate shown by a dropdown or autocomplete popup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DropdownItem<T> {
    pub(crate) value: T,
    pub(crate) label: String,
}

impl<T> DropdownItem<T> {
    pub(crate) fn new(value: T, label: impl Into<String>) -> Self {
        Self {
            value,
            label: label.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DropdownState<T> {
    items: Vec<DropdownItem<T>>,
    selected: usize,
    open: bool,
}

impl<T> Default for DropdownState<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            selected: 0,
            open: false,
        }
    }
}

impl<T> DropdownState<T> {
    pub(crate) fn open(&mut self, items: impl IntoIterator<Item = DropdownItem<T>>) {
        self.items = items.into_iter().collect();
        self.selected = 0;
        self.open = true;
        self.normalize_selection();
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
        self.selected = 0;
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn filtered(&self) -> impl Iterator<Item = (usize, &DropdownItem<T>)> {
        self.items.iter().enumerate()
    }

    pub(crate) fn filtered_item(&self, index: usize) -> Option<(usize, &DropdownItem<T>)> {
        self.filtered().nth(index)
    }

    pub(crate) fn move_next(&mut self) {
        let count = self.filtered().count();
        if count > 0 {
            self.selected = (self.selected + 1) % count;
        }
    }

    pub(crate) fn move_previous(&mut self) {
        let count = self.filtered().count();
        if count > 0 {
            self.selected = self.selected.checked_sub(1).unwrap_or(count - 1);
        }
    }

    #[cfg(test)]
    pub(crate) fn selected(&self) -> Option<&T> {
        self.filtered()
            .nth(self.selected)
            .map(|(_, item)| &item.value)
    }

    pub(crate) fn selected_index(&self) -> Option<usize> {
        self.filtered().nth(self.selected).map(|(index, _)| index)
    }

    pub(crate) fn select_index(&mut self, index: usize) {
        let selected = {
            self.filtered()
                .position(|(item_index, _)| item_index == index)
                .unwrap_or(0)
        };
        self.selected = selected;
        self.normalize_selection();
    }

    fn normalize_selection(&mut self) {
        let count = self.filtered().count();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(count - 1);
        }
    }
}
