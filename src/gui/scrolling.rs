use super::layout::Layout;
use gpui::{Pixels, Point, ScrollHandle, UniformListScrollHandle, px};
use std::collections::HashMap;

type Target = (&'static str, u64);

#[derive(Clone)]
enum Handle {
    Content(ScrollHandle),
    List(UniformListScrollHandle),
}

impl Handle {
    fn base(&self) -> ScrollHandle {
        match self {
            Self::Content(handle) => handle.clone(),
            Self::List(handle) => handle.0.borrow().base_handle.clone(),
        }
    }

    fn page(&self, down: bool) {
        if let Self::List(handle) = self {
            handle.0.borrow_mut().deferred_scroll_to_item = None;
        }
        let handle = self.base();
        let mut offset = handle.offset();
        offset.y = page_offset(
            offset.y,
            handle.bounds().size.height,
            handle.max_offset().y,
            down,
        );
        handle.set_offset(offset);
    }
}

fn page_offset(offset: Pixels, viewport: Pixels, maximum: Pixels, down: bool) -> Pixels {
    let step = viewport.max(px(0.));
    let next = if down { offset - step } else { offset + step };
    next.max(-maximum.max(px(0.))).min(px(0.))
}

struct Region {
    handle: Handle,
    visible: bool,
}

/// Viewport state lives with its panel in the current layout, even behind a tab.
/// Only rendered viewports participate in pointer and keyboard input.
#[derive(Default)]
pub(super) struct ScrollViews {
    regions: HashMap<Target, Region>,
    active: Option<Target>,
}

impl ScrollViews {
    pub(super) fn retain_layout(&mut self, layout: &Layout) {
        self.regions
            .retain(|(_, panel_id), _| layout.contains_panel(*panel_id));
        self.active = None;
    }

    /// Cached active panels may not render again this frame. Keep their last
    /// registrations, but never route input into a panel hidden by tab changes.
    pub(super) fn begin_frame(&mut self, layout: &Layout) {
        for ((_, panel_id), region) in &mut self.regions {
            region.visible &= layout.is_panel_active(*panel_id);
        }
    }

    /// An actual panel render rebuilds only its own registrations. In particular,
    /// switching a playlist's list region must retire the previous region even
    /// while other cached panels remain input-active.
    pub(super) fn begin_panel(&mut self, panel_id: u64) {
        for ((_, owner), region) in &mut self.regions {
            if *owner == panel_id {
                region.visible = false;
            }
        }
    }

    pub(super) fn finish_frame(&mut self) {
        if self.active.is_some_and(|target| {
            !self
                .regions
                .get(&target)
                .is_some_and(|region| region.visible)
        }) {
            self.active = None;
        }
    }

    pub(super) fn register_list(&mut self, target: Target, handle: &UniformListScrollHandle) {
        self.regions.insert(
            target,
            Region {
                handle: Handle::List(handle.clone()),
                visible: true,
            },
        );
    }

    pub(super) fn list(&mut self, target: Target) -> UniformListScrollHandle {
        let region = self.regions.entry(target).or_insert_with(|| Region {
            handle: Handle::List(UniformListScrollHandle::new()),
            visible: true,
        });
        region.visible = true;
        let Handle::List(handle) = &region.handle else {
            unreachable!("list target uses a list handle")
        };
        handle.clone()
    }

    pub(super) fn scroll_list_to(&self, kind: &str, index: usize) {
        for ((region_kind, _), region) in &self.regions {
            if *region_kind == kind
                && region.visible
                && let Handle::List(handle) = &region.handle
            {
                handle.scroll_to_item(index, gpui::ScrollStrategy::Nearest);
            }
        }
    }

    pub(super) fn content(&mut self, target: Target) -> ScrollHandle {
        let region = self.regions.entry(target).or_insert_with(|| Region {
            handle: Handle::Content(ScrollHandle::new()),
            visible: true,
        });
        region.visible = true;
        let Handle::Content(handle) = &region.handle else {
            unreachable!("content target uses a content handle")
        };
        handle.clone()
    }

    fn under_pointer(&self, position: Point<Pixels>) -> Option<Target> {
        self.regions.iter().find_map(|(&target, region)| {
            (region.visible && region.handle.base().bounds().contains(&position)).then_some(target)
        })
    }

    pub(super) fn click(&mut self, position: Point<Pixels>, modal: bool) {
        self.active = if modal {
            None
        } else {
            self.under_pointer(position)
        };
    }

    /// Return the panel whose viewport changed so its cached view can be invalidated.
    pub(super) fn page(&self, down: bool, position: Point<Pixels>) -> Option<u64> {
        let target = self.active.or_else(|| self.under_pointer(position))?;
        let region = self.regions.get(&target).filter(|region| region.visible)?;
        region.handle.page(down);
        Some(target.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout {
            root: None,
            next_id: 1,
            locked: false,
        }
    }

    #[test]
    fn hidden_viewports_keep_independent_offsets_without_receiving_input() {
        let mut layout = layout();
        let queue = layout.add("queue", None);
        let history = layout.add("history", None);
        let metadata = layout.add("metadata", None);
        let mut views = ScrollViews::default();
        let queue_scroll = views.list(("queue", queue));
        queue_scroll
            .0
            .borrow()
            .base_handle
            .set_offset(gpui::point(px(0.), px(-420.)));
        views
            .list(("history", history))
            .0
            .borrow()
            .base_handle
            .set_offset(gpui::point(px(0.), px(-840.)));
        views
            .content(("metadata", metadata))
            .set_offset(gpui::point(px(0.), px(-160.)));
        views.active = Some(("queue", queue));
        views.begin_frame(&layout);
        views.begin_panel(metadata);
        views.finish_frame();
        assert!(views.active.is_none());
        assert!(views.page(true, gpui::point(px(0.), px(0.))).is_none());
        assert!(views.under_pointer(gpui::point(px(0.), px(0.))).is_none());
        views.scroll_list_to("queue", 80);
        assert!(queue_scroll.0.borrow().deferred_scroll_to_item.is_none());
        assert_eq!(
            views
                .list(("queue", queue))
                .0
                .borrow()
                .base_handle
                .offset()
                .y,
            px(-420.)
        );
        assert_eq!(
            views
                .list(("history", history))
                .0
                .borrow()
                .base_handle
                .offset()
                .y,
            px(-840.)
        );
        assert_eq!(views.content(("metadata", metadata)).offset().y, px(-160.));
    }

    #[test]
    fn removing_a_panel_releases_scroll_state_but_not_hidden_siblings() {
        let mut layout = layout();
        let removed = layout.add("queue", None);
        let retained = layout.add("queue", None);
        let mut views = ScrollViews::default();
        let handle = views.list(("queue", removed));
        let retired = std::rc::Rc::downgrade(&handle.0);
        drop(handle);
        views
            .list(("queue", retained))
            .0
            .borrow()
            .base_handle
            .set_offset(gpui::point(px(0.), px(-420.)));
        views.begin_frame(&layout);
        views.finish_frame();
        assert!(layout.remove(removed));
        views.retain_layout(&layout);
        assert!(retired.upgrade().is_none());
        assert_eq!(
            views
                .list(("queue", retained))
                .0
                .borrow()
                .base_handle
                .offset()
                .y,
            px(-420.)
        );
        let added = layout.add("queue", None);
        assert_eq!(
            views
                .list(("queue", added))
                .0
                .borrow()
                .base_handle
                .offset()
                .y,
            px(0.)
        );
    }

    #[test]
    fn cached_active_panels_keep_scroll_targets_until_their_own_render() {
        let mut layout = layout();
        let playlist = layout.add("playlists", None);
        let mut views = ScrollViews::default();
        let old = views.list(("playlist-list", playlist));
        views.active = Some(("playlist-list", playlist));
        views.begin_frame(&layout);
        views.finish_frame();
        assert_eq!(views.active, Some(("playlist-list", playlist)));
        views.scroll_list_to("playlist-list", 7);
        assert!(old.0.borrow().deferred_scroll_to_item.is_some());

        views.begin_panel(playlist);
        views.list(("playlist-tracks", playlist));
        views.finish_frame();
        assert!(views.active.is_none());
        assert!(!views.regions[&("playlist-list", playlist)].visible);
        assert!(views.regions[&("playlist-tracks", playlist)].visible);
        views.begin_frame(&layout);
        views.finish_frame();
        assert!(!views.regions[&("playlist-list", playlist)].visible);
        assert!(views.regions[&("playlist-tracks", playlist)].visible);

        layout.add("history", None);
        views.begin_frame(&layout);
        views.finish_frame();
        assert!(!views.regions[&("playlist-tracks", playlist)].visible);
    }

    #[test]
    fn album_region_switch_retires_grid_and_preserves_other_instances() {
        let mut views = ScrollViews::default();
        let grid = UniformListScrollHandle::new();
        let tracks = UniformListScrollHandle::new();
        let sibling = UniformListScrollHandle::new();
        views.register_list(("album-grid", 1), &grid);
        views.register_list(("album-grid", 2), &sibling);
        grid.scroll_to_item(12, gpui::ScrollStrategy::Top);
        sibling.scroll_to_item(24, gpui::ScrollStrategy::Top);
        views.active = Some(("album-grid", 1));

        // Paging mutates the browser-owned handle, not a parallel registry handle.
        assert_eq!(views.page(true, gpui::point(px(-1.), px(-1.))), Some(1));
        assert!(grid.0.borrow().deferred_scroll_to_item.is_none());
        assert!(sibling.0.borrow().deferred_scroll_to_item.is_some());

        grid.0
            .borrow()
            .base_handle
            .set_offset(gpui::point(px(0.), px(-240.)));
        views.begin_panel(1);
        views.register_list(("album-tracks", 1), &tracks);
        views.finish_frame();
        assert!(views.active.is_none());
        assert!(!views.regions[&("album-grid", 1)].visible);
        assert!(views.regions[&("album-tracks", 1)].visible);
        assert!(views.regions[&("album-grid", 2)].visible);

        tracks.scroll_to_item(8, gpui::ScrollStrategy::Top);
        views.active = Some(("album-tracks", 1));
        assert_eq!(views.page(false, gpui::point(px(-1.), px(-1.))), Some(1));
        assert!(tracks.0.borrow().deferred_scroll_to_item.is_none());
        assert!(sibling.0.borrow().deferred_scroll_to_item.is_some());

        views.begin_panel(1);
        views.register_list(("album-grid", 1), &grid);
        views.finish_frame();
        assert!(!views.regions[&("album-tracks", 1)].visible);
        assert_eq!(grid.0.borrow().base_handle.offset().y, px(-240.));
    }

    #[test]
    fn hidden_and_removed_album_regions_cannot_receive_paging() {
        let mut layout = layout();
        let albums = layout.add("albums", None);
        let mut views = ScrollViews::default();
        let grid = UniformListScrollHandle::new();
        views.register_list(("album-grid", albums), &grid);
        views.active = Some(("album-grid", albums));
        grid.scroll_to_item(12, gpui::ScrollStrategy::Top);

        // Window hiding/empty content retires registrations even without a paint.
        views.begin_panel(albums);
        views.finish_frame();
        assert_eq!(views.page(true, gpui::point(px(-1.), px(-1.))), None);
        assert!(grid.0.borrow().deferred_scroll_to_item.is_some());

        views.register_list(("album-grid", albums), &grid);
        views.active = Some(("album-grid", albums));
        layout.add("history", None);
        views.begin_frame(&layout);
        views.finish_frame();
        assert_eq!(views.page(true, gpui::point(px(-1.), px(-1.))), None);
        assert!(grid.0.borrow().deferred_scroll_to_item.is_some());

        assert!(layout.remove(albums));
        views.retain_layout(&layout);
        assert!(!views.regions.contains_key(&("album-grid", albums)));
    }

    #[test]
    fn page_steps_by_viewport_and_clamps_both_edges() {
        assert_eq!(page_offset(px(-20.), px(100.), px(250.), true), px(-120.));
        assert_eq!(page_offset(px(-120.), px(100.), px(250.), false), px(-20.));
        assert_eq!(page_offset(px(-200.), px(100.), px(250.), true), px(-250.));
        assert_eq!(page_offset(px(-20.), px(100.), px(250.), false), px(0.));
        assert_eq!(page_offset(px(0.), px(100.), px(0.), true), px(0.));
        assert_eq!(page_offset(px(-20.), px(0.), px(250.), true), px(-20.));
    }
}
