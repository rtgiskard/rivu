//! Shared services, not shared business state, for window-local features.
use super::{scrolling::ScrollViews, snapshot::SnapshotSource};
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
