//! Metadata artwork uses the existing per-window image registry and one physical loader.

use super::Metadata;
use crate::{artwork::ArtworkIdentity, model::Track};
use gpui::Context;
use std::sync::Arc;

impl Metadata {
    pub(super) fn prepare_artwork(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            // Shared acquisition may finish, but hidden consumers own no demand.
            let _ = self.artwork_loader.prepare(None);
            return;
        }
        self.artwork_view.replace(
            self.metadata
                .track()
                .and_then(|track| self.artwork_manager.cached(track))
                .filter(|artwork| !artwork.is_placeholder()),
            cx,
        );
        let current = self
            .metadata
            .track()
            .filter(|track| !self.artwork_manager.is_loaded(track));
        if let Some(track) = self.artwork_loader.prepare(current) {
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
                // A completion may warm the cache, never revive a hidden consumer.
                if this.visible
                    && this
                        .metadata
                        .track()
                        .is_some_and(|track| identity.matches(track))
                {
                    this.artwork_view
                        .replace(loaded.filter(|artwork| !artwork.is_placeholder()), cx);
                    cx.notify();
                }
                let current = this
                    .metadata
                    .track()
                    .filter(|track| this.visible && !this.artwork_manager.is_loaded(track));
                if let Some(next) = this.artwork_loader.finish(&identity, current) {
                    this.start_artwork(next, cx);
                } else {
                    this.prepare_artwork(cx);
                }
            });
        })
        .detach();
    }
}
