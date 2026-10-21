//! Emaki shared-edge handoffs (`crate::ecs::edge`) in the mock harness: two
//! displays side by side, a strip scrolled across the shared edge.
use bevy::prelude::*;

use super::*;
use crate::commands::Command;
use crate::ecs::Position;
use crate::ecs::edge::{EdgeParked, EdgePlugin, test_change, test_phase};
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;
use crate::manager::Window;
use crate::types::commands::EdgeCommand;

const FRAME_MS: u64 = 20;
/// The neighbour overlaps the lower part of the test display, so the
/// predicted ownership change sits well above the switch points.
fn neighbour() -> IRect {
    IRect::new(TEST_DISPLAY_WIDTH, 400, TEST_DISPLAY_WIDTH + 1920, 1600)
}
fn owner() -> IRect {
    IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT)
}

fn harness() -> TestHarness {
    let mut harness = TestHarness::new()
        .with_display(EXT_DISPLAY_ID, neighbour(), vec![EXT_WORKSPACE_ID])
        .with_windows(2);
    harness.app.add_plugins(EdgePlugin);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);
    harness
}

struct Probe {
    entity: Entity,
    strip: Entity,
}

impl Probe {
    fn new(harness: &mut TestHarness, id: i32) -> Self {
        let entity = find_window_entity(id, harness.world());
        let world = harness.world();
        let strip = world
            .query::<(Entity, &LayoutStrip)>()
            .iter(world)
            .find(|(_, strip)| strip.contains(entity))
            .map(|(e, _)| e)
            .expect("strip");
        Self { entity, strip }
    }
    fn shift(&self, harness: &mut TestHarness, dx: i32) {
        harness.world().get_mut::<Position>(self.strip).unwrap().0.x += dx;
        harness.advance(std::time::Duration::from_millis(FRAME_MS));
    }
    fn phase(&self, harness: &mut TestHarness) -> String {
        test_phase(harness.world(), self.entity).unwrap_or_else(|| "None".into())
    }
    fn parked(&self, harness: &mut TestHarness) -> bool {
        harness.world().get::<EdgeParked>(self.entity).is_some()
    }
    /// What macOS was last told (padding is zero in the default config).
    fn committed(&self, harness: &mut TestHarness) -> IRect {
        harness.world().get::<Window>(self.entity).unwrap().frame()
    }
    fn slot(&self, harness: &mut TestHarness) -> IRect {
        let world = harness.world();
        let position = world.get::<Position>(self.entity).unwrap().0;
        let size = world.get::<crate::ecs::Bounds>(self.entity).unwrap().0;
        IRect::from_corners(position, position + size)
    }
}

fn overflow(frame: IRect) -> f64 {
    f64::from((frame.max.x - TEST_DISPLAY_WIDTH).max(0)) / f64::from(frame.width())
}

/// Scrolls outward until parked; checks the committed frame never reaches the
/// ownership change while real. Returns the overflow it parked at.
fn scroll_out(harness: &mut TestHarness, probe: &Probe, step: i32, reply: bool) -> f64 {
    for _ in 0..200 {
        let before = probe.slot(harness);
        probe.shift(harness, step);
        let committed = probe.committed(harness);
        let change = test_change(committed, owner(), neighbour());
        if probe.parked(harness) {
            // Emaki (#44): a 5 pt sliver at the outer (left) edge, same height.
            assert_eq!(committed.max.x, 5, "{committed:?}");
            assert_eq!(committed.min.y, before.min.y, "{committed:?}");
            return overflow(before);
        }
        assert!(
            overflow(committed) < change - 0.005,
            "real window at {} past change {change}",
            overflow(committed)
        );
        if reply && probe.phase(harness) == "Approaching" {
            let id = harness.world().get::<Window>(probe.entity).unwrap().id();
            harness.world().write_message(Event::Command {
                command: Command::Edge(EdgeCommand::Covered(id)),
            });
        }
    }
    panic!("never parked");
}

#[test]
fn parks_at_the_cap_without_a_reply_and_returns_after_the_return_cap() {
    let mut harness = harness();
    let probe = Probe::new(&mut harness, 1);
    let parked_at = scroll_out(&mut harness, &probe, 8, false);
    assert!(
        parked_at > 0.5 && parked_at <= 0.63,
        "parked at {parked_at}"
    );

    // Parked: the slot keeps moving with the strip, the committed frame stays.
    let corner = probe.committed(&mut harness);
    probe.shift(&mut harness, 40);
    assert_eq!(probe.committed(&mut harness), corner);

    // Back in past the exit point: Returning, then real after 400 ms unanswered.
    let mut returned = false;
    for _ in 0..200 {
        probe.shift(&mut harness, -8);
        if !probe.parked(&mut harness) {
            returned = true;
            break;
        }
    }
    assert!(returned, "never returned");
    let slot = probe.slot(&mut harness);
    assert!(overflow(slot) <= 0.45, "returned at {}", overflow(slot));
    assert_eq!(probe.committed(&mut harness).min, slot.min);
}

