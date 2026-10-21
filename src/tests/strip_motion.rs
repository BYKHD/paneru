//! Emaki prototype (#37): strip motion during live resizes. Throwaway.
use std::time::Duration;

use bevy::prelude::*;
use objc2_core_foundation::CGPoint;

use super::*;
use crate::commands::Command;
use crate::ecs::Position;
use crate::ecs::edge::{EdgeParked, EdgePlugin};
use crate::events::Event;
use crate::manager::{Origin, Size};
use crate::platform::Modifiers;

fn harness(windows: i32) -> TestHarness {
    let neighbour = IRect::new(TEST_DISPLAY_WIDTH, 0, TEST_DISPLAY_WIDTH + 1920, 1080);
    let mut h = TestHarness::new()
        .with_display(EXT_DISPLAY_ID, neighbour, vec![EXT_WORKSPACE_ID])
        .with_windows(windows);
    h.app.add_plugins(EdgePlugin);
    h.run(vec![Event::Command {
        command: Command::PrintState,
    }]);
    h
}

fn x_of(h: &mut TestHarness, id: i32) -> i32 {
    let e = find_window_entity(id, h.world());
    h.world().get::<Position>(e).unwrap().0.x
}

/// macOS resizes `id` by its right edge in steps of `step`, reporting the
/// origin `jitter` px off (as some apps do), with the button held.
fn right_edge_drag(h: &mut TestHarness, id: i32, steps: i32, step: i32, jitter: i32) {
    let m = Modifiers::empty();
    let e = find_window_entity(id, h.world());
    let Origin { x, y } = h.world().get::<Position>(e).unwrap().0;
    let size = h.world().get::<crate::ecs::Bounds>(e).unwrap().0;
    let at = CGPoint::new(f64::from(x + size.x), f64::from(y + 300));
    h.world().write_message(Event::MouseDown {
        point: at,
        modifiers: m,
    });
    h.advance(Duration::from_millis(16));
    for i in 1..=steps {
        if jitter != 0 {
            h.mock_state.os_move_window(id, Origin::new(x + jitter, y));
        }
        h.mock_state
            .os_resize_window(id, Size::new(size.x + step * i, size.y));
        h.world().write_message(Event::MouseDragged {
            point: at,
            modifiers: m,
        });
        h.advance(Duration::from_millis(16));
    }
    // The layout follows the last event a frame later; the button stays held.
    h.advance(Duration::from_millis(32));
}

fn release(h: &mut TestHarness) {
    let m = Modifiers::empty();
    h.world().write_message(Event::MouseUp {
        point: CGPoint::new(0.0, 300.0),
        modifiers: m,
    });
    h.advance(Duration::from_millis(1500));
}

/// The owner saw neighbours squeezed from both sides. A right-edge drag whose
/// origin reads 1 px off used to shift the strip by the whole size change.
#[test]
fn a_right_edge_drag_leaves_the_left_side_still() {
    let mut h = harness(3);
    let left = x_of(&mut h, 0);
    right_edge_drag(&mut h, 1, 6, 30, 1);
    assert_eq!(
        x_of(&mut h, 0),
        left,
        "left neighbour moved during the drag"
    );
    release(&mut h);
}

/// Live resizes used to suspend every handoff, so a neighbour pushed past the
/// shared edge spilled onto the other display. Only the resized window is the
/// user's now; the neighbour parks.
#[test]
fn a_neighbour_pushed_past_the_shared_edge_parks() {
    let mut h = harness(3);
    // Columns at 0, 400, 800; the third reaches 1200, past the 1024 edge.
    let third = find_window_entity(2, h.world());
    let was_parked = h.world().get::<EdgeParked>(third).is_some();
    right_edge_drag(&mut h, 1, 10, 30, 0);
    assert_eq!(x_of(&mut h, 2), 800 + 300, "neighbour tracks the edge");
    assert!(
        was_parked || h.world().get::<EdgeParked>(third).is_some(),
        "pushed to 1100..1500, it must not stay real past the shared edge"
    );
    release(&mut h);
}

