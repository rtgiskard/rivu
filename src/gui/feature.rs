//! Shared services, not shared business state, for window-local features.
use super::scrolling::ScrollViews;
use crate::{core::CoreHandle, i18n::Localizer, projection::GuiSnapshot};
use gpui::SharedString;
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

#[derive(Clone)]
pub(super) struct FeatureContext {
    pub(super) snapshots: Rc<SnapshotSource>,
    pub(super) i18n: Arc<Localizer>,
    pub(super) labels: Rc<HashMap<String, SharedString>>,
    pub(super) scroll: Rc<RefCell<ScrollViews>>,
}

impl FeatureContext {
    pub(super) fn tr(&self, id: &str) -> SharedString {
        self.labels
            .get(id)
            .expect("cached localized GUI text")
            .clone()
    }

    pub(super) fn snapshot(&self) -> Rc<GuiSnapshot> {
        self.snapshots.read()
    }

    pub(super) fn handle(&self) -> &CoreHandle {
        self.snapshots.handle()
    }
}

/// One UI-thread snapshot cache shared by feature owners and the window.
///
/// Core remains authoritative. Reading after a query completion may refresh the
/// cache, but never creates another Core subscription or a feature-state copy.
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
        if let Some(snapshot) = self.handle.gui_snapshot_if_changed(current.system.revision) {
            *current = Rc::new(snapshot);
        }
        Rc::clone(&current)
    }
}
