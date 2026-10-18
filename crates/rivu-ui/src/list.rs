use std::ops::Range;

use gpui::{
    AnyElement, App, Bounds, Context, ElementId, InteractiveElement, Interactivity, IntoElement,
    Pixels, Point, StyleRefinement, Styled, UniformList, UniformListDecoration,
    UniformListScrollHandle, WeakEntity, Window, div, uniform_list,
};

/// A controlled, uniform-height viewport over absolute logical row indices.
///
/// The owner supplies the logical count, scroll handle, and range renderer. Sparse rows must
/// render placeholders at their original indices. Rendering can also measure a sample row;
/// only viewport callbacks report demand, after GPUI has resolved the actual visible range.
/// Selection, queries, loading, and the lifetime of the scroll handle remain owner-controlled.
pub struct VirtualList {
    list: UniformList,
    scroll: UniformListScrollHandle,
}

impl VirtualList {
    pub fn new<R: IntoElement>(
        id: impl Into<ElementId>,
        count: usize,
        scroll: &UniformListScrollHandle,
        render: impl Fn(Range<usize>, &mut Window, &mut App) -> Vec<R> + 'static,
    ) -> Self {
        Self {
            list: uniform_list(id, count, render).track_scroll(scroll),
            scroll: scroll.clone(),
        }
    }

    /// Observes the actual viewport synchronously, never the renderer's measurement sample.
    /// Runs during prepaint: use `on_viewport_deferred` for demand that mutates owner state.
    pub fn on_viewport(
        self,
        callback: impl Fn(Range<usize>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.with_decoration(ViewportDemand(callback))
    }

    /// Delivers each actual viewport at the end of GPUI's current effect cycle, after drawing.
    /// The owner is held weakly; delivery is skipped if it has been released. The callback must
    /// retain the owner's visibility/generation guards and must not capture a strong owner.
    /// Equal ranges are not deduplicated: a retry or newly available data may need fresh demand.
    /// Keep captures cheap to clone: one clone moves into each deferred effect, without a shared
    /// callback allocation. Capture weak handles rather than retaining an entity in the callback.
    pub fn on_viewport_deferred<T: 'static>(
        self,
        owner: WeakEntity<T>,
        callback: impl Fn(&mut T, Range<usize>, &mut Context<T>) + Clone + 'static,
    ) -> Self {
        self.with_decoration(DeferredViewportDemand { owner, callback })
    }

    pub fn with_decoration(mut self, decoration: impl UniformListDecoration + 'static) -> Self {
        self.list = self.list.with_decoration(decoration);
        self
    }

    /// Adds the standard reserved scrollbar gutter, using the owner's stable interaction ID.
    pub fn scrollbar(self, id: impl Into<ElementId>) -> impl IntoElement {
        self.scrollbar_with(id, |list| list)
    }

    /// Wraps the viewport before adding its scrollbar gutter. This keeps host measurement or
    /// overlay elements inside the same content bounds as the rows, excluding the gutter.
    pub fn scrollbar_with<E: IntoElement>(
        self,
        id: impl Into<ElementId>,
        wrap: impl FnOnce(Self) -> E,
    ) -> impl IntoElement {
        let scroll = self.scroll.clone();
        crate::scrollbar::scrollable_list(id, wrap(self), &scroll).into_any_element()
    }
}

impl Styled for VirtualList {
    fn style(&mut self) -> &mut StyleRefinement {
        self.list.style()
    }
}

impl InteractiveElement for VirtualList {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.list.interactivity()
    }
}

impl IntoElement for VirtualList {
    type Element = UniformList;

    fn into_element(self) -> Self::Element {
        self.list
    }
}

struct ViewportDemand<F>(F);

impl<F: Fn(Range<usize>, &mut Window, &mut App)> UniformListDecoration for ViewportDemand<F> {
    fn compute(
        &self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: Point<Pixels>,
        _: Pixels,
        _: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        (self.0)(range, window, cx);
        div().into_any_element()
    }
}

struct DeferredViewportDemand<T: 'static, F> {
    owner: WeakEntity<T>,
    callback: F,
}

