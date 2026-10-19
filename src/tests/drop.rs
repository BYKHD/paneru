//! Emaki prototype (#33): drop previews in the mock harness.
use std::time::Duration;

use bevy::prelude::*;
use objc2_core_foundation::CGPoint;

use super::*;
use crate::commands::Command;
use crate::ecs::Position;
use crate::ecs::drop::{DropDragged, DropPreviewPlugin};
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;
use crate::manager::Origin;
use crate::platform::Modifiers;

const EXT: IRect = IRect {
    min: IVec2::new(TEST_DISPLAY_WIDTH, 0),
    max: IVec2::new(TEST_DISPLAY_WIDTH + 1920, 1080),
};

fn harness() -> TestHarness {
    let mut harness = TestHarness::new()
        .with_display(EXT_DISPLAY_ID, EXT, vec![EXT_WORKSPACE_ID])
        .with_windows(3);
    harness.app.add_plugins(DropPreviewPlugin);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);
    harness
}

fn send(harness: &mut TestHarness, event: Event) {
    harness.world().write_message(event);
    harness.advance(Duration::from_millis(40));
}

fn at(x: i32, y: i32) -> CGPoint {
    CGPoint::new(f64::from(x), f64::from(y))
}

fn order(harness: &mut TestHarness, id: i32) -> Vec<i32> {
    let world = harness.world();
    let entity = find_window_entity(id, world);
    let strip = world
        .query::<&LayoutStrip>()
        .iter(world)
        .find(|strip| strip.contains(entity))
        .expect("strip")
        .all_windows();
    strip
        .into_iter()
        .map(|e| world.get::<crate::manager::Window>(e).unwrap().id())
        .collect()
}

fn x_of(harness: &mut TestHarness, id: i32) -> i32 {
    let world = harness.world();
    let entity = find_window_entity(id, world);
    world.get::<Position>(entity).unwrap().0.x
}

/// Presses on `id`, then macOS moves it as the title bar is dragged.
fn grab(harness: &mut TestHarness, id: i32) {
    let world = harness.world();
    let entity = find_window_entity(id, world);
    let Origin { x, y } = world.get::<Position>(entity).unwrap().0;
    let modifiers = Modifiers::empty();
    send(
        harness,
        Event::MouseDown {
            point: at(x + 200, y + 10),
            modifiers,
        },
    );
    harness
        .mock_state
        .os_move_window(id, Origin::new(x + 20, y));
    harness.advance(Duration::from_millis(40));
    send(
        harness,
        Event::MouseDragged {
            point: at(x + 220, y + 10),
            modifiers,
        },
    );
}

fn drag_to(harness: &mut TestHarness, id: i32, x: i32, y: i32) {
    harness
        .mock_state
        .os_move_window(id, Origin::new(x - 200, y));
    harness.advance(Duration::from_millis(20));
    let modifiers = Modifiers::empty();
    send(
        harness,
        Event::MouseDragged {
            point: at(x, y),
            modifiers,
        },
    );
    send(
        harness,
        Event::MouseDragged {
            point: at(x, y),
            modifiers,
        },
    );
}

fn release(harness: &mut TestHarness, x: i32, y: i32) {
    let modifiers = Modifiers::empty();
    send(
        harness,
        Event::MouseUp {
            point: at(x, y),
            modifiers,
        },
    );
    harness.advance(Duration::from_millis(1600));
}

#[test]
fn hover_opens_a_gap_and_release_commits_the_reorder() {
    let mut h = harness();
    let before = order(&mut h, 0);
    let (x0, x1) = (x_of(&mut h, before[0]), x_of(&mut h, before[1]));
    let first = before[0];
    grab(&mut h, first);
    let entity = find_window_entity(first, h.world());
    assert!(
        h.world().get::<DropDragged>(entity).is_some(),
        "drag started"
    );

    // Past the second column's centre: it slides left into the old slot.
    drag_to(&mut h, first, x1 + 300, 10);
    assert_eq!(order(&mut h, first), vec![before[1], first, before[2]]);
    assert_eq!(x_of(&mut h, before[1]), x0, "neighbour slid aside");

    release(&mut h, x1 + 300, 10);
    assert_eq!(order(&mut h, first), vec![before[1], first, before[2]]);
    assert_eq!(x_of(&mut h, first), x1, "landed in the gap");
    assert!(
        h.world().get::<DropDragged>(entity).is_none(),
        "landing over"
    );
}

#[test]
fn esc_restores_the_original_order() {
    let mut h = harness();
    let before = order(&mut h, 0);
    let x1 = x_of(&mut h, before[1]);
    let first = before[0];
    let home = x_of(&mut h, first);
    grab(&mut h, first);
    drag_to(&mut h, first, x1 + 300, 10);
    assert_ne!(order(&mut h, first), before);

    send(&mut h, Event::DropCancel);
    assert_eq!(order(&mut h, first), before, "cancel reopens the home slot");
    // Further hovering is ignored once cancelled.
    drag_to(&mut h, first, x1 + 300, 10);
    assert_eq!(order(&mut h, first), before);

    release(&mut h, x1 + 300, 10);
    assert_eq!(order(&mut h, first), before);
    assert_eq!(x_of(&mut h, first), home, "animated home");
}

#[test]
fn transfer_keeps_relative_width_and_closes_the_source_gap() {
    let mut h = harness();
    let before = order(&mut h, 0);
    let first = before[0];
    let x0 = x_of(&mut h, first);
    grab(&mut h, first);
    drag_to(&mut h, first, EXT.min.x + 300, 500);
    let source = order(&mut h, before[1]);
    assert_eq!(source, vec![before[1], before[2]], "one gap at a time");
    assert_eq!(x_of(&mut h, before[1]), x0, "source closed up");
    assert_eq!(
        order(&mut h, first),
        vec![first],
        "previewed on the other display"
    );

    release(&mut h, EXT.min.x + 300, 500);
    let world = h.world();
    let entity = find_window_entity(first, world);
    let width = world.get::<crate::ecs::Bounds>(entity).unwrap().0.x;
    let expected = (f64::from(TEST_WINDOW_WIDTH) / f64::from(TEST_DISPLAY_WIDTH) * 1920.0).round();
    assert!(
        (f64::from(width) - expected).abs() <= 2.0,
        "width {width} vs {expected}"
    );
    assert!(
        x_of(&mut h, first) >= EXT.min.x,
        "landed on the other display"
    );
}