#[test]
fn covered_parks_before_the_cap() {
    let mut harness = harness();
    let probe = Probe::new(&mut harness, 1);
    let parked_at = scroll_out(&mut harness, &probe, 4, true);
    assert!(parked_at < 0.6, "parked at {parked_at}, after the cover");
}

#[test]
fn a_flick_never_crosses() {
    let mut harness = harness();
    let probe = Probe::new(&mut harness, 1);
    scroll_out(&mut harness, &probe, 60, false);
}

#[test]
fn ready_restores_at_rest_short_of_the_limit_and_snaps_past_it() {
    let mut harness = harness();
    let probe = Probe::new(&mut harness, 1);
    scroll_out(&mut harness, &probe, 8, false);
    // Rest with the column parked well past its rest limit: the strip settles
    // it back to the limit (nearer than fully into the edge), then it returns.
    let width = probe.slot(&mut harness).width();
    let target = 0.72;
    let dx = (target - overflow(probe.slot(&mut harness))) * f64::from(width);
    #[allow(clippy::cast_possible_truncation)]
    probe.shift(&mut harness, dx.round() as i32);
    harness.advance(std::time::Duration::from_millis(900));
    let slot = probe.slot(&mut harness);
    let change = test_change(slot, owner(), neighbour());
    assert!(
        (overflow(slot) - (change - 0.04)).abs() < 0.01,
        "snapped to {} (limit {})",
        overflow(slot),
        change - 0.04
    );
    assert!(!probe.parked(&mut harness), "{}", probe.phase(&mut harness));
    assert_eq!(probe.committed(&mut harness).min, slot.min);
    // It stays real at rest: no park/return loop.
    harness.advance(std::time::Duration::from_millis(1500));
    assert_eq!(probe.phase(&mut harness), "Real");
    // Moving on outward, it parks within 0.03 of where it rested.
    let rested = overflow(probe.slot(&mut harness));
    let parked_at = scroll_out(&mut harness, &probe, 4, false);
    assert!(
        parked_at - rested <= 0.035,
        "rested {rested}, parked {parked_at}"
    );
}

#[test]
fn snap_off_leaves_a_column_resting_past_its_limit() {
    let mut harness = harness();
    let config =
        crate::config::Config::try_from("[options]\nsnap_at_display_edge = false\n[bindings]\n")
            .expect("config");
    harness.world().insert_resource(config);
    let probe = Probe::new(&mut harness, 1);
    scroll_out(&mut harness, &probe, 8, false);
    let width = probe.slot(&mut harness).width();
    let dx = (0.72 - overflow(probe.slot(&mut harness))) * f64::from(width);
    #[allow(clippy::cast_possible_truncation)]
    probe.shift(&mut harness, dx.round() as i32);
    harness.advance(std::time::Duration::from_millis(900));
    assert!((overflow(probe.slot(&mut harness)) - 0.72).abs() < 0.01);
    assert!(probe.parked(&mut harness));
}

fn mouse(harness: &mut TestHarness, down: bool) {
    let point = objc2_core_foundation::CGPoint::new(5000.0, 5000.0);
    let modifiers = crate::platform::Modifiers::empty();
    harness.world().write_message(if down {
        Event::MouseDown { point, modifiers }
    } else {
        Event::MouseUp { point, modifiers }
    });
}

#[test]
fn a_drag_takes_a_window_across_and_it_is_left_there() {
    let mut harness = harness();
    let probe = Probe::new(&mut harness, 1);
    mouse(&mut harness, true);
    // While the button is held the window goes well past the change, real.
    for _ in 0..60 {
        probe.shift(&mut harness, 10);
    }
    let slot = probe.slot(&mut harness);
    assert!(overflow(slot) > 0.9, "{}", overflow(slot));
    assert!(!probe.parked(&mut harness));
    mouse(&mut harness, false);
    harness.advance(std::time::Duration::from_millis(1500));
    assert!(!probe.parked(&mut harness), "{}", probe.phase(&mut harness));
    // Back inside, it is the strip's again and hands off normally.
    let back = -(probe.slot(&mut harness).max.x - TEST_DISPLAY_WIDTH + 50);
    probe.shift(&mut harness, back);
    harness.advance(std::time::Duration::from_millis(300));
    scroll_out(&mut harness, &probe, 8, false);
}