impl<T: 'static, F: Fn(&mut T, Range<usize>, &mut Context<T>) + Clone + 'static>
    UniformListDecoration for DeferredViewportDemand<T, F>
{
    fn compute(
        &self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: Point<Pixels>,
        _: Pixels,
        _: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let owner = self.owner.clone();
        let callback = self.callback.clone();
        cx.defer(move |cx| {
            let _ = owner.update(cx, |this, cx| callback(this, range, cx));
        });
        div().into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Render, ScrollStrategy, TestAppContext, prelude::*, px};
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    struct ListFixture {
        scroll: UniformListScrollHandle,
        rendered: Rc<RefCell<Vec<Range<usize>>>>,
        demanded: Rc<RefCell<Vec<Range<usize>>>>,
    }

    impl Render for ListFixture {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let rendered = self.rendered.clone();
            let demanded = self.demanded.clone();
            div().size_full().child(
                VirtualList::new("rows", 100, &self.scroll, move |range, _, _| {
                    rendered.borrow_mut().push(range.clone());
                    range.map(|index| div().id(index).h(px(20.))).collect()
                })
                .on_viewport(move |range, _, _| demanded.borrow_mut().push(range))
                .h(px(200.))
                .w(px(200.)),
            )
        }
    }

    #[test]
    fn measurement_samples_do_not_issue_viewport_demand() {
        let mut cx = TestAppContext::single();
        let rendered = Rc::new(RefCell::new(Vec::new()));
        let demanded = Rc::new(RefCell::new(Vec::new()));
        let scroll = UniformListScrollHandle::new();
        scroll.scroll_to_item_strict(40, ScrollStrategy::Top);
        let window = cx.add_window({
            let rendered = rendered.clone();
            let demanded = demanded.clone();
            move |_, _| ListFixture {
                scroll,
                rendered,
                demanded,
            }
        });
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();

        assert!(
            rendered.borrow().contains(&(0..1)),
            "GPUI measured its sample row"
        );
        assert!(
            rendered.borrow().contains(&(40..50)),
            "logical viewport indices are retained"
        );
        let demanded = demanded.borrow();
        assert!(!demanded.is_empty());
        assert!(demanded.iter().all(|range| *range == (40..50)));
    }

    #[derive(Debug, PartialEq)]
    enum ViewportPhase {
        Render(Range<usize>, bool),
        Viewport(Range<usize>),
        Paint,
        Demand(Range<usize>),
    }

    struct DeferredListFixture {
        scroll: UniformListScrollHandle,
        phases: Rc<RefCell<Vec<ViewportPhase>>>,
        loaded: bool,
    }

    impl Render for DeferredListFixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let rendered = self.phases.clone();
            let observed = self.phases.clone();
            let painted = self.phases.clone();
            let loaded = self.loaded;
            div()
                .size_full()
                .child(
                    VirtualList::new("deferred-rows", 100, &self.scroll, move |range, _, _| {
                        rendered
                            .borrow_mut()
                            .push(ViewportPhase::Render(range.clone(), loaded));
                        range.map(|index| div().id(index).h(px(20.))).collect()
                    })
                    .on_viewport(move |range, _, _| {
                        observed.borrow_mut().push(ViewportPhase::Viewport(range));
                    })
                    .on_viewport_deferred(cx.weak_entity(), |this, range, cx| {
                        this.phases.borrow_mut().push(ViewportPhase::Demand(range));
                        if !this.loaded {
                            this.loaded = true;
                            cx.notify();
                        }
                    })
                    .h(px(200.))
                    .w(px(200.)),
                )
                .child(
                    gpui::canvas(
                        |_, _, _| (),
                        move |_, _, _, _| {
                            painted.borrow_mut().push(ViewportPhase::Paint);
                        },
                    )
                    .size(px(1.)),
                )
        }
    }

    #[test]
    fn deferred_viewport_runs_after_paint_and_notifies_a_fresh_frame() {
        let mut cx = TestAppContext::single();
        let phases = Rc::new(RefCell::new(Vec::new()));
        let scroll = UniformListScrollHandle::new();
        scroll.scroll_to_item_strict(40, ScrollStrategy::Top);
        let window = cx.add_window({
            let phases = phases.clone();
            move |_, _| DeferredListFixture {
                scroll,
                phases,
                loaded: false,
            }
        });
        cx.run_until_parked();
        {
            let phases = phases.borrow();
            assert!(phases.contains(&ViewportPhase::Render(0..1, false)));
            let viewport = phases
                .iter()
                .position(|phase| *phase == ViewportPhase::Viewport(40..50))
                .expect("actual GPUI prepaint viewport");
            let paint = phases
                .iter()
                .position(|phase| *phase == ViewportPhase::Paint)
                .expect("actual GPUI paint");
            let demand = phases
                .iter()
                .position(|phase| *phase == ViewportPhase::Demand(40..50))
                .expect("deferred owner demand");
            assert!(viewport < paint && paint < demand, "{phases:?}");
            assert!(
                phases[demand + 1..].contains(&ViewportPhase::Render(40..50, true)),
                "the deferred notification redraws accepted rows without another input: {phases:?}",
            );
            assert!(phases.iter().all(|phase| {
                !matches!(phase, ViewportPhase::Demand(range) if *range != (40..50))
            }));
        }

        // Keep geometry unchanged and draw twice in one effect cycle. Each report must survive,
        // but neither callback may run while GPUI is still drawing.
        phases.borrow_mut().clear();
        cx.update_window(window.into(), |_, window, cx| {
            for _ in 0..2 {
                window.refresh();
                window.draw(cx).clear(cx);
            }
            let phases = phases.borrow();
            assert_eq!(
                phases
                    .iter()
                    .filter(|phase| { matches!(phase, ViewportPhase::Viewport(_)) })
                    .count(),
                2
            );
            assert!(
                !phases
                    .iter()
                    .any(|phase| matches!(phase, ViewportPhase::Demand(_)))
            );
        })
        .unwrap();
        let phases = phases.borrow();
        assert_eq!(
            phases
                .iter()
                .filter(|phase| {
                    matches!(phase, ViewportPhase::Demand(range) if *range == (40..50))
                })
                .count(),
            2
        );
    }

    struct DemandOwner {
        delivered: Rc<Cell<usize>>,
        released: Rc<Cell<bool>>,
    }

    impl Drop for DemandOwner {
        fn drop(&mut self) {
            self.released.set(true);
        }
    }

    struct WeakOwnerListFixture {
        owner: WeakEntity<DemandOwner>,
        scroll: UniformListScrollHandle,
        observed: Rc<Cell<usize>>,
    }

    impl Render for WeakOwnerListFixture {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let observed = self.observed.clone();
            VirtualList::new("weak-owner-rows", 100, &self.scroll, |range, _, _| {
                range.map(|index| div().id(index).h(px(20.))).collect()
            })
            .on_viewport(move |_, _, _| observed.set(observed.get() + 1))
            .on_viewport_deferred(self.owner.clone(), |owner, _, _| {
                owner.delivered.set(owner.delivered.get() + 1);
            })
            .h(px(200.))
            .w(px(200.))
        }
    }

    #[test]
    fn deferred_viewport_does_not_retain_owner_until_delivery() {
        let mut cx = TestAppContext::single();
        let delivered = Rc::new(Cell::new(0));
        let released = Rc::new(Cell::new(false));
        let observed = Rc::new(Cell::new(0));
        let mut owner = Some(cx.new(|_| DemandOwner {
            delivered: delivered.clone(),
            released: released.clone(),
        }));
        let weak = owner.as_ref().unwrap().downgrade();
        let window = cx.add_window({
            let owner = weak.clone();
            let observed = observed.clone();
            move |_, _| WeakOwnerListFixture {
                owner,
                scroll: UniformListScrollHandle::new(),
                observed,
            }
        });
        cx.run_until_parked();
        let before = delivered.get();
        let observed_before = observed.get();
        cx.update_window(window.into(), |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
            assert_eq!(observed.get(), observed_before + 1, "viewport was computed");
            assert_eq!(delivered.get(), before, "delivery is still deferred");
            drop(owner.take());
            assert!(
                weak.upgrade().is_none(),
                "queued delivery must not hold the owner"
            );
        })
        .unwrap();
        assert!(released.get());
        assert_eq!(delivered.get(), before, "released owner receives no demand");
    }
}
