//! One UI-thread snapshot cache shared by feature owners and the window.
//!
//! Core remains authoritative. Reading after a query completion may refresh the
//! cache, but never creates another Core subscription or a feature-state copy.
use crate::{core::CoreHandle, projection::GuiSnapshot};
use std::{cell::RefCell, rc::Rc};

pub(super) struct SnapshotSource {
    handle: CoreHandle,
    current: RefCell<Rc<GuiSnapshot>>,
}

impl SnapshotSource {
    pub(super) fn new(handle: CoreHandle) -> Self {
        let current = Rc::new(handle.gui_snapshot());
        Self {
            handle,
            current: RefCell::new(current),
        }
    }

    pub(super) fn handle(&self) -> &CoreHandle {
        &self.handle
    }

    pub(super) fn read(&self) -> Rc<GuiSnapshot> {
        let mut current = self.current.borrow_mut();
        if self.handle.revision() != current.system.revision {
            *current = Rc::new(self.handle.gui_snapshot());
        }
        Rc::clone(&current)
    }
}