#[test]
fn mission_control_never_leaves_a_window_across() {
    // Emaki (#37): Mission Control is the user's, so nothing parks while it is
    // open. Once it closes, a window left past the shared edge parks; the
    // click that closed it is no drag. Seen live on 2026-10-22.
    let mut harness = harness();
    let probe = Probe::new(&mut harness, 1);
    harness
        .world()
        .insert_resource(crate::ecs::MissionControlActive(true));
    mouse(&mut harness, true);
    let past = TEST_DISPLAY_WIDTH - probe.slot(&mut harness).min.x + 20;
    probe.shift(&mut harness, past);
    harness.advance(std::time::Duration::from_millis(100));
    assert!(!probe.parked(&mut harness), "{}", probe.phase(&mut harness));
    mouse(&mut harness, false);
    harness.advance(std::time::Duration::from_millis(40));
    harness
        .world()
        .insert_resource(crate::ecs::MissionControlActive(false));
    // Once the layout has settled.
    harness.advance(std::time::Duration::from_millis(300));
    assert!(probe.parked(&mut harness), "{}", probe.phase(&mut harness));
    assert_eq!(probe.committed(&mut harness).max.x, 5);
}

fn set_mission_control(harness: &mut TestHarness, open: bool) {
    harness
        .world()
        .insert_resource(crate::ecs::MissionControlActive(open));
    harness.advance(std::time::Duration::from_millis(60));
}

fn strip_id(harness: &mut TestHarness, entity: Entity) -> Option<u64> {
    let world = harness.world();
    world
        .query::<&LayoutStrip>()
        .iter(world)
        .find(|strip| strip.contains(entity))
        .map(LayoutStrip::id)
}

#[test]
fn mission_control_shows_a_parked_window_inside_its_outer_edge() {
    // Emaki (#37): Mission Control moves a window to the other display at the
    // same place relative to the display. A sliver parked at the outer edge
    // lands on the destination's shared side, mostly across, and macOS hands
    // it back. While the overview is open it sits flush inside instead.
    let mut harness = harness();
    let probe = Probe::new(&mut harness, 1);
    scroll_out(&mut harness, &probe, 8, false);
    let sliver = probe.committed(&mut harness);
    set_mission_control(&mut harness, true);
    let shown = probe.committed(&mut harness);
    assert_eq!(shown.min.x, 0, "{shown:?}");
    assert_eq!(shown.size(), sliver.size());
    set_mission_control(&mut harness, false);
    assert!(probe.parked(&mut harness), "{}", probe.phase(&mut harness));
    assert_eq!(probe.committed(&mut harness), sliver, "straight back");
}

#[test]
fn a_parked_window_mission_control_moves_joins_its_new_display() {
    let mut harness = harness();
    let probe = Probe::new(&mut harness, 1);
    scroll_out(&mut harness, &probe, 8, false);
    set_mission_control(&mut harness, true);
    let n = neighbour();
    harness.mock_state.update_window(1, |w| {
        w.workspace_id = EXT_WORKSPACE_ID;
        w.frame = IRect::new(n.min.x, n.min.y, n.min.x + 700, n.min.y + 600);
    });
    set_mission_control(&mut harness, false);
    // No detour: from the close on, the window never leaves its new display.
    for _ in 0..30 {
        let committed = probe.committed(&mut harness);
        assert!(n.contains(committed.center()), "{committed:?}");
        harness.advance(std::time::Duration::from_millis(FRAME_MS));
    }
    assert_eq!(strip_id(&mut harness, probe.entity), Some(EXT_WORKSPACE_ID));
    assert!(!probe.parked(&mut harness), "{}", probe.phase(&mut harness));
    let committed = probe.committed(&mut harness);
    assert!(n.contains(committed.center()), "{committed:?}");
}

#[test]
fn a_stale_position_as_mission_control_closes_does_not_park() {
    // Emaki (#37): for a frame or two after the overview closes, positions are
    // still the overview's until the strips are laid out again. Parking on them
    // flicks a window out and straight back. Seen live on 2026-10-22.
    let mut harness = harness();
    let probe = Probe::new(&mut harness, 1);
    set_mission_control(&mut harness, true);
    let past = TEST_DISPLAY_WIDTH - probe.slot(&mut harness).min.x + 20;
    probe.shift(&mut harness, past);
    harness
        .world()
        .insert_resource(crate::ecs::MissionControlActive(false));
    harness.advance(std::time::Duration::from_millis(FRAME_MS));
    assert!(!probe.parked(&mut harness), "{}", probe.phase(&mut harness));
    probe.shift(&mut harness, -past);
    harness.advance(std::time::Duration::from_millis(300));
    assert!(!probe.parked(&mut harness), "{}", probe.phase(&mut harness));
}
