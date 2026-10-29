//! Metadata artwork uses the existing per-window image registry and one physical loader.

use super::Metadata;
use crate::artwork::ArtworkIdentity;
use gpui::Context;
use rivu_protocol::model::Track;
use std::sync::Arc;

impl Metadata {
    pub(super) fn prepare_artwork(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            // Shared acquisition may finish, but hidden consumers own no demand.
            return;
        }
        let cached = self
            .metadata
            .track()
            .and_then(|track| self.artwork_manager.cached_result(track));
        let cold = cached.is_none();
        self.artwork_view.replace(cached.flatten(), cx);
        if let Some(track) = self.metadata.track().filter(|_| cold)
            && let Some(track) = self.artwork_loader.prepare(track)
        {
            self.start_artwork(track, cx);
        }
    }

    fn start_artwork(&mut self, track: Track, cx: &mut Context<Self>) {
        let identity = ArtworkIdentity::for_track(&track);
        let manager = Arc::clone(&self.artwork_manager);
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { manager.load(&track) })
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
                        .is_some_and(|track| identity.matches(track))
                {
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
