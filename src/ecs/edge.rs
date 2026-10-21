//! Emaki: shared-edge handoffs (ADR 0002, "the engine owns handoffs; Emaki
//! owns the glass").
//!
//! With two displays side by side, macOS hands a window to the neighbour once
//! the neighbour holds the larger part of it. The engine stays the only writer
//! of tiled window positions and never lets a tiled window get there: a column
//! moving past its switch point is announced (`Approaching`), Emaki covers it
//! with a stand-in and replies `covered`, and the window parks in its display's
//! bottom outer corner. It parks anyway at the cap, however late the reply.
//! Moving back, or coming to rest short of its limit, it is announced
//! (`Returning`), Emaki blurs in beneath the stand-in and replies `ready`, and
//! the window returns. `Position` keeps the strip slot throughout; only the
//! committed AX origin differs while parked.

use std::collections::HashMap;
use std::time::Duration;

use bevy::app::{App, Plugin, PostUpdate, Update};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::{not, resource_exists};
use bevy::ecs::system::{Commands, Query, Res, ResMut};
use bevy::math::IRect;
use bevy::time::Time;
use tracing::debug;

use crate::commands::Command;
use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::workspace::SnapStripMarker;
use crate::ecs::{
    Bounds, Initializing, Position, RepositionMarker, ResizeMarker, Scrolling, SendMessageTrigger,
    Unmanaged,
};
use crate::events::Event;
use crate::manager::{Display, Origin, Window};
use crate::types::commands::EdgeCommand;
use crate::types::state::{EdgeColumn, EdgeHandoff, EdgeSide, EdgeState, Frame};
use crate::types::windowset::LayoutOp;

/// Still this long, with no input, counts as rest (E9eb's edge idle time).
const REST: Duration = Duration::from_millis(200);
/// Rest snap: ease-out settle to the rest limit or fully into the edge.
const REST_SNAP: Duration = Duration::from_millis(260);
/// A column parks by `enter + PARK_CAP` however far its cover got.
const PARK_CAP: f64 = 0.07;
/// Resuming outward from a real rest, it parks within this of where it rested.
const ANCHOR_CAP: f64 = 0.03;
/// Margins below the predicted ownership change: the most a column may rest at,
/// the cap, and the hard never-cross limit.
const REST_MARGIN: f64 = 0.04;
const CAP_MARGIN: f64 = 0.02;
const CROSS_MARGIN: f64 = 0.01;
/// A `Returning` window comes back anyway after this, so a lost reply never
/// leaves a column under glass.
const RETURN_CAP: Duration = Duration::from_millis(400);
/// User resizes this recent suspend handoffs.
const LIVE_RESIZE: Duration = Duration::from_millis(300);
/// After a drag, resize or Mission Control ends, handoffs wait this long.
const HANDS_OFF_GRACE: Duration = Duration::from_millis(500);
/// Emaki (#44, #46): a parked window leaves this much at its owning display's
/// outer edge.
const SLIVER: i32 = 5;

/// Emaki (#37): throwaway owner knobs, read when needed.
pub(crate) fn knob(name: &str, default: &str) -> String {
    std::fs::read_to_string(format!("/Users/Shared/EmakiEdge/{name}"))
        .map(|s| s.trim().to_owned())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_owned())
}
/// `early`: a Mission Control selection places the strip and window while the
/// overview closes. `after`: the strip eases once it has closed.
pub(crate) fn mc_early() -> bool {
    knob("mc-select", "early") != "after"
}

/// Emaki (#37): while set, strip and window moves land in one tick: during a
/// live resize (neighbours track the edge) and, in `early` mode, while Mission
/// Control is open.
pub(crate) static MOTION_DIRECT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static BUTTON_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static RESIZED_SINCE_PRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static MC_EARLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn update_motion_direct() {
    use std::sync::atomic::Ordering::Relaxed;
    let direct = BUTTON_DOWN.load(Relaxed) && RESIZED_SINCE_PRESS.load(Relaxed)
        || MC_EARLY.load(Relaxed);
    if MOTION_DIRECT.swap(direct, Relaxed) != direct {
        debug!(target: "emaki::motion", "direct motion {direct}");
    }
}

