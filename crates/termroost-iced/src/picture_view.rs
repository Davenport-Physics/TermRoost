//! A picture, fit to its space or zoomed. The wheel zooms around the
//! pointer, stopping at 100% and at the fit on the way; dragging moves a
//! picture larger than its space; a double click goes between the fit and a
//! closer look. A checkerboard shows through where the picture is see-through.
//!
//! The zoom is kept by the app (see [`Zoom`]); the widget only reports
//! changes to it.

use iced::advanced::image::{self as raster, FilterMethod, Renderer as _};
use iced::advanced::layout::{self, Layout};
use iced::advanced::mouse::{self, click};
use iced::advanced::renderer::{self, Quad, Renderer as _};
use iced::advanced::svg::{self, Renderer as _};
use iced::advanced::widget::{self, Widget, tree};
use iced::advanced::{Clipboard, Shell};
use iced::{Element, Event, Length, Point, Radians, Rectangle, Size, Theme, Vector};

use crate::editor::picture::{Image, Picture, Zoom};
use crate::theme;

/// Space kept around a picture fit to its space.
const MARGIN: f32 = 12.0;
/// The most a picture is enlarged.
const MAX_SCALE: f32 = 32.0;
/// An SVG is drawn whole at the size it is shown, so it is shown no larger
/// than this on its longer side.
const MAX_VECTOR_SIDE: f32 = 4096.0;
/// How much one notch of the wheel zooms.
const STEP: f32 = 1.2;
/// Wheel travel in pixels (from a touchpad) that counts as one notch.
const NOTCH_PIXELS: f32 = 50.0;
/// Side of a checkerboard square.
const CHECK: f32 = 8.0;

pub struct PictureView<'a, M> {
    picture: &'a Picture,
    zoom: Zoom,
    on_zoom: Box<dyn Fn(Zoom) -> M + 'a>,
}

impl<'a, M> PictureView<'a, M> {
    pub fn new(picture: &'a Picture, zoom: Zoom, on_zoom: impl Fn(Zoom) -> M + 'a) -> Self {
        PictureView { picture, zoom, on_zoom: Box::new(on_zoom) }
    }

    fn frame(&self) -> Frame {
        let natural = Size::new(self.picture.width as f32, self.picture.height as f32);
        Frame { natural, vector: matches!(self.picture.image, Image::Vector(_)) }
    }
}

/// A picture's own size, and whether it can be drawn at any size.
#[derive(Clone, Copy, Debug)]
struct Frame {
    natural: Size,
    vector: bool,
}

/// Where a picture goes in its space.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Placed {
    scale: f32,
    /// The scale that fits it.
    fit: f32,
    offset: Vector,
    rect: Rectangle,
}

impl Frame {
    /// The scale that fits the picture into `space`, less a margin: rasters
    /// are only made smaller, while vectors are drawn as large as fits.
    fn fit(self, space: Size) -> f32 {
        let room = Size::new((space.width - 2.0 * MARGIN).max(1.0), (space.height - 2.0 * MARGIN).max(1.0));
        let fit = (room.width / self.natural.width).min(room.height / self.natural.height);
        if self.vector { fit.min(self.max_scale()) } else { fit.min(1.0) }
    }

    fn max_scale(self) -> f32 {
        if self.vector {
            (MAX_VECTOR_SIDE / self.natural.width.max(self.natural.height)).min(MAX_SCALE)
        } else {
            MAX_SCALE
        }
    }

    /// Smaller than the fit is no use, except to see a large vector at 100%.
    fn min_scale(fit: f32) -> f32 {
        fit.min(1.0)
    }

    fn place(self, zoom: Zoom, bounds: Rectangle) -> Placed {
        let fit = self.fit(bounds.size());
        let scale = zoom.scale.unwrap_or(fit).clamp(Self::min_scale(fit), self.max_scale().max(fit));
        let shown = Size::new(self.natural.width * scale, self.natural.height * scale);
        let offset = clamp_offset(zoom.offset, shown, bounds.size());
        let center = bounds.center() + offset;
        // On whole pixels, so a picture at 100% stays sharp.
        let top_left = Point::new((center.x - shown.width / 2.0).round(), (center.y - shown.height / 2.0).round());
        Placed { scale, fit, offset, rect: Rectangle::new(top_left, shown) }
    }

    /// The zoom at `scale`, keeping the point of the picture at `at` (from
    /// the middle of `bounds`) where it is. Landing on the fit fits it.
    fn zoom_at(self, placed: &Placed, scale: f32, at: Vector, bounds: Rectangle) -> Zoom {
        if (scale - placed.fit).abs() < 1e-4 {
            return Zoom::default();
        }
        let offset = at - (at - placed.offset) * (scale / placed.scale);
        let shown = Size::new(self.natural.width * scale, self.natural.height * scale);
        Zoom { scale: Some(scale), offset: clamp_offset(offset, shown, bounds.size()) }
    }

