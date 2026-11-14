//! Metadata artwork uses the existing per-window image registry and one physical loader.

use super::Metadata;
use crate::gui::artwork::owned_artwork_source;
use gpui::Context;
use rivu_metadata::artwork::ArtworkIdentity;
use rivu_protocol::model::ArtworkSource;
use std::sync::Arc;

impl Metadata {
    pub(super) fn prepare_artwork(&mut self, cx: &mut Context<Self>) {
        self.artwork_loader.retain_negative(
            self.metadata
                .track()
                .filter(|_| self.visible)
                .map(crate::artwork_source),
        );
        if !self.visible {
            // Shared acquisition may finish, but hidden consumers own no demand.
            return;
        }
        let cached = self.metadata.track().and_then(|track| {
            self.artwork_manager
                .cached_result(crate::artwork_source(track))
        });
        if let Some(result) = &cached
            && let Some(track) = self.metadata.track()
        {
            self.artwork_loader.remember_result(
                ArtworkIdentity::new(crate::artwork_source(track)),
                result.is_some(),
            );
        }
        let cold = cached.is_none();
        self.artwork_view.replace(cached.flatten(), cx);
        if let Some(track) = self.metadata.track().filter(|_| cold)
            && let Some(source) = self.artwork_loader.prepare(crate::artwork_source(track))
        {
            self.start_artwork(source, cx);
        }
    }

    fn start_artwork(&mut self, source: ArtworkSource, cx: &mut Context<Self>) {
        let identity = ArtworkIdentity::new(owned_artwork_source(&source));
        let manager = Arc::clone(&self.artwork_manager);
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { manager.load(owned_artwork_source(&source)) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.artwork_loader.finish(&identity) {
                    return;
                }
                // A completion may warm the cache, never revive a hidden consumer.
                if this.visible
                    && this
                        .metadata
                        .track()
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