pub struct EdgePlugin;

impl Plugin for EdgePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EdgeOutbox>()
            .init_resource::<EdgeWake>()
            .init_resource::<EdgeTracker>()
            .add_systems(Update, mark_live_resizes)
            .add_systems(
                PostUpdate,
                edge_handoffs
                    .after(super::systems::animate_entities)
                    .before(super::systems::commit_window_position)
                    .run_if(not(resource_exists::<Initializing>)),
            );
    }
}

/// The window's AX origin is the parking corner, not its `Position`.
#[derive(Component)]
pub(crate) struct EdgeParked;

/// The display's maximum refresh rate, read with its other screen properties.
#[derive(Component)]
pub(crate) struct DisplayRefresh(pub u32);

#[derive(Component)]
pub(crate) struct LiveResize(Duration);

/// Edge events waiting for subscribers.
#[derive(Default, Resource)]
pub(crate) struct EdgeOutbox(pub Vec<EdgeState>);

/// Keeps the event pump ticking while a handoff or rest timer is pending.
#[derive(Default, Resource)]
pub(crate) struct EdgeWake(pub bool);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Real,
    Approaching,
    Parked,
    Returning,
}

#[derive(Debug)]
struct Handoff {
    phase: Phase,
    since: Duration,
    /// Came to rest real between `enter` and the rest limit: its switch point
    /// moves out to here.
    rest_anchor: Option<f64>,
    prev_overflow: Option<f64>,
}

struct Snap {
    strip: Entity,
    from: i32,
    to: i32,
    start: Duration,
    guard: Entity,
}

#[derive(Default)]
struct DisplayMotion {
    strip_x: HashMap<Entity, i32>,
    /// Last motion that was not our own rest snap.
    last_input: Option<Duration>,
    snap: Option<Snap>,
    last_sent: Option<EdgeState>,
}

#[derive(Default, Resource)]
struct EdgeTracker {
    /// Mouse button held (a drag or resize may be under way), or Mission
    /// Control open: windows move by the user's hand, never by handoffs.
    button_down: bool,
    hands_off_until: Duration,
    /// Emaki (#37): a live resize left the strip where the user put it; no
    /// rest snap moves it until the next scroll. Knob `resize-snap` = `snap`
    /// restores the snap.
    hold_after_resize: bool,
    /// Emaki (#37): Mission Control was open last frame.
    mc_was_open: bool,
    /// Real windows the user put past the edge: left alone, not parked back.
    foreign: std::collections::HashSet<Entity>,
    windows: HashMap<Entity, Handoff>,
    displays: HashMap<Entity, DisplayMotion>,
}

/// Seconds on the `CACurrentMediaTime` clock.
fn media_time() -> f64 {
    unsafe extern "C" {
        fn clock_gettime_nsec_np(clock: libc::clockid_t) -> u64;
    }
    #[allow(clippy::cast_precision_loss)]
    let nanos = unsafe { clock_gettime_nsec_np(libc::CLOCK_UPTIME_RAW) } as f64;
    nanos / 1e9
}

/// The side of `owner` it shares with another display, and that display.
pub(crate) fn shared_edge(
    owner: IRect,
    displays: impl Iterator<Item = IRect>,
) -> Option<(EdgeSide, IRect)> {
    displays.into_iter().find_map(|other| {
        if other == owner || owner.min.y >= other.max.y || other.min.y >= owner.max.y {
            None
        } else if owner.max.x == other.min.x {
            Some((EdgeSide::Right, other))
        } else if owner.min.x == other.max.x {
            Some((EdgeSide::Left, other))
        } else {
            None
        }
    })
}