    /// The scale `notches` of the wheel away (up is closer), stopping at
    /// 100% and at the fit when passing them.
    fn wheel_scale(self, placed: &Placed, notches: f32) -> f32 {
        let from = placed.scale;
        let to = (from * STEP.powf(notches)).clamp(Self::min_scale(placed.fit), self.max_scale().max(placed.fit));
        let passed = |stop: f32| (from < stop && stop < to) || (to < stop && stop < from);
        [1.0, placed.fit]
            .into_iter()
            .filter(|s| passed(*s))
            .min_by(|a, b| (a - from).abs().total_cmp(&(b - from).abs()))
            .unwrap_or(to)
    }

    /// The scale a double click goes to: from the fit, 100% (or twice the
    /// fit, if that is 100% or more already); from anywhere else, the fit.
    fn double_click_scale(self, zoom: Zoom, placed: &Placed) -> f32 {
        if zoom.scale.is_some() {
            return placed.fit;
        }
        let closer = if placed.fit < 1.0 { 1.0 } else { placed.fit * 2.0 };
        closer.min(self.max_scale().max(placed.fit))
    }
}

/// Keeps a picture shown at `shown` size from leaving a gap at an edge of
/// `space` it is larger than, and in the middle where it is smaller.
fn clamp_offset(offset: Vector, shown: Size, space: Size) -> Vector {
    let free = |shown: f32, space: f32, at: f32| {
        let room = ((shown - space) / 2.0).max(0.0);
        at.clamp(-room, room)
    };
    Vector::new(free(shown.width, space.width, offset.x), free(shown.height, space.height, offset.y))
}

#[derive(Default)]
struct State {
    /// Where a drag started, and the offset then.
    grab: Option<(Point, Vector)>,
    last_click: Option<mouse::Click>,
}

