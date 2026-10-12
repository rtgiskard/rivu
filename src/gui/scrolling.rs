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

/// Only rendered viewports participate. A click selects the keyboard target;
/// with no clicked target, the viewport under the pointer receives page keys.
#[derive(Default)]
pub(super) struct ScrollViews {
    regions: HashMap<Target, Region>,
    active: Option<Target>,
}

impl ScrollViews {
    pub(super) fn begin_frame(&mut self) {
        for region in self.regions.values_mut() {
            region.visible = false;
        }
    }

    pub(super) fn finish_frame(&mut self) {
        self.regions.retain(|_, region| region.visible);
        if self
            .active
            .is_some_and(|target| !self.regions.contains_key(&target))
        {
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
            region
                .handle
                .base()
                .bounds()
                .contains(&position)
                .then_some(target)
        })
    }

    pub(super) fn click(&mut self, position: Point<Pixels>, modal: bool) {
        self.active = if modal {
            None
        } else {
            self.under_pointer(position)
        };
    }

    pub(super) fn page(&self, down: bool, position: Point<Pixels>) -> bool {
        let target = self.active.or_else(|| self.under_pointer(position));
        let Some(region) = target.and_then(|target| self.regions.get(&target)) else {
            return false;
        };
        region.handle.page(down);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
