//! The popup window's mouse gestures as pure geometry: which part of the
//! frame a press landed on, and what a drag from there does to the frame.
//!
//! Nothing here touches state, PTYs or the screen. Deltas are taken from the
//! press anchor and the frame as it was at the press, never from the previous
//! motion event: a motion the terminal coalesced or dropped changes nothing
//! about where the frame ends up (TP-POPUP-MOVE-01, TP-POPUP-RESIZE-01/02).

use ratatui::layout::Rect;

use crate::popup_size::PopupPosition;

/// The smallest outer frame the resolver accepts (TP-CHROME-48).
pub(crate) const POPUP_MIN_OUTER_WIDTH: u16 = 6;
pub(crate) const POPUP_MIN_OUTER_HEIGHT: u16 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PopupCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// The three edges that resize. The top edge is the title bar, and it moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PopupEdge {
    Left,
    Right,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PopupResizeHandle {
    Corner(PopupCorner),
    Edge(PopupEdge),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PopupZone {
    /// The top border between the corners: the move handle.
    Title,
    Resize(PopupResizeHandle),
    /// The program's own cells — forwarded, never a gesture.
    Inside,
    Outside,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PopupGestureKind {
    Move,
    Resize(PopupResizeHandle),
}

/// A gesture in flight: what it does, where it was pressed, and the frame it
/// started from. Ephemeral per display (HP18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PopupGesture {
    pub kind: PopupGestureKind,
    pub anchor_column: u16,
    pub anchor_row: u16,
    pub origin: Rect,
}

/// Where the frame goes and how big it is after a motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PopupGestureGeometry {
    pub position: PopupPosition,
    pub width: u16,
    pub height: u16,
}

/// Which part of the frame a cell belongs to. The border is one cell thick,
/// so a corner is the cell where two borders meet and wins over both edges;
/// everything inside the border — the program's cells and the scrollbar
/// column the resolver keeps beside them — is `Inside`.
impl PopupGestureGeometry {
    /// The frame exactly as drawn: what a gesture starts from.
    pub(crate) fn of(outer: Rect) -> Self {
        Self {
            position: PopupPosition {
                x: outer.x,
                y: outer.y,
            },
            width: outer.width,
            height: outer.height,
        }
    }
}

/// A gesture and the frame it has reached so far: what the renderer draws
/// while the button is held, and what the release commits. Ephemeral per
/// display (HP18) — the display that pressed is the only one that can let go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PopupGestureState {
    pub gesture: PopupGesture,
    pub geometry: PopupGestureGeometry,
}

impl PopupGestureState {
    /// A press at (`column`, `row`) on a frame drawn at `outer`.
    pub(crate) fn begin(kind: PopupGestureKind, column: u16, row: u16, outer: Rect) -> Self {
        Self {
            gesture: PopupGesture {
                kind,
                anchor_column: column,
                anchor_row: row,
                origin: outer,
            },
            geometry: PopupGestureGeometry::of(outer),
        }
    }

    /// The frame after the pointer reached (`column`, `row`).
    pub(crate) fn reach(&mut self, column: u16, row: u16, area: Rect) {
        self.geometry = apply_popup_gesture(&self.gesture, column, row, area);
    }

    /// Whether the frame is anywhere other than where the press found it.
    /// A click that never moved commits nothing (TP-POPUP-MOVE-02).
    pub(crate) fn changed(&self) -> bool {
        self.geometry != PopupGestureGeometry::of(self.gesture.origin)
    }
}

/// What a press on `zone` starts, if anything: the title bar moves, the
/// handles resize, and the inside and the outside are not gestures.
pub(crate) fn gesture_for_zone(zone: PopupZone) -> Option<PopupGestureKind> {
    match zone {
        PopupZone::Title => Some(PopupGestureKind::Move),
        PopupZone::Resize(handle) => Some(PopupGestureKind::Resize(handle)),
        PopupZone::Inside | PopupZone::Outside => None,
    }
}

pub(crate) fn classify_popup_zone(column: u16, row: u16, outer: Rect) -> PopupZone {
    if outer.width == 0 || outer.height == 0 {
        return PopupZone::Outside;
    }
    let right = outer.x.saturating_add(outer.width).saturating_sub(1);
    let bottom = outer.y.saturating_add(outer.height).saturating_sub(1);
    if column < outer.x || column > right || row < outer.y || row > bottom {
        return PopupZone::Outside;
    }
    let on_left = column == outer.x;
    let on_right = column == right;
    let on_top = row == outer.y;
    let on_bottom = row == bottom;
    use PopupCorner::*;
    use PopupEdge::*;
    use PopupResizeHandle::*;
    match (on_left, on_right, on_top, on_bottom) {
        (true, _, true, _) => PopupZone::Resize(Corner(TopLeft)),
        (_, true, true, _) => PopupZone::Resize(Corner(TopRight)),
        (true, _, _, true) => PopupZone::Resize(Corner(BottomLeft)),
        (_, true, _, true) => PopupZone::Resize(Corner(BottomRight)),
        (_, _, true, _) => PopupZone::Title,
        (_, _, _, true) => PopupZone::Resize(Edge(Bottom)),
        (true, _, _, _) => PopupZone::Resize(Edge(Left)),
        (_, true, _, _) => PopupZone::Resize(Edge(Right)),
        _ => PopupZone::Inside,
    }
}