/// Fraction of `real`'s width past the shared edge.
fn overflow(real: IRect, owner: IRect, side: EdgeSide) -> f64 {
    let past = match side {
        EdgeSide::Right => real.max.x - owner.max.x,
        EdgeSide::Left => owner.min.x - real.min.x,
    };
    f64::from(past.max(0)) / f64::from(real.width().max(1))
}

/// Overflow at which macOS hands the window to the neighbour: where the
/// neighbour's share of its area becomes the larger (REST.md: h / (h + overlap)).
fn ownership_change(real: IRect, owner: IRect, neighbour: IRect) -> f64 {
    let overlap =
        |display: IRect| (real.max.y.min(display.max.y) - real.min.y.max(display.min.y)).max(0);
    let (mine, theirs) = (f64::from(overlap(owner)), f64::from(overlap(neighbour)));
    if theirs <= 0.0 {
        1.0
    } else {
        mine / (mine + theirs)
    }
}

/// Switch points for the owning display's refresh (#19, #21): the stand-in is
/// drawn a frame behind, so faster displays switch earlier.
fn thresholds(refresh_hz: u32) -> (f64, f64) {
    if refresh_hz >= 100 {
        (0.43, 0.33)
    } else {
        (0.55, 0.45)
    }
}

/// Padded origin beyond the owning display's outer edge (opposite `side`, the
/// shared edge), leaving a plain `SLIVER` at the same height (#44, #48).
fn park_origin(slot: IRect, owner: IRect, h_pad: i32, _v_pad: i32, side: EdgeSide) -> Origin {
    let real_width = slot.width() - 2 * h_pad;
    let x = match side {
        EdgeSide::Right => owner.min.x - real_width + SLIVER,
        EdgeSide::Left => owner.max.x - SLIVER,
    };
    // `reposition` adds the padding back.
    Origin::new(x - h_pad, slot.min.y)
}

#[derive(Clone, Copy)]
struct Limits {
    enter_at: f64,
    cap_at: f64,
    exit_at: f64,
    rest_limit: f64,
    cross: f64,
}

fn limits(change: f64, enter: f64, exit: f64, anchor: Option<f64>) -> Limits {
    let cross = (change - CROSS_MARGIN).min(0.998);
    let rest_limit = (change - REST_MARGIN).min(0.998);
    let cap = (change - CAP_MARGIN).min(cross);
    // An anchor is at most the rest limit, so already below the cap.
    let enter_at = anchor.unwrap_or_else(|| enter.min(cap - 0.03)).max(0.0);
    let cap_at = anchor.map_or(enter + PARK_CAP, |a| a + ANCHOR_CAP).min(cap);
    Limits {
        enter_at,
        cap_at,
        exit_at: exit.min(enter_at - 0.1).max(0.0),
        rest_limit,
        cross,
    }
}

/// Records user resizes, which suspend handoffs (REST.md).
fn mark_live_resizes(
    mut messages: MessageReader<Event>,
    time: Res<Time>,
    windows: Query<(Entity, &Window, Has<ResizeMarker>)>,
    mission_control: Option<Res<crate::ecs::MissionControlActive>>,
    mut commands: Commands,
) {
    use std::sync::atomic::Ordering::Relaxed;
    for event in messages.read() {
        match event {
            Event::MouseDown { .. } => {
                BUTTON_DOWN.store(true, Relaxed);
                RESIZED_SINCE_PRESS.store(false, Relaxed);
            }
            Event::MouseUp { .. } => BUTTON_DOWN.store(false, Relaxed),
            _ => {}
        }
        let Event::WindowResized { window_id } = event else {
            continue;
        };
        if let Some((entity, _, false)) = windows.iter().find(|(_, w, _)| w.id() == *window_id)
            && let Ok(mut cmd) = commands.get_entity(entity)
        {
            cmd.try_insert(LiveResize(time.elapsed()));
            if BUTTON_DOWN.load(Relaxed) {
                RESIZED_SINCE_PRESS.store(true, Relaxed);
            }
        }
    }
    let mc = mission_control.is_some_and(|mc| mc.0);
    let early = mc && (MC_EARLY.load(Relaxed) || mc_early());
    MC_EARLY.store(early, Relaxed);
    update_motion_direct();
}

