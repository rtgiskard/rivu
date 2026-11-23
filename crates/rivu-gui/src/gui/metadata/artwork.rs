//! Metadata artwork uses the existing per-window image registry and one physical loader.

use super::Metadata;
use crate::gui::artwork::owned_artwork_source;
use gpui::Context;
use rivu_protocol::model::ArtworkSource;
use std::sync::Arc;

impl Metadata {
    pub(super) fn prepare_artwork(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            // Shared acquisition may finish, but hidden consumers own no demand.
            self.artwork_loader.retain_negative(None);
            return;
        }
        let now = cx.background_executor().now();
        let source = self
            .metadata
            .display_track_at(now)
            .map(crate::artwork_source);
        let demand = self.artwork_loader.resolve(&self.artwork_manager, source);
        self.artwork_view.replace(demand.artwork, cx);
        if let Some(source) = demand.load {
            self.start_artwork(source, cx);
        }
    }

    fn start_artwork(&mut self, source: ArtworkSource, cx: &mut Context<Self>) {
        let manager = Arc::clone(&self.artwork_manager);
        cx.spawn(async move |this, cx| {
            let (source, loaded) = cx
                .background_executor()
                .spawn(async move {
                    let loaded = manager.load(owned_artwork_source(&source));
                    (source, loaded)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some(identity) = this.artwork_loader.finish(owned_artwork_source(&source))
                else {
                    return;
                };
                // A completion may warm the cache, never revive a hidden consumer.
                if this.visible
                    && this
                        .metadata
                        .display_track_at(cx.background_executor().now())
                        .is_some_and(|track| identity.matches(crate::artwork_source(track)))
                {
                    this.artwork_loader
                        .remember_result(identity, loaded.is_some());
                    this.artwork_view.replace(loaded, cx);
                    cx.notify();
                } else {
                    this.prepare_artwork(cx);
                }
            });
        })
        .detach();
    }
}