impl<M> Widget<M, Theme, iced::Renderer> for PictureView<'_, M> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn layout(
        &mut self,
        _tree: &mut widget::Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
    }

    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, M>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        let bounds = layout.bounds();
        let frame = self.frame();
        let placed = frame.place(self.zoom, bounds);
        match event {
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let Some(at) = cursor.position_over(bounds) else { return };
                let notches = match *delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / NOTCH_PIXELS,
                };
                let scale = frame.wheel_scale(&placed, notches);
                let zoom = frame.zoom_at(&placed, scale, at - bounds.center(), bounds);
                if zoom != self.zoom {
                    shell.publish((self.on_zoom)(zoom));
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(at) = cursor.position_over(bounds) else { return };
                let click = mouse::Click::new(at, mouse::Button::Left, state.last_click);
                state.last_click = Some(click);
                if click.kind() == click::Kind::Double {
                    state.grab = None;
                    let scale = frame.double_click_scale(self.zoom, &placed);
                    shell.publish((self.on_zoom)(frame.zoom_at(&placed, scale, at - bounds.center(), bounds)));
                } else {
                    state.grab = Some((at, placed.offset));
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let Some((from, start)) = state.grab else { return };
                let offset = clamp_offset(start + (*position - from), placed.rect.size(), bounds.size());
                if offset != placed.offset {
                    shell.publish((self.on_zoom)(Zoom { scale: self.zoom.scale, offset }));
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => state.grab = None,
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &widget::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State>();
        if state.grab.is_some() {
            return mouse::Interaction::Grabbing;
        }
        let bounds = layout.bounds();
        let shown = self.frame().place(self.zoom, bounds).rect;
        // A hand where there is something to move.
        if cursor.is_over(bounds) && (shown.width > bounds.width || shown.height > bounds.height) {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(
        &self,
        _tree: &widget::Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let placed = self.frame().place(self.zoom, bounds);
        let Some(visible) = placed.rect.intersection(&bounds) else { return };
        renderer.with_layer(bounds, |r| {
            if self.picture.transparent {
                checkerboard(r, theme, placed.rect, visible);
            }
            match &self.picture.image {
                Image::Raster(handle) => {
                    // Enlarged, its pixels show as squares, as one zooms in to see them.
                    let filter_method = if placed.scale >= 1.0 { FilterMethod::Nearest } else { FilterMethod::Linear };
                    let image = raster::Image {
                        handle: handle.clone(),
                        filter_method,
                        rotation: Radians(0.0),
                        border_radius: Default::default(),
                        opacity: 1.0,
                        snap: true,
                    };
                    r.draw_image(image, placed.rect, bounds);
                }
                Image::Vector(handle) => r.draw_svg(svg::Svg::new(handle.clone()), placed.rect, bounds),
            }
        });
    }
}

/// Squares under the part of `rect` that is `visible`, lined up with `rect`
/// so they move with the picture.
fn checkerboard(r: &mut iced::Renderer, theme: &Theme, rect: Rectangle, visible: Rectangle) {
    let (light, dark) = theme::checkerboard(theme);
    r.fill_quad(Quad { bounds: visible, ..Quad::default() }, light);
    let cells = |from: f32, to: f32, origin: f32| {
        ((from - origin) / CHECK).floor() as i64..((to - origin) / CHECK).ceil() as i64
    };
    for row in cells(visible.y, visible.y + visible.height, rect.y) {
        for col in cells(visible.x, visible.x + visible.width, rect.x).filter(|col| (row + col) % 2 != 0) {
            let cell = Rectangle {
                x: rect.x + col as f32 * CHECK,
                y: rect.y + row as f32 * CHECK,
                width: CHECK,
                height: CHECK,
            };
            if let Some(bounds) = cell.intersection(&visible) {
                r.fill_quad(Quad { bounds, ..Quad::default() }, dark);
            }
        }
    }
}

impl<'a, M: 'a> From<PictureView<'a, M>> for Element<'a, M> {
    fn from(view: PictureView<'a, M>) -> Self {
        Element::new(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPACE: Rectangle = Rectangle { x: 100.0, y: 50.0, width: 824.0, height: 624.0 };

    fn raster(width: f32, height: f32) -> Frame {
        Frame { natural: Size::new(width, height), vector: false }
    }

    fn vector(width: f32, height: f32) -> Frame {
        Frame { natural: Size::new(width, height), vector: true }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn large_pictures_are_fit_and_small_ones_kept_at_100() {
        // 800 x 600 of room inside the margin.
        let big = raster(1600.0, 900.0).place(Zoom::default(), SPACE);
        assert!(close(big.scale, 0.5), "{big:?}");
        assert_eq!(big.rect.size(), Size::new(800.0, 450.0));
        assert_eq!(big.rect.center(), SPACE.center(), "in the middle");
        let small = raster(16.0, 16.0).place(Zoom::default(), SPACE);
        assert!(close(small.scale, 1.0), "not enlarged");
    }

    #[test]
    fn vectors_are_drawn_as_large_as_fits() {
        let icon = vector(48.0, 24.0).place(Zoom::default(), SPACE);
        assert!(close(icon.scale, 800.0 / 48.0), "{icon:?}");
        let tiny = vector(1.0, 1.0);
        assert!(close(tiny.fit(SPACE.size()), MAX_SCALE), "no larger than the most a picture is enlarged");
        let huge = vector(8192.0, 100.0);
        assert!(huge.max_scale() <= 0.5 + 1e-6, "kept below the largest drawable side");
    }

    #[test]
    fn a_zoomed_picture_moves_only_as_far_as_its_edges() {
        let frame = raster(2000.0, 400.0);
        let zoom = Zoom { scale: Some(1.0), offset: Vector::new(5000.0, 80.0) };
        let placed = frame.place(zoom, SPACE);
        // 2000 wide in 824: 588 to either side; 400 high in 624: centered.
        assert_eq!(placed.offset, Vector::new(588.0, 0.0));
        assert_eq!(placed.rect.x, SPACE.x, "its left edge at the space's");
    }

    #[test]
    fn zooming_keeps_the_point_under_the_pointer() {
        let frame = raster(4000.0, 3000.0);
        let placed = frame.place(Zoom { scale: Some(0.5), offset: Vector::ZERO }, SPACE);
        let at = Vector::new(100.0, -50.0);
        let picture_point = |p: &Placed| (at - p.offset) * (1.0 / p.scale);
        let zoom = frame.zoom_at(&placed, 1.0, at, SPACE);
        let after = frame.place(zoom, SPACE);
        let (a, b) = (picture_point(&placed), picture_point(&after));
        assert!(close(a.x, b.x) && close(a.y, b.y), "{a:?} then {b:?}");
    }

    #[test]
    fn the_wheel_stops_at_100_and_at_the_fit() {
        let frame = raster(4000.0, 3000.0);
        let fit = frame.place(Zoom::default(), SPACE);
        assert!(close(fit.scale, 0.2));
        let near = frame.place(Zoom { scale: Some(0.9), offset: Vector::ZERO }, SPACE);
        assert_eq!(frame.wheel_scale(&near, 1.0), 1.0, "up past 100%");
        let above = frame.place(Zoom { scale: Some(0.22), offset: Vector::ZERO }, SPACE);
        let back = frame.wheel_scale(&above, -1.0);
        assert!(close(back, fit.fit), "down onto the fit");
        assert_eq!(frame.zoom_at(&above, back, Vector::ZERO, SPACE), Zoom::default(), "which fits it again");
        assert!(close(frame.wheel_scale(&fit, -3.0), fit.fit), "no smaller than the fit");
        let most = frame.place(Zoom { scale: Some(MAX_SCALE), offset: Vector::ZERO }, SPACE);
        assert_eq!(frame.wheel_scale(&most, 1.0), MAX_SCALE);
    }

    #[test]
    fn a_double_click_goes_between_the_fit_and_a_closer_look() {
        let photo = raster(4000.0, 3000.0);
        let fit = photo.place(Zoom::default(), SPACE);
        assert_eq!(photo.double_click_scale(Zoom::default(), &fit), 1.0);
        let zoomed = Zoom { scale: Some(3.0), offset: Vector::ZERO };
        assert!(close(photo.double_click_scale(zoomed, &photo.place(zoomed, SPACE)), fit.fit));
        let icon = raster(16.0, 16.0);
        assert_eq!(icon.double_click_scale(Zoom::default(), &icon.place(Zoom::default(), SPACE)), 2.0);
    }
}