type EdgeWindows<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut Window,
        &'static Position,
        &'static Bounds,
        Option<&'static Unmanaged>,
        Has<RepositionMarker>,
        Option<&'static LiveResize>,
        Has<EdgeParked>,
    ),
    Without<LayoutStrip>,
>;

type EdgeStrips<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static LayoutStrip,
        &'static mut Position,
        Has<Scrolling>,
        &'static ChildOf,
    ),
    (With<LayoutStrip>, Without<Window>),
>;

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn edge_handoffs(
    mut messages: MessageReader<Event>,
    mut windows: EdgeWindows,
    mut strips: EdgeStrips,
    displays: Query<(Entity, &Display, Option<&DisplayRefresh>)>,
    config: Res<Config>,
    mission_control: Option<Res<crate::ecs::MissionControlActive>>,
    drop: Option<Res<crate::ecs::drop::DropPreview>>,
    time: Res<Time>,
    mut tracker: ResMut<EdgeTracker>,
    mut outbox: ResMut<EdgeOutbox>,
    mut wake: ResMut<EdgeWake>,
    mut commands: Commands,
) {
    let now = time.elapsed();
    let mut replies: HashMap<i32, EdgeCommand> = HashMap::new();
    for event in messages.read() {
        match event {
            Event::MouseDown { .. } => tracker.button_down = true,
            Event::MouseUp { .. } => {
                tracker.button_down = false;
                // Emaki (#37): after a live resize the strip settles under
                // handoffs, so nothing spills while it eases.
                if RESIZED_SINCE_PRESS.load(std::sync::atomic::Ordering::Relaxed) {
                    tracker.hold_after_resize = knob("resize-snap", "hold") != "snap";
                } else {
                    tracker.hands_off_until = now + HANDS_OFF_GRACE;
                }
            }
            _ => {}
        }
        if let Event::Command {
            command: Command::Edge(reply),
        } = event
        {
            match *reply {
                EdgeCommand::Focus(id) => commands.trigger(SendMessageTrigger(Event::Command {
                    command: Command::Layout(vec![LayoutOp::Focus(id)]),
                })),
                EdgeCommand::Covered(id) | EdgeCommand::Ready(id) => {
                    replies.insert(id, *reply);
                }
            }
        }
    }

    let arrangement = displays
        .iter()
        .map(|(_, display, _)| display.bounds())
        .collect::<Vec<_>>();
    // Emaki (#37): Mission Control is the user's. In `early` mode a selection
    // places windows at once, with no glass, while the overview closes.
    let mc_open = mission_control.is_some_and(|mc| mc.0);
    let mc_place = mc_open && MC_EARLY.load(std::sync::atomic::Ordering::Relaxed);
    // The click that closed the overview dragged no real window: no grace.
    if tracker.mc_was_open && !mc_open {
        tracker.hands_off_until = now;
    }
    tracker.mc_was_open = mc_open;
    // Emaki (#33): during a drop preview only the dragged window is the user's;
    // neighbours sliding aside still hand off, and no rest snap moves the strip.
    let dragged = drop.and_then(|d| d.dragged());
    // Emaki (#37): during a live resize only the resized window is the user's.
    let live_resize = tracker.button_down
        && RESIZED_SINCE_PRESS.load(std::sync::atomic::Ordering::Relaxed);
    let hands_off_all = mc_open
        || dragged.is_none()
            && !live_resize
            && (tracker.button_down || now < tracker.hands_off_until);
    if live_resize {
        tracker.hold_after_resize = true;
    }
    if strips.iter().any(|(_, _, _, scrolling, _)| scrolling) {
        tracker.hold_after_resize = false;
    }
    let snap_on = config.snap_at_display_edge() && !tracker.hold_after_resize;
    let mut busy = false;
    let mut seen = Vec::new();
    let tracker = &mut *tracker;

    for (display_entity, display, refresh) in &displays {
        let owner = display.bounds();
        let edge = shared_edge(owner, arrangement.iter().copied());
        let refresh_hz = refresh.map_or(60, |r| r.0);
        let (enter, exit) = thresholds(refresh_hz);
        let motion = tracker.displays.entry(display_entity).or_default();

        // Motion: any strip of this display moved, swiping, or animating.
        let mut moved = false;
        let mut input = false;
        let mut display_strips = Vec::new();
        for (strip_entity, strip, position, scrolling, child_of) in &strips {
            if child_of.parent() != display_entity {
                continue;
            }
            let previous = motion.strip_x.insert(strip_entity, position.0.x);
            let snapping = motion
                .snap
                .as_ref()
                .is_some_and(|s| s.strip == strip_entity);
            let shifted = previous.is_some_and(|x| x != position.0.x);
            let animating = strip
                .all_windows()
                .into_iter()
                .any(|entity| windows.get(entity).is_ok_and(|w| w.4));
            moved |= shifted || scrolling || animating;
            input |= scrolling || animating || (shifted && !snapping);
            display_strips.push((strip_entity, strip.all_windows(), position.0.x, scrolling));
        }
        if input {
            motion.last_input = Some(now);
        }
        // New input cancels a rest snap.
        if input && let Some(snap) = motion.snap.take() {
            commands.entity(snap.guard).despawn();
            debug!("edge: rest snap aborted by input");
        }
        let resting = motion
            .last_input
            .is_none_or(|t| now.saturating_sub(t) >= REST);
        if motion
            .last_input
            .is_some_and(|t| now.saturating_sub(t) < REST + REST)
        {
            busy = true;
        }

        let Some((side, neighbour)) = edge else {
            // No shared edge (any more): bring parked windows back.
            for (_, entities, _, _) in &display_strips {
                for entity in entities {
                    if let Some(handoff) = tracker.windows.remove(entity)
                        && handoff.phase != Phase::Real
                        && let Ok((mut window, position, ..)) = windows.get_mut(*entity)
                    {
                        window.reposition(position.0);
                        if let Ok(mut cmd) = commands.get_entity(*entity) {
                            cmd.try_remove::<EdgeParked>();
                        }
                    }
                }
            }
            continue;
        };
        let edge_x = match side {
            EdgeSide::Right => owner.max.x,
            EdgeSide::Left => owner.min.x,
        };

        // Rest snap in flight: write the strip directly, member windows follow
        // in lockstep under the snap guard.
        if let Some(snap) = &motion.snap {
            let u =
                (now.saturating_sub(snap.start).as_secs_f64() / REST_SNAP.as_secs_f64()).min(1.0);
            let eased = 1.0 - (1.0 - u).powi(3);
            #[allow(clippy::cast_possible_truncation)]
            let x = snap.from + (f64::from(snap.to - snap.from) * eased).round() as i32;
            if let Ok((_, _, mut position, _, _)) = strips.get_mut(snap.strip)
                && position.0.x != x
            {
                position.0.x = x;
                motion.strip_x.insert(snap.strip, x);
            }
            moved = true;
            busy = true;
            if u >= 1.0 {
                let snap = motion.snap.take().expect("checked above");
                commands.entity(snap.guard).despawn();
                debug!("edge: rest snap done");
            }
        }

        let mut columns = Vec::new();
        let mut visible_strip_x = None;
        let mut snap_candidate: Option<(Entity, i32, f64, f64, i32)> = None;
        for (strip_entity, entities, strip_x, scrolling) in &display_strips {
            for &entity in entities {
                let Ok((mut window, position, bounds, unmanaged, _, live, parked)) =
                    windows.get_mut(entity)
                else {
                    continue;
                };
                if unmanaged.is_some() {
                    continue;
                }
                let (h_pad, v_pad) = (window.horizontal_padding(), window.vertical_padding());
                let slot = IRect::from_corners(position.0, position.0 + bounds.0);
                let real = IRect::new(
                    slot.min.x + h_pad,
                    slot.min.y + v_pad,
                    slot.max.x - h_pad,
                    slot.max.y - v_pad,
                );
                // Hidden virtual rows sit parked below the display.
                if real.min.y >= owner.max.y - crate::ecs::layout::PARKED_STRIP_SLIVER
                    || real.width() <= 0
                {
                    continue;
                }
                seen.push(entity);
                visible_strip_x.get_or_insert(*strip_x);
                let spill = overflow(real, owner, side);
                let change = ownership_change(real, owner, neighbour);
                let handoff = tracker.windows.entry(entity).or_insert(Handoff {
                    phase: if parked { Phase::Parked } else { Phase::Real },
                    since: now,
                    rest_anchor: None,
                    prev_overflow: None,
                });
                let vel = handoff.prev_overflow.map_or(0.0, |p| spill - p);
                handoff.prev_overflow = Some(spill);
                let live_now = live.is_some_and(|l| now.saturating_sub(l.0) < LIVE_RESIZE);
                let hands_off =
                    hands_off_all || dragged == Some(entity) || (live_resize && live_now);
                // Emaki (#37): a window Mission Control moved is placed in the
                // right strip once it closes; it is not the user's to leave
                // across.
                let user_moving = hands_off && !mc_open;
                let resizing = hands_off || live_now;
                if live.is_some()
                    && !resizing
                    && let Ok(mut cmd) = commands.get_entity(entity)
                {
                    cmd.try_remove::<LiveResize>();
                }

                // Came to rest real between the switch point and the rest
                // limit: stay real, and switch from here when moving on.
                let l0 = limits(change, enter, exit, None);
                if matches!(handoff.phase, Phase::Real | Phase::Approaching) {
                    if resting && spill > l0.enter_at && spill <= l0.rest_limit {
                        handoff.rest_anchor =
                            Some(handoff.rest_anchor.map_or(spill, |a| a.min(spill)));
                    } else if spill <= l0.enter_at {
                        handoff.rest_anchor = None;
                    }
                }
                let lim = limits(change, enter, exit, handoff.rest_anchor);
                let id = window.id();
                let reply = replies.get(&id).copied();
                let before = handoff.phase;

                let mut park = |window: &mut Window, handoff: &mut Handoff, why: &str| {
                    window.reposition(park_origin(slot, owner, h_pad, v_pad, side));
                    if let Ok(mut cmd) = commands.get_entity(entity) {
                        cmd.try_insert(EdgeParked);
                    }
                    debug!("edge: park {id} at {spill:.3} ({why})");
                    handoff.phase = Phase::Parked;
                    handoff.since = now;
                };

                // Hands off: the user moves windows; nothing parks, returns or
                // is guarded, so a drag can take a window to the other display.
                // A real window left past its switch point afterwards is the
                // user's, until it is back inside.
                if user_moving {
                    if handoff.phase == Phase::Real && spill > lim.enter_at {
                        tracker.foreign.insert(entity);
                    }
                } else if spill <= 0.0 {
                    tracker.foreign.remove(&entity);
                }
                let foreign = tracker.foreign.contains(&entity);
                match handoff.phase {
                    // Emaki (#37, early): place at once, no cover, no reply.
                    Phase::Parked | Phase::Returning if mc_place && spill <= lim.rest_limit => {
                        window.reposition(position.0);
                        if let Ok(mut cmd) = commands.get_entity(entity) {
                            cmd.try_remove::<EdgeParked>();
                        }
                        debug!(target: "emaki::motion", "mc: restore {id} at {spill:.3}");
                        handoff.phase = Phase::Real;
                        handoff.since = now;
                    }
                    Phase::Real | Phase::Approaching
                        if mc_place && !foreign && spill > lim.rest_limit =>
                    {
                        park(&mut window, handoff, "mission control");
                    }
                    _ if hands_off || foreign => {}
                    Phase::Real => {
                        if spill >= lim.cross {
                            park(&mut window, handoff, "never-cross");
                        } else if !resizing && spill > lim.enter_at {
                            if spill >= lim.cap_at || spill + vel >= lim.cap_at {
                                park(&mut window, handoff, "cap");
                            } else {
                                handoff.phase = Phase::Approaching;
                                handoff.since = now;
                            }
                        }
                    }
                    Phase::Approaching => {
                        let capped =
                            spill >= lim.cap_at || (vel > 0.0 && spill + vel >= lim.cap_at);
                        if spill >= lim.cross || (!resizing && capped) {
                            park(&mut window, handoff, "cap");
                        } else if !resizing && matches!(reply, Some(EdgeCommand::Covered(_))) {
                            park(&mut window, handoff, "covered");
                        } else if spill <= lim.enter_at && vel <= 0.0 {
                            handoff.phase = Phase::Real;
                            handoff.since = now;
                        }
                    }
                    Phase::Parked => {
                        let rest_return = resting
                            && motion.snap.is_none()
                            && spill > lim.exit_at
                            && spill <= lim.rest_limit;
                        if !resizing && (spill <= lim.exit_at || rest_return) {
                            handoff.phase = Phase::Returning;
                            handoff.since = now;
                        }
                    }
                    Phase::Returning => {
                        let still_wanted = spill <= lim.exit_at
                            || (resting && motion.snap.is_none() && spill <= lim.rest_limit);
                        if !still_wanted {
                            handoff.phase = Phase::Parked;
                            handoff.since = now;
                        } else if let Some(why) = if matches!(reply, Some(EdgeCommand::Ready(_))) {
                            Some("ready")
                        } else {
                            (now.saturating_sub(handoff.since) >= RETURN_CAP)
                                .then_some("return cap")
                        } {
                            window.reposition(position.0);
                            if let Ok(mut cmd) = commands.get_entity(entity) {
                                cmd.try_remove::<EdgeParked>();
                            }
                            // Returned at rest past `enter`: switch from here.
                            handoff.rest_anchor = (spill > enter).then_some(spill);
                            debug!("edge: restore {id} at {spill:.3} ({why})");
                            handoff.phase = Phase::Real;
                            handoff.since = now;
                        }
                    }
                }
                if handoff.phase != before {
                    debug!("edge: {id} {before:?} -> {:?} at {spill:.3}", handoff.phase);
                }
                if matches!(handoff.phase, Phase::Approaching | Phase::Returning) || resizing {
                    busy = true;
                }

                // A column resting past its limit with part of it showing.
                if snap_on
                    && !hands_off_all
                    && dragged.is_none()
                    && !foreign
                    && resting
                    && !scrolling
                    && motion.snap.is_none()
                    && spill > lim.rest_limit
                    && spill < 0.998
                    && snap_candidate.is_none()
                {
                    let to = if spill - lim.rest_limit <= 1.0 - spill {
                        lim.rest_limit - 0.002
                    } else {
                        1.0
                    };
                    snap_candidate = Some((*strip_entity, *strip_x, spill, to, real.width()));
                }

                if handoff.phase != Phase::Real || spill > 0.0 {
                    columns.push(EdgeColumn {
                        window_id: id,
                        frame: Frame {
                            x: real.min.x,
                            y: real.min.y,
                            width: real.width(),
                            height: real.height(),
                        },
                        handoff: match handoff.phase {
                            Phase::Real => EdgeHandoff::Real,
                            Phase::Approaching => EdgeHandoff::Approaching,
                            Phase::Parked => EdgeHandoff::Parked,
                            Phase::Returning => EdgeHandoff::Returning,
                        },
                        overflow: spill,
                        enter_at: lim.enter_at,
                        cap_at: lim.cap_at,
                        exit_at: lim.exit_at,
                        rest_limit: lim.rest_limit,
                    });
                }
            }
        }

        if let Some((strip, from, spill, to, width)) = snap_candidate {
            #[allow(clippy::cast_possible_truncation)]
            let shift = ((to - spill) * f64::from(width)).round() as i32;
            let shift = match side {
                EdgeSide::Right => shift,
                EdgeSide::Left => -shift,
            };
            let guard = commands.spawn(SnapStripMarker { strip }).id();
            motion.snap = Some(Snap {
                strip,
                from,
                to: from + shift,
                start: now,
                guard,
            });
            busy = true;
            debug!("edge: rest snap {spill:.3} -> {to:.3}");
        }

        let state = EdgeState {
            display_id: display.id(),
            side,
            edge_x,
            top: owner.min.y,
            bottom: owner.max.y,
            refresh_hz,
            strip_x: visible_strip_x.unwrap_or(0),
            moving: moved,
            resting,
            columns,
            time: 0.0,
        };
        // Silent while nothing changes: an idle edge costs no events.
        if motion.last_sent.as_ref() != Some(&state) {
            motion.last_sent = Some(state.clone());
            outbox.0.push(EdgeState {
                time: media_time(),
                ..state
            });
        }
    }

    tracker.windows.retain(|entity, _| seen.contains(entity));
    tracker.foreign.retain(|entity| seen.contains(entity));
    wake.0 = busy;
}

