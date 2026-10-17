//! Emaki-owned Stop placement (BYKHD/Emaki#4): tiled windows return to their
//! pre-tiling origin on the display owning their strip; others stay put.
use bevy::app::AppExit;
use bevy::prelude::*;

use crate::commands::{Command, MoveFocus, Operation};
use crate::ecs::{OwnedEngine, Unmanaged};
use crate::events::Event;
use crate::manager::{Origin, Window};
use crate::platform::WinID;

use super::*;

const SIZE: (i32, i32) = (400, 500);

fn frame_at(x: i32, y: i32) -> IRect {
    IRect::new(x, y, x + SIZE.0, y + SIZE.1)
}

fn harness(owned: bool, windows: &[(WinID, i32, i32)]) -> TestHarness {
    let mut harness = TestHarness::new();
    if owned {
        harness.world().insert_resource(OwnedEngine);
    }
    for &(id, x, y) in windows {
        harness = harness.with_window(id, |w| w.frame = frame_at(x, y));
    }
    harness
}

fn origin(harness: &mut TestHarness, id: WinID) -> Origin {
    let world = harness.world();
    let mut query = world.query::<&Window>();
    let window = query.iter(world).find(|w| w.id() == id).expect("window");
    window.frame().min + Origin::new(window.horizontal_padding(), window.vertical_padding())
}

fn stop(harness: &mut TestHarness) {
    harness.world().write_message(AppExit::Success);
    harness.app.update();
}

fn print() -> Event {
    Event::Command {
        command: Command::PrintState,
    }
}

fn window_op(op: Operation) -> Event {
    Event::Command {
        command: Command::Window(op),
    }
}

#[test]
fn owned_stop_returns_tiled_windows_to_pre_tiling_origins_despite_padding() {
    let mut harness = harness(true, &[(0, 100, 120), (1, 300, 200)]);
    harness.run(vec![print()]);
    // Padding is applied after discovery, as apply_window_defaults does.
    for id in [0, 1] {
        harness.mock_state.update_window(id, |w| {
            w.horizontal_padding = 8;
            w.vertical_padding = 4;
        });
    }
    harness.run(vec![print(), print()]);
    assert_ne!(
        origin(&mut harness, 1),
        Origin::new(300, 200),
        "precondition: tiled"
    );

    stop(&mut harness);
    assert_eq!(origin(&mut harness, 0), Origin::new(100, 120));
    assert_eq!(origin(&mut harness, 1), Origin::new(300, 200));
}

fn set_floating(world: &mut World, id: WinID, floating: bool) {
    let entity = find_window_entity(id, world);
    if floating {
        world.entity_mut(entity).insert(Unmanaged::Floating);
    } else {
        world.entity_mut(entity).remove::<Unmanaged>();
    }
}

#[test]
fn owned_stop_leaves_floating_and_minimized_windows_untouched() {
    let mut harness = harness(true, &[(0, 100, 120), (1, 300, 200), (2, 500, 220)])
        .on_iteration(0, |world, _state| set_floating(world, 1, true))
        .on_iteration(1, |_world, state| {
            state.os_move_window(1, Origin::new(50, 300));
            state.os_minimize_window(2, true);
        });
    harness.run(vec![print(), print(), print()]);
    let entity = find_window_entity(2, harness.world());
    assert!(
        matches!(
            harness.world().get::<Unmanaged>(entity),
            Some(Unmanaged::Minimized)
        ),
        "precondition: minimized"
    );
    let minimized = origin(&mut harness, 2);

    stop(&mut harness);
    assert_eq!(origin(&mut harness, 1), Origin::new(50, 300));
    assert_eq!(origin(&mut harness, 2), minimized);
    assert_eq!(origin(&mut harness, 0), Origin::new(100, 120));
}

#[test]
fn owned_stop_uses_the_position_a_floating_window_had_when_retiled() {
    let mut harness = harness(true, &[(0, 100, 120), (1, 300, 200)])
        .on_iteration(0, |world, _state| set_floating(world, 1, true))
        .on_iteration(1, |_world, state| {
            state.os_move_window(1, Origin::new(560, 260));
        })
        .on_iteration(2, |world, _state| set_floating(world, 1, false));
    harness.run(vec![print(), print(), print(), print()]);
    let entity = find_window_entity(1, harness.world());
    assert!(harness.world().get::<Unmanaged>(entity).is_none());
    assert_ne!(
        origin(&mut harness, 1),
        Origin::new(560, 260),
        "precondition: retiled"
    );

    stop(&mut harness);
    assert_eq!(origin(&mut harness, 1), Origin::new(560, 260));
}

#[test]
fn owned_stop_centers_a_transferred_window_on_its_new_display() {
    let mut harness = harness(true, &[(0, 100, 120)]).with_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        print(),
        window_op(Operation::ToNextDisplay(MoveFocus::Follow)),
        print(),
    ]);

    stop(&mut harness);
    let world = harness.world();
    let mut query = world.query::<&Window>();
    let frame = query.iter(world).find(|w| w.id() == 0).unwrap().frame();
    assert_eq!(frame.min.x, (EXT_DISPLAY_WIDTH - frame.width()) / 2);
    let ext_usable = IRect::new(
        0,
        -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT,
        EXT_DISPLAY_WIDTH,
        0,
    );
    assert!(ext_usable.contains(frame.min) && ext_usable.contains(frame.max));
}

#[test]
fn owned_stop_after_owning_display_disconnects_lands_on_an_available_display() {
    let mut harness = harness(true, &[(0, 100, 120)]).with_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );
    harness = harness.on_iteration(3, |_world, state| state.remove_display(EXT_DISPLAY_ID));
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        print(),
        window_op(Operation::ToNextDisplay(MoveFocus::Follow)),
        print(),
        Event::DisplayRemoved {
            display_id: EXT_DISPLAY_ID,
        },
        print(),
    ]);

    stop(&mut harness);
    let main_usable = IRect::new(
        0,
        TEST_MENUBAR_HEIGHT,
        TEST_DISPLAY_WIDTH,
        TEST_DISPLAY_HEIGHT,
    );
    let title = origin(&mut harness, 0);
    assert!(main_usable.contains(title) && main_usable.contains(title + Origin::new(SIZE.0, 40)));
}

#[test]
fn unowned_stop_keeps_upstream_centering_of_every_window() {
    let mut harness = harness(false, &[(0, 100, 120), (1, 300, 200)])
        .on_iteration(0, |world, _state| set_floating(world, 1, true))
        .on_iteration(1, |_world, state| {
            state.os_move_window(1, Origin::new(50, 300));
        });
    harness.run(vec![print(), print(), print()]);

    stop(&mut harness);
    let world = harness.world();
    let mut query = world.query::<&Window>();
    let frame = query.iter(world).find(|w| w.id() == 1).unwrap().frame();
    let usable_height = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;
    assert_eq!(
        frame.min,
        Origin::new(
            (TEST_DISPLAY_WIDTH - frame.width()) / 2,
            TEST_MENUBAR_HEIGHT + (usable_height - frame.height()) / 2
        )
    );
}
