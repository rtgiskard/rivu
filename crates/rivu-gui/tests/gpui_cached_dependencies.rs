use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, StyleRefinement, Styled,
    TestAppContext, Window, div, px,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

struct ValueView {
    value: usize,
    rendered_values: Rc<RefCell<Vec<usize>>>,
}

impl Render for ValueView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.rendered_values.borrow_mut().push(self.value);
        div().size(px(self.value as f32 + 1.))
    }
}

struct CreatedDuringRender {
    child: Option<Entity<ValueView>>,
    rendered_values: Rc<RefCell<Vec<usize>>>,
    renders: Rc<Cell<usize>>,
}

impl Render for CreatedDuringRender {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.renders.set(self.renders.get() + 1);
        self.child
            .get_or_insert_with(|| {
                cx.new(|_| ValueView {
                    value: 0,
                    rendered_values: self.rendered_values.clone(),
                })
            })
            .clone()
            .cached(StyleRefinement::default().size(px(100.)))
    }
}

#[gpui::test]
fn cached_child_created_during_render_updates_after_parent_redraw(cx: &mut TestAppContext) {
    let rendered_values = Rc::new(RefCell::new(Vec::new()));
    let renders = Rc::new(Cell::new(0));
    let window = cx.add_window({
        let rendered_values = rendered_values.clone();
        let renders = renders.clone();
        move |_, _| CreatedDuringRender {
            child: None,
            rendered_values,
            renders,
        }
    });
    cx.run_until_parked();
    assert_eq!(rendered_values.borrow().as_slice(), &[0]);
    let initial_renders = renders.get();

    window.update(cx, |_, _, cx| cx.notify()).unwrap();
    cx.run_until_parked();
    assert_eq!(renders.get(), initial_renders + 1);
    assert_eq!(rendered_values.borrow().as_slice(), &[0]);

    window
        .update(cx, |root, _, cx| {
            root.child.as_ref().unwrap().update(cx, |child, cx| {
                child.value = 1;
                cx.notify();
            });
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(rendered_values.borrow().as_slice(), &[0, 1]);
}

struct SharedDependencyRoot {
    child: Entity<ValueView>,
    prime_access: bool,
    renders: Rc<Cell<usize>>,
}

impl Render for SharedDependencyRoot {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.renders.set(self.renders.get() + 1);
        if std::mem::take(&mut self.prime_access) {
            // This access precedes the cached child's dependency-capture scope.
            assert_eq!(self.child.read(cx).value, 0);
        }
        self.child
            .clone()
            .cached(StyleRefinement::default().size(px(100.)))
    }
}

#[gpui::test]
fn cached_consumer_retains_dependency_accessed_before_capture(cx: &mut TestAppContext) {
    let rendered_values = Rc::new(RefCell::new(Vec::new()));
    let renders = Rc::new(Cell::new(0));
    let child = cx.new({
        let rendered_values = rendered_values.clone();
        move |_| ValueView {
            value: 0,
            rendered_values,
        }
    });
    let window = cx.add_window({
        let child = child.clone();
        let renders = renders.clone();
        move |_, _| SharedDependencyRoot {
            child,
            prime_access: true,
            renders,
        }
    });
    cx.run_until_parked();
    assert_eq!(rendered_values.borrow().as_slice(), &[0]);
    let initial_renders = renders.get();

    // Only cache reuse can retain the dependency now: the root no longer reads it.
    window.update(cx, |_, _, cx| cx.notify()).unwrap();
    cx.run_until_parked();
    assert_eq!(renders.get(), initial_renders + 1);
    assert_eq!(rendered_values.borrow().as_slice(), &[0]);

    child.update(cx, |child, cx| {
        child.value = 1;
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(rendered_values.borrow().as_slice(), &[0, 1]);
}

struct CachedBranch {
    child: Entity<ValueView>,
    renders: Rc<Cell<usize>>,
}

impl Render for CachedBranch {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.renders.set(self.renders.get() + 1);
        let _value = self.child.read(cx).value;
        self.child
            .clone()
            .cached(StyleRefinement::default().size(px(50.)))
    }
}

struct NestedDependencyRoot {
    branch: Entity<CachedBranch>,
    child: Entity<ValueView>,
    sibling: Entity<ValueView>,
    prime_access: bool,
}

impl Render for NestedDependencyRoot {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.prime_access) {
            assert_eq!(self.child.read(cx).value, 0);
        }
        div()
            .child(
                self.branch
                    .clone()
                    .cached(StyleRefinement::default().size(px(100.))),
            )
            .child(
                self.sibling
                    .clone()
                    .cached(StyleRefinement::default().size(px(100.))),
            )
    }
}

#[gpui::test]
fn nested_cached_views_retain_shared_dependency_without_rerendering_sibling(
    cx: &mut TestAppContext,
) {
    let rendered_values = Rc::new(RefCell::new(Vec::new()));
    let sibling_values = Rc::new(RefCell::new(Vec::new()));
    let branch_renders = Rc::new(Cell::new(0));
    let child = cx.new({
        let rendered_values = rendered_values.clone();
        move |_| ValueView {
            value: 0,
            rendered_values,
        }
    });
    let window = cx.add_window({
        let child = child.clone();
        let sibling_values = sibling_values.clone();
        let branch_renders = branch_renders.clone();
        move |_, cx| NestedDependencyRoot {
            branch: cx.new(|_| CachedBranch {
                child: child.clone(),
                renders: branch_renders,
            }),
            child,
            sibling: cx.new(|_| ValueView {
                value: 99,
                rendered_values: sibling_values,
            }),
            prime_access: true,
        }
    });
    cx.run_until_parked();
    assert_eq!(rendered_values.borrow().as_slice(), &[0]);
    assert_eq!(branch_renders.get(), 1);
    assert_eq!(sibling_values.borrow().as_slice(), &[99]);

    // Reuse nested cached ranges twice, including ranges already reused once.
    for _ in 0..2 {
        window.update(cx, |_, _, cx| cx.notify()).unwrap();
        cx.run_until_parked();
        assert_eq!(rendered_values.borrow().as_slice(), &[0]);
        assert_eq!(branch_renders.get(), 1);
        assert_eq!(sibling_values.borrow().as_slice(), &[99]);
    }

    for value in 1..=2 {
        child.update(cx, |child, cx| {
            child.value = value;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(rendered_values.borrow().last(), Some(&value));
        assert_eq!(rendered_values.borrow().len(), value + 1);
        assert_eq!(branch_renders.get(), value + 1);
        assert_eq!(sibling_values.borrow().as_slice(), &[99]);

        window.update(cx, |_, _, cx| cx.notify()).unwrap();
        cx.run_until_parked();
        assert_eq!(rendered_values.borrow().len(), value + 1);
        assert_eq!(branch_renders.get(), value + 1);
        assert_eq!(sibling_values.borrow().as_slice(), &[99]);
    }
}