/// Option-drag (with Cmd: without activating) resizes about the centre: both
/// edges move, so each side's neighbours follow their edge, by half.
#[test]
fn a_centred_resize_moves_both_sides_by_half() {
    let mut h = harness(3);
    let (left, right) = (x_of(&mut h, 0), x_of(&mut h, 2));
    let m = Modifiers::empty();
    let e = find_window_entity(1, h.world());
    let Origin { x, y } = h.world().get::<Position>(e).unwrap().0;
    let size = h.world().get::<crate::ecs::Bounds>(e).unwrap().0;
    let at = CGPoint::new(f64::from(x + size.x), f64::from(y + 300));
    h.world().write_message(Event::MouseDown {
        point: at,
        modifiers: m,
    });
    h.advance(Duration::from_millis(16));
    for i in 1..=6 {
        h.mock_state.os_move_window(1, Origin::new(x - 10 * i, y));
        h.mock_state
            .os_resize_window(1, Size::new(size.x + 20 * i, size.y));
        h.world().write_message(Event::MouseDragged {
            point: at,
            modifiers: m,
        });
        h.advance(Duration::from_millis(16));
    }
    h.advance(Duration::from_millis(32));
    assert_eq!(
        x_of(&mut h, 0),
        left - 60,
        "left side follows the left edge"
    );
    assert_eq!(x_of(&mut h, 1), x - 60, "the window keeps its centre");
    assert_eq!(
        x_of(&mut h, 2),
        right + 60,
        "right side follows the right edge"
    );
    release(&mut h);
}

#[test]
fn a_resize_reported_twice_shifts_the_strip_once() {
    // macOS can deliver one centred resize as several `WindowResized`
    // events before the layout catches up; seen live on 2026-10-21.
    let mut h = harness(3);
    let (left, right) = (x_of(&mut h, 0), x_of(&mut h, 2));
    let m = Modifiers::empty();
    let e = find_window_entity(1, h.world());
    let Origin { x, y } = h.world().get::<Position>(e).unwrap().0;
    let size = h.world().get::<crate::ecs::Bounds>(e).unwrap().0;
    let at = CGPoint::new(f64::from(x + size.x), f64::from(y + 300));
    h.world().write_message(Event::MouseDown {
        point: at,
        modifiers: m,
    });
    h.advance(Duration::from_millis(16));
    for i in 1..=6 {
        // A centred resize arrives as resize events alone, no `WindowMoved`.
        h.mock_state.update_window(1, |w| {
            w.frame = IRect::new(x - 10 * i, y, x + size.x + 10 * i, y + size.y);
        });
        h.mock_state
            .os_resize_window(1, Size::new(size.x + 20 * i, size.y));
        h.mock_state
            .os_resize_window(1, Size::new(size.x + 20 * i, size.y));
        h.world().write_message(Event::MouseDragged {
            point: at,
            modifiers: m,
        });
        h.advance(Duration::from_millis(16));
    }
    h.advance(Duration::from_millis(32));
    assert_eq!(
        x_of(&mut h, 0),
        left - 60,
        "left side follows the left edge"
    );
    assert_eq!(x_of(&mut h, 1), x - 60, "the window keeps its centre");
    assert_eq!(
        x_of(&mut h, 2),
        right + 60,
        "right side follows the right edge"
    );
    release(&mut h);
}

fn strip_of(h: &mut TestHarness, id: i32) -> Option<u64> {
    let e = find_window_entity(id, h.world());
    let world = h.world();
    world
        .query::<&crate::ecs::layout::LayoutStrip>()
        .iter(world)
        .find(|strip| strip.contains(e))
        .map(crate::ecs::layout::LayoutStrip::id)
}

#[test]
fn a_window_moved_in_mission_control_joins_its_new_display_once_it_closes() {
    // macOS moves the real window during a Mission Control drag, and the
    // active display flips back and forth; seen live on 2026-10-22.
    let mut h = harness(3);
    assert_eq!(strip_of(&mut h, 2), Some(TEST_WORKSPACE_ID));
    h.world()
        .insert_resource(crate::ecs::MissionControlActive(true));
    h.advance(Duration::from_millis(32));
    h.mock_state.update_window(2, |w| {
        w.workspace_id = EXT_WORKSPACE_ID;
        w.frame = IRect::new(TEST_DISPLAY_WIDTH + 100, 0, TEST_DISPLAY_WIDTH + 600, 1080);
    });
    h.advance(Duration::from_millis(100));
    assert_eq!(
        strip_of(&mut h, 2),
        Some(TEST_WORKSPACE_ID),
        "nothing moves while open"
    );
    h.world()
        .insert_resource(crate::ecs::MissionControlActive(false));
    h.advance(Duration::from_millis(100));
    assert_eq!(strip_of(&mut h, 2), Some(EXT_WORKSPACE_ID));
    assert_eq!(
        strip_of(&mut h, 1),
        Some(TEST_WORKSPACE_ID),
        "the rest stay"
    );
}