/// Where the frame is after the pointer reached (`column`, `row`).
///
/// A move carries the whole frame by the press delta and keeps it inside the
/// area. A resize moves only the pressed edges by the delta, keeps the frame
/// at least the minimum the resolver accepts, and stops at the area's edges;
/// the opposite edge never moves, so shrinking from the left keeps the right
/// edge where it was.
pub(crate) fn apply_popup_gesture(
    gesture: &PopupGesture,
    column: u16,
    row: u16,
    area: Rect,
) -> PopupGestureGeometry {
    let dx = i32::from(column) - i32::from(gesture.anchor_column);
    let dy = i32::from(row) - i32::from(gesture.anchor_row);
    let area_left = i32::from(area.x);
    let area_top = i32::from(area.y);
    let area_right = area_left + i32::from(area.width);
    let area_bottom = area_top + i32::from(area.height);
    let min_width = i32::from(POPUP_MIN_OUTER_WIDTH);
    let min_height = i32::from(POPUP_MIN_OUTER_HEIGHT);

    let origin = gesture.origin;
    let mut left = i32::from(origin.x);
    let mut top = i32::from(origin.y);
    let mut right = left + i32::from(origin.width);
    let mut bottom = top + i32::from(origin.height);

    match gesture.kind {
        PopupGestureKind::Move => {
            let width = (right - left).min(area_right - area_left);
            let height = (bottom - top).min(area_bottom - area_top);
            left = bounded(left + dx, area_left, area_right - width);
            top = bounded(top + dy, area_top, area_bottom - height);
            right = left + width;
            bottom = top + height;
        }
        PopupGestureKind::Resize(handle) => {
            use PopupCorner::*;
            use PopupEdge::*;
            use PopupResizeHandle::*;
            let (moves_left, moves_right, moves_top, moves_bottom) = match handle {
                Corner(TopLeft) => (true, false, true, false),
                Corner(TopRight) => (false, true, true, false),
                Corner(BottomLeft) => (true, false, false, true),
                Corner(BottomRight) => (false, true, false, true),
                Edge(Left) => (true, false, false, false),
                Edge(Right) => (false, true, false, false),
                Edge(Bottom) => (false, false, false, true),
            };
            if moves_left {
                left = bounded(left + dx, area_left, right - min_width);
            }
            if moves_right {
                right = bounded(right + dx, left + min_width, area_right);
            }
            if moves_top {
                top = bounded(top + dy, area_top, bottom - min_height);
            }
            if moves_bottom {
                bottom = bounded(bottom + dy, top + min_height, area_bottom);
            }
        }
    }

    PopupGestureGeometry {
        position: PopupPosition {
            x: cells(left),
            y: cells(top),
        },
        width: cells(right - left),
        height: cells(bottom - top),
    }
}

/// `value` held between `low` and `high`, with `high` winning when the two
/// cross — a frame that cannot honour both bounds keeps its size rather
/// than panicking the way `clamp` would.
fn bounded(value: i32, low: i32, high: i32) -> i32 {
    let value = if value < low { low } else { value };
    if value > high {
        high
    } else {
        value
    }
}

