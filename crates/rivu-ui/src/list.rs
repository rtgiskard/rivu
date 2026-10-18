use std::ops::Range;

use gpui::{
    AnyElement, App, Bounds, ElementId, InteractiveElement, Interactivity, IntoElement, Pixels,
    Point, StyleRefinement, Styled, UniformList, UniformListDecoration, UniformListScrollHandle,
    Window, div, uniform_list,
};

/// A controlled, uniform-height viewport over absolute logical row indices.
///
/// The owner supplies the logical count, scroll handle, and range renderer. Sparse rows must
/// render placeholders at their original indices. Rendering can also measure a sample row;
/// only `on_viewport` reports demand, after GPUI has resolved the actual visible range.
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

    /// Reports the actual viewport, never the renderer's measurement sample.
    /// The callback may run during prepaint; owners should defer changes requiring a new frame.
    pub fn on_viewport(
        self,
        callback: impl Fn(Range<usize>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.with_decoration(ViewportDemand(callback))
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Render, ScrollStrategy, TestAppContext, prelude::*, px};
    use std::{cell::RefCell, rc::Rc};

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
}
