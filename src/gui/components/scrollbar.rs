use std::{cell::RefCell, rc::Rc};

use gpui::{
    App, Bounds, DispatchPhase, Element, ElementId, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, ScrollWheelEvent, Style, UniformListScrollHandle, Window, div, fill,
    linear_color_stop, linear_gradient, point, prelude::*, px, relative, rgb, size,
};

const SLOT_WIDTH: f32 = 12.;
const THUMB_WIDTH: f32 = 6.;
const IDLE_THUMB_WIDTH: f32 = 2.;
const MIN_THUMB_HEIGHT: f32 = 24.;

#[derive(Clone)]
enum ScrollSource {
    List(UniformListScrollHandle),
    Area(Rc<gpui::ScrollHandle>),
}

impl ScrollSource {
    fn handle(&self) -> gpui::ScrollHandle {
        match self {
            Self::List(scroll) => scroll.0.borrow().base_handle.clone(),
            Self::Area(scroll) => scroll.as_ref().clone(),
        }
    }

    fn same_handle(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::List(a), Self::List(b)) => Rc::ptr_eq(&a.0, &b.0),
            (Self::Area(a), Self::Area(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

pub(super) fn scrollable_area(
    id: impl Into<ElementId>,
    child: impl IntoElement,
    scroll: &Rc<gpui::ScrollHandle>,
) -> impl IntoElement {
    scrollable(id, child, ScrollSource::Area(scroll.clone()))
}

pub(super) fn scrollable_list(
    id: impl Into<ElementId>,
    child: impl IntoElement,
    scroll: &UniformListScrollHandle,
) -> impl IntoElement {
    scrollable(id, child, ScrollSource::List(scroll.clone()))
}

fn scrollable(
    id: impl Into<ElementId>,
    child: impl IntoElement,
    scroll: ScrollSource,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .size_full()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .overflow_hidden()
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .h_full()
                .overflow_hidden()
                .child(child),
        )
        // Keep this after the list: its prepaint reads this frame's list metrics,
        // including wheel offsets, deferred scroll requests and shrink clamping.
        .child(Scrollbar { scroll })
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Geometry {
    track: Bounds<Pixels>,
    viewport: Bounds<Pixels>,
    content_height: Pixels,
    max_offset: Pixels,
}

impl Geometry {
    fn read(track: Bounds<Pixels>, scroll: &ScrollSource) -> Self {
        let handle = scroll.handle();
        let viewport = handle.bounds();
        let max_offset = handle.max_offset().y.max(px(0.));
        Self {
            track,
            viewport,
            content_height: viewport.size.height + max_offset,
            max_offset,
        }
    }

    fn thumb_height(self) -> Option<Pixels> {
        if self.max_offset <= px(0.)
            || self.viewport.size.height <= px(0.)
            || self.track.size.height <= px(0.)
            || self.content_height <= px(0.)
        {
            return None;
        }
        Some(
            (self.track.size.height * (self.viewport.size.height / self.content_height)).clamp(
                px(MIN_THUMB_HEIGHT).min(self.track.size.height),
                self.track.size.height,
            ),
        )
    }

    fn thumb(self, offset: Pixels) -> Option<Bounds<Pixels>> {
        let height = self.thumb_height()?;
        let travel = self.track.size.height - height;
        let progress = (-offset).clamp(px(0.), self.max_offset) / self.max_offset;
        Some(Bounds::new(
            point(
                self.track.left() + (self.track.size.width - px(THUMB_WIDTH)) / 2.,
                self.track.top() + travel * progress,
            ),
            size(px(THUMB_WIDTH), height),
        ))
    }

    fn offset_for_pointer(self, y: Pixels, grab_offset: Pixels) -> Pixels {
        let travel = self.track.size.height - self.thumb_height().unwrap_or(px(0.));
        if travel <= px(0.) {
            return px(0.);
        }
        let progress = (y - self.track.top() - grab_offset).clamp(px(0.), travel) / travel;
        -(self.max_offset * progress)
    }
}

// Paint after the list has resolved scrolling, without a hitbox or an offscreen
// content texture. The solid surface color deliberately matches docked panels.
fn paint_scroll_edges(
    theme: &'static crate::theme::Palette,
    geometry: Geometry,
    offset: Pixels,
    window: &mut Window,
) {
    let viewport = geometry.viewport;
    let height = (window.rem_size() * 0.75).min(viewport.size.height / 4.);
    if geometry.max_offset <= px(0.) || height <= px(0.) || viewport.size.width <= px(0.) {
        return;
    }
    let scrolled = (-offset).clamp(px(0.), geometry.max_offset);
    for (distance, y, angle) in [
        (scrolled, viewport.top(), 180.),
        (
            geometry.max_offset - scrolled,
            viewport.bottom() - height,
            0.,
        ),
    ] {
        let opacity = (distance / height).clamp(0., 1.);
        if opacity > 0. {
            let color = rgb(theme.panel);
            window.paint_quad(fill(
                Bounds::new(point(viewport.left(), y), size(viewport.size.width, height)),
                linear_gradient(
                    angle,
                    linear_color_stop(color.alpha(opacity), 0.),
                    linear_color_stop(color.alpha(0.), 1.),
                ),
            ));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Gesture {
    grab_offset: Option<Pixels>,
}

struct ScrollbarState {
    scroll: ScrollSource,
    geometry: Geometry,
    hovered: bool,
    // An owned press survives geometry invalidation so its release cannot reach
    // a row. A missing grab offset cancels movement until that release.
    gesture: Option<Gesture>,
}

impl ScrollbarState {
    fn update_geometry(&mut self, geometry: Geometry) {
        if self.geometry != geometry {
            self.geometry = geometry;
            self.cancel_motion();
        }
    }

    fn cancel_motion(&mut self) {
        if let Some(gesture) = self.gesture.as_mut() {
            gesture.grab_offset = None;
        }
    }

    fn set_offset(&self, y: Pixels) {
        let handle = self.scroll.handle();
        let mut offset = handle.offset();
        offset.y = y.clamp(-self.geometry.max_offset, px(0.));
        handle.set_offset(offset);
    }

    fn begin(&mut self, y: Pixels) {
        let offset = self.scroll.handle().offset().y;
        let Some(thumb) = self.geometry.thumb(offset) else {
            return;
        };
        let grab_offset = if y >= thumb.top() && y <= thumb.bottom() {
            y - thumb.top()
        } else {
            let center = thumb.size.height / 2.;
            self.set_offset(self.geometry.offset_for_pointer(y, center));
            center
        };
        self.gesture = Some(Gesture {
            grab_offset: Some(grab_offset),
        });
    }

    fn move_pointer(&mut self, y: Pixels, left_pressed: bool) -> bool {
        let Some(gesture) = self.gesture else {
            return false;
        };
        if !left_pressed {
            self.gesture = None;
        } else if let Some(grab_offset) = gesture.grab_offset {
            self.set_offset(self.geometry.offset_for_pointer(y, grab_offset));
        }
        true
    }

    fn release(&mut self) -> bool {
        self.gesture.take().is_some()
    }
}

struct Scrollbar {
    scroll: ScrollSource,
}

struct FrameState {
    state: Rc<RefCell<ScrollbarState>>,
    hitbox: Hitbox,
}

impl IntoElement for Scrollbar {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Scrollbar {
    type RequestLayoutState = ();
    type PrepaintState = FrameState;

    fn id(&self) -> Option<ElementId> {
        Some("vertical-scrollbar".into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = Style {
            size: size(px(SLOT_WIDTH).into(), relative(1.).into()),
            flex_shrink: 0.,
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) -> FrameState {
        let geometry = Geometry::read(bounds, &self.scroll);
        let state = window.with_element_state::<Rc<RefCell<ScrollbarState>>, _>(
            id.expect("scrollbar has a stable element id"),
            |state, _| {
                let state = state.unwrap_or_else(|| {
                    Rc::new(RefCell::new(ScrollbarState {
                        scroll: self.scroll.clone(),
                        geometry,
                        hovered: false,
                        gesture: None,
                    }))
                });
                {
                    let mut retained = state.borrow_mut();
                    if !retained.scroll.same_handle(&self.scroll) {
                        retained.scroll = self.scroll.clone();
                        retained.cancel_motion();
                    }
                    retained.update_geometry(geometry);
                    // UniformList already clamps before laying out its rows.
                    // Retain the same invariant for empty lists as well.
                    let offset = self.scroll.handle().offset().y;
                    retained.set_offset(offset);
                }
                (state.clone(), state)
            },
        );
        FrameState {
            state,
            hitbox: window.insert_hitbox(bounds, HitboxBehavior::BlockMouse),
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        frame: &mut FrameState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let theme = crate::gui::theme::palette(cx);
        {
            let mut state = frame.state.borrow_mut();
            state.hovered = frame.hitbox.is_hovered(window);
            let offset = self.scroll.handle().offset().y;
            paint_scroll_edges(theme, state.geometry, offset, window);
            if let Some(mut thumb) = state.geometry.thumb(offset) {
                let expanded = state.gesture.is_some() || state.hovered;
                let width = px(if expanded {
                    THUMB_WIDTH
                } else {
                    IDLE_THUMB_WIDTH
                });
                thumb.origin.x += (thumb.size.width - width) / 2.;
                thumb.size.width = width;
                let color = if expanded {
                    rgb(theme.accent).alpha(0.85)
                } else {
                    rgb(theme.muted).alpha(0.22)
                };
                window.paint_quad(fill(thumb, color).corner_radii(width / 2.));
            }
        }

        let state = frame.state.clone();
        let hitbox = frame.hitbox.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture && hitbox.is_hovered(window) {
                if event.button == MouseButton::Left {
                    state.borrow_mut().begin(event.position.y);
                    window.refresh();
                }
                window.prevent_default();
                cx.stop_propagation();
            }
        });

        let state = frame.state.clone();
        let hitbox = frame.hitbox.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Capture {
                return;
            }
            let mut state = state.borrow_mut();
            let hovered = hitbox.is_hovered(window);
            let hover_changed = hovered != state.hovered;
            state.hovered = hovered;
            let owned = state.move_pointer(event.position.y, event.dragging());
            if hover_changed || owned {
                window.refresh();
            }
            if owned {
                cx.stop_propagation();
            }
        });

        let state = frame.state.clone();
        let hitbox = frame.hitbox.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture {
                let owned = event.button == MouseButton::Left && state.borrow_mut().release();
                if owned || hitbox.is_hovered(window) {
                    window.refresh();
                    cx.stop_propagation();
                }
            }
        });

        let state = frame.state.clone();
        let hitbox = frame.hitbox.clone();
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.should_handle_scroll(window) {
                let state = state.borrow();
                let delta = event.delta.pixel_delta(window.line_height()).y;
                let offset = state.scroll.handle().offset().y;
                state.set_offset(offset + delta);
                window.refresh();
                cx.stop_propagation();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(viewport: f32, content: f32) -> Geometry {
        Geometry {
            track: Bounds::new(point(px(100.), px(10.)), size(px(SLOT_WIDTH), px(viewport))),
            viewport: Bounds::new(point(px(0.), px(10.)), size(px(100.), px(viewport))),
            content_height: px(content),
            max_offset: px((content - viewport).max(0.)),
        }
    }

    fn state(geometry: Geometry) -> ScrollbarState {
        ScrollbarState {
            scroll: ScrollSource::List(UniformListScrollHandle::new()),
            geometry,
            hovered: false,
            gesture: None,
        }
    }

    #[test]
    fn thumb_is_proportional_bounded_and_hidden_without_overflow() {
        assert!(geometry(100., 100.).thumb(px(0.)).is_none());
        assert!(geometry(100., 0.).thumb(px(0.)).is_none());
        assert!(geometry(0., 100.).thumb(px(0.)).is_none());
        assert_eq!(geometry(100., 200.).thumb_height(), Some(px(50.)));
        assert_eq!(geometry(100., 10_000.).thumb_height(), Some(px(24.)));
        assert_eq!(geometry(10., 100.).thumb_height(), Some(px(10.)));
    }

    #[test]
    fn thumb_and_pointer_mapping_clamp_at_both_ends() {
        let metrics = geometry(100., 1_000.);
        let top = metrics.thumb(px(50.)).unwrap();
        let bottom = metrics.thumb(px(-2_000.)).unwrap();
        assert_eq!(top.top(), metrics.track.top());
        assert_eq!(bottom.bottom(), metrics.track.bottom());
        assert_eq!(top.size.width, px(THUMB_WIDTH));
        assert_eq!(metrics.offset_for_pointer(px(-100.), px(12.)), px(0.));
        assert_eq!(metrics.offset_for_pointer(px(500.), px(12.)), px(-900.));
        assert_eq!(metrics.offset_for_pointer(px(60.), px(12.)), px(-450.));
        assert_eq!(
            geometry(10., 100.).offset_for_pointer(px(30.), px(5.)),
            px(0.)
        );
    }

    #[test]
    fn track_jump_and_drag_preserve_grab_offset_and_capture_release() {
        let mut state = state(geometry(100., 1_000.));
        state.begin(px(60.));
        assert_eq!(state.scroll.handle().offset().y, px(-450.));
        assert!(state.move_pointer(px(500.), true));
        assert_eq!(state.scroll.handle().offset().y, px(-900.));
        assert!(state.move_pointer(px(-100.), true));
        assert_eq!(state.scroll.handle().offset().y, px(0.));
        assert!(state.release());
        assert!(!state.move_pointer(px(60.), true));
        state.begin(px(15.));
        assert_eq!(state.gesture.unwrap().grab_offset, Some(px(5.)));
        assert!(state.move_pointer(px(53.), true));
        assert_eq!(state.scroll.handle().offset().y, px(-450.));
    }

    #[test]
    fn geometry_changes_cancel_motion_but_still_consume_release() {
        let mut state = state(geometry(100., 1_000.));
        state.begin(px(15.));
        state.update_geometry(geometry(80., 1_000.));
        assert_eq!(state.gesture.unwrap().grab_offset, None);
        assert!(state.move_pointer(px(60.), true));
        assert_eq!(state.scroll.handle().offset().y, px(0.));
        assert!(state.release());

        state.begin(px(15.));
        state.update_geometry(geometry(80., 20.));
        state.set_offset(px(-900.));
        assert_eq!(state.scroll.handle().offset().y, px(0.));
        assert!(state.release());
        state.begin(px(15.));
        assert!(state.gesture.is_none());
    }
}