#[cfg(test)]
pub(crate) fn test_phase(world: &bevy::ecs::world::World, entity: Entity) -> Option<String> {
    world
        .get_resource::<EdgeTracker>()?
        .windows
        .get(&entity)
        .map(|handoff| format!("{:?}", handoff.phase))
}

#[cfg(test)]
pub(crate) fn test_change(real: IRect, owner: IRect, neighbour: IRect) -> f64 {
    ownership_change(real, owner, neighbour)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_change_matches_the_measured_arrangement() {
        // REST.md: 900 pt tall at y 200 on the Studio Display, built-in to the
        // right at (2560, 639): 900 / (900 + 461) = 0.661.
        let studio = IRect::new(0, 0, 2560, 1440);
        let builtin = IRect::new(2560, 639, 4072, 1621);
        let real = IRect::new(2000, 200, 2900, 1100);
        let change = ownership_change(real, studio, builtin);
        assert!((change - 0.661).abs() < 0.001, "{change}");
        assert_eq!(
            shared_edge(studio, [studio, builtin].into_iter()),
            Some((EdgeSide::Right, builtin))
        );
        assert_eq!(
            shared_edge(builtin, [studio, builtin].into_iter()),
            Some((EdgeSide::Left, studio))
        );
    }

    #[test]
    fn limits_stay_below_the_change() {
        let l = limits(0.661, 0.55, 0.45, None);
        assert!((l.cap_at - 0.62).abs() < 1e-9);
        assert!((l.rest_limit - 0.621).abs() < 1e-9);
        assert!(l.cross < 0.661 && l.cap_at < l.cross);
        // A short reach: everything moves inside the change.
        let l = limits(0.5, 0.55, 0.45, None);
        assert!(l.enter_at < l.cap_at && l.cap_at < l.cross && l.cross < 0.5);
        assert!(l.exit_at <= l.enter_at);
        // Anchored after a rest: parks within 0.03 of it.
        let l = limits(0.661, 0.55, 0.45, Some(0.6));
        assert!((l.enter_at - 0.6).abs() < 1e-9 && (l.cap_at - 0.63).abs() < 1e-9);
    }

    #[test]
    fn parking_leaves_a_sliver_at_the_outer_edge() {
        let studio = IRect::new(0, 0, 2560, 1440);
        let slot = IRect::new(2000, 200, 2908, 1108);
        // Shared edge on the right: the sliver shows at the left, same height.
        let o = park_origin(slot, studio, 4, 4, EdgeSide::Right);
        assert_eq!((o.x + 4 + 900, o.y + 4), (5, 204));
        let o = park_origin(slot, studio, 4, 4, EdgeSide::Left);
        assert_eq!((o.x + 4, o.y + 4), (2555, 204));
    }
}
