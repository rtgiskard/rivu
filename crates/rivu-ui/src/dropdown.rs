/// A candidate shown by a dropdown or autocomplete popup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropdownItem<T> {
    pub value: T,
    pub label: String,
}

impl<T> DropdownItem<T> {
    pub fn new(value: T, label: impl Into<String>) -> Self {
        Self {
            value,
            label: label.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DropdownState<T> {
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
    pub fn open(&mut self, items: impl IntoIterator<Item = DropdownItem<T>>) {
        self.items = items.into_iter().collect();
        self.selected = 0;
        self.open = !self.items.is_empty();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.selected = 0;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn filtered(&self) -> impl Iterator<Item = (usize, &DropdownItem<T>)> {
        self.items.iter().enumerate()
    }

    pub fn filtered_item(&self, index: usize) -> Option<(usize, &DropdownItem<T>)> {
        self.items.get(index).map(|item| (index, item))
    }

    pub fn move_next(&mut self) {
        let count = self.items.len();
        if count > 0 {
            self.selected = (self.selected + 1) % count;
        }
    }

    pub fn move_previous(&mut self) {
        let count = self.items.len();
        if count > 0 {
            self.selected = self.selected.checked_sub(1).unwrap_or(count - 1);
        }
    }

    pub fn selected_index(&self) -> Option<usize> {
        (!self.items.is_empty()).then_some(self.selected)
    }

    pub fn select_index(&mut self, index: usize) {
        self.selected = index.min(self.items.len().saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::{DropdownItem, DropdownState};

    #[test]
    fn empty_open_does_not_leave_a_zero_height_popup() {
        let mut state = DropdownState::<u8>::default();
        state.open([]);
        assert!(!state.is_open());
        assert_eq!(state.selected_index(), None);
        state.move_next();
        state.move_previous();
        state.select_index(usize::MAX);
        assert_eq!(state.selected_index(), None);
    }

    #[test]
    fn keyboard_selection_wraps_and_clamps() {
        let mut state = DropdownState::default();
        state.open([
            DropdownItem::new(1, "one"),
            DropdownItem::new(2, "two"),
            DropdownItem::new(3, "three"),
        ]);
        state.move_previous();
        assert_eq!(state.selected_index(), Some(2));
        state.move_next();
        assert_eq!(state.selected_index(), Some(0));
        state.select_index(99);
        assert_eq!(state.selected_index(), Some(2));
    }

    #[test]
    fn replacing_choices_resets_selection_and_empty_choices_close_popup() {
        let mut state = DropdownState::default();
        state.open([
            DropdownItem::new(10, "ten"),
            DropdownItem::new(20, "twenty"),
        ]);
        state.move_next();
        state.close();
        assert!(!state.is_open());

        state.open([DropdownItem::new(30, "thirty")]);
        assert!(state.is_open());
        assert_eq!(state.selected_index(), Some(0));
        assert_eq!(state.filtered_item(0).map(|(_, item)| item.value), Some(30));
        assert!(state.filtered_item(1).is_none());
        state.move_next();
        state.move_previous();
        assert_eq!(state.selected_index(), Some(0));

        state.open([]);
        assert!(!state.is_open());
        assert_eq!(state.selected_index(), None);
        assert_eq!(state.filtered().count(), 0);
    }
}