fn cells(value: i32) -> u16 {
    u16::try_from(value.max(0)).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Rect {
        // outer 20×10 at (10,5). The resolver keeps one column inside the
        // frame for the scrollbar; to the classifier it is Inside like the
        // program's own cells.
        Rect::new(10, 5, 20, 10)
    }

    fn gesture(kind: PopupGestureKind, anchor: (u16, u16)) -> PopupGesture {
        PopupGesture {
            kind,
            anchor_column: anchor.0,
            anchor_row: anchor.1,
            origin: frame(),
        }
    }

    // TB2.1 — every cell of the frame has exactly one meaning.
    #[test]
    fn zones_cover_corners_title_edges_inside_and_outside() {
        let outer = frame();
        use PopupCorner::*;
        use PopupEdge::*;
        use PopupResizeHandle::*;
        let cases = [
            ((10, 5), PopupZone::Resize(Corner(TopLeft))),
            ((29, 5), PopupZone::Resize(Corner(TopRight))),
            ((10, 14), PopupZone::Resize(Corner(BottomLeft))),
            ((29, 14), PopupZone::Resize(Corner(BottomRight))),
            ((11, 5), PopupZone::Title),
            ((28, 5), PopupZone::Title),
            ((10, 6), PopupZone::Resize(Edge(Left))),
            ((29, 13), PopupZone::Resize(Edge(Right))),
            ((20, 14), PopupZone::Resize(Edge(Bottom))),
            ((15, 8), PopupZone::Inside),
            ((28, 8), PopupZone::Inside),
            ((9, 5), PopupZone::Outside),
            ((30, 14), PopupZone::Outside),
            ((20, 15), PopupZone::Outside),
        ];
        for ((column, row), expected) in cases {
            assert_eq!(
                classify_popup_zone(column, row, outer),
                expected,
                "cell ({column},{row})"
            );
        }
        // The smallest frame: corners win the fight for the four cells.
        let tiny = Rect::new(0, 0, 6, 4);
        assert_eq!(
            classify_popup_zone(5, 3, tiny),
            PopupZone::Resize(Corner(BottomRight))
        );
    }

    // TB2.2 — a move is the press delta applied to the frame's origin, and
    // the same motion twice lands in the same place (absolute anchor).
    #[test]
    fn move_translates_by_the_press_delta_and_clamps_to_the_area() {
        let area = Rect::new(0, 0, 80, 24);
        let g = gesture(PopupGestureKind::Move, (15, 5));
        let moved = apply_popup_gesture(&g, 18, 7, area);
        assert_eq!(moved.position, PopupPosition { x: 13, y: 7 });
        assert_eq!((moved.width, moved.height), (20, 10));
        assert_eq!(
            apply_popup_gesture(&g, 18, 7, area),
            moved,
            "no drift on repeats"
        );
        // Off the left/top: pinned to the area's origin. Off the right/bottom:
        // pinned so the frame stays whole.
        assert_eq!(
            apply_popup_gesture(&g, 0, 0, area).position,
            PopupPosition { x: 0, y: 0 }
        );
        assert_eq!(
            apply_popup_gesture(&g, 79, 23, area).position,
            PopupPosition { x: 60, y: 14 }
        );
    }

    // TB2.3 — the bottom-right corner grows the frame in place.
    #[test]
    fn bottom_right_resize_keeps_the_origin_and_grows() {
        let area = Rect::new(0, 0, 80, 24);
        let g = gesture(
            PopupGestureKind::Resize(PopupResizeHandle::Corner(PopupCorner::BottomRight)),
            (29, 14),
        );
        let grown = apply_popup_gesture(&g, 33, 15, area);
        assert_eq!(grown.position, PopupPosition { x: 10, y: 5 });
        assert_eq!((grown.width, grown.height), (24, 11));
    }

    // TB2.4 — the top-left corner moves the origin and grows the other way.
    #[test]
    fn top_left_resize_moves_the_origin_and_grows_toward_it() {
        let area = Rect::new(0, 0, 80, 24);
        let g = gesture(
            PopupGestureKind::Resize(PopupResizeHandle::Corner(PopupCorner::TopLeft)),
            (10, 5),
        );
        let grown = apply_popup_gesture(&g, 8, 4, area);
        assert_eq!(grown.position, PopupPosition { x: 8, y: 4 });
        assert_eq!((grown.width, grown.height), (22, 11));
        // Edges: only their own axis moves.
        let left = gesture(
            PopupGestureKind::Resize(PopupResizeHandle::Edge(PopupEdge::Left)),
            (10, 8),
        );
        let shrunk = apply_popup_gesture(&left, 12, 20, area);
        assert_eq!(shrunk.position, PopupPosition { x: 12, y: 5 });
        assert_eq!((shrunk.width, shrunk.height), (18, 10));
        let bottom = gesture(
            PopupGestureKind::Resize(PopupResizeHandle::Edge(PopupEdge::Bottom)),
            (20, 14),
        );
        let taller = apply_popup_gesture(&bottom, 3, 16, area);
        assert_eq!(taller.position, PopupPosition { x: 10, y: 5 });
        assert_eq!((taller.width, taller.height), (20, 12));
    }

    // TB2.5/TB2.6 — the frame never collapses below the minimum and never
    // grows past the area; a left-edge drag that would collapse it stops at
    // the minimum, with the origin stopping accordingly.
    #[test]
    fn resize_is_clamped_to_the_minimum_and_the_area() {
        let area = Rect::new(0, 0, 80, 24);
        let br = gesture(
            PopupGestureKind::Resize(PopupResizeHandle::Corner(PopupCorner::BottomRight)),
            (29, 14),
        );
        let tiny = apply_popup_gesture(&br, 0, 0, area);
        assert_eq!(
            (tiny.width, tiny.height),
            (POPUP_MIN_OUTER_WIDTH, POPUP_MIN_OUTER_HEIGHT)
        );
        let huge = apply_popup_gesture(&br, 200, 200, area);
        assert_eq!(
            (huge.width, huge.height),
            (70, 19),
            "stops at the area's edge"
        );
        let left = gesture(
            PopupGestureKind::Resize(PopupResizeHandle::Edge(PopupEdge::Left)),
            (10, 8),
        );
        let collapsed = apply_popup_gesture(&left, 79, 8, area);
        assert_eq!(collapsed.width, POPUP_MIN_OUTER_WIDTH);
        assert_eq!(
            collapsed.position.x,
            30 - POPUP_MIN_OUTER_WIDTH,
            "the right edge stays put"
        );
        let tl = gesture(
            PopupGestureKind::Resize(PopupResizeHandle::Corner(PopupCorner::TopLeft)),
            (10, 5),
        );
        let pinned = apply_popup_gesture(&tl, 0, 0, area);
        assert_eq!(pinned.position, PopupPosition { x: 0, y: 0 });
        assert_eq!((pinned.width, pinned.height), (30, 15));
    }
}
