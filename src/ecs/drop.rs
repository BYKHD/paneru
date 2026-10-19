//! Emaki prototype (#33): drop previews for dragging tiled windows.
//!
//! Throwaway. While the user drags a tiled window by its title bar, the window
//! is moved, tentatively, to the column slot under the pointer on whichever
//! display the pointer is over; the strip's layout pass then slides the
//! neighbours aside to leave a gap of its landing width. macOS keeps moving the
//! dragged window itself: the engine does not position it until release. Esc
//! with the button held puts the gap back at the window's original slot.
//! Release commits the tentative slot and animates the window into it.
//!
//! The tentative slot lives in the real `LayoutStrip`, so a crash or Stop mid
//! drag leaves it there; release or cancel are the only commits the owner sees.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bevy::app::{App, Plugin, Update};
use bevy::ecs::change_detection::DetectChangesMut as _;
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Query, Res, ResMut};
use bevy::math::IRect;
use bevy::time::Time;
use objc2_core_foundation::CGPoint;
use tracing::debug;

use crate::config::Config;
use crate::ecs::layout::{Column, LayoutStrip, origin_exposing};
use crate::ecs::{
    Bounds, DockPosition, LayoutPosition, MouseHeldMarker, Position, RepositionMarker,
    SelectedVirtualMarker, SpawnCommandsExt as _, Unmanaged,
};
use crate::events::Event;
use crate::manager::{Display, Size, Window, WindowManager};
use crate::util::round_px;

/// Set while a drop preview is live, so the event tap can claim Esc.
pub(crate) static DRAG_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The window moves at least this far before a press counts as a drag.
const DRAG_START_PX: i32 = 4;
/// After release the window stays shielded from reshuffles while it lands.
const LANDING_MIN: Duration = Duration::from_millis(100);
const LANDING_MAX: Duration = Duration::from_millis(1500);

/// On the dragged window. `hovering`: macOS moves it, the layout must not.
#[derive(Component)]
pub(crate) struct DropDragged {
    pub landing: Size,
    pub hovering: bool,
}

#[derive(Default)]
enum State {
    #[default]
    Idle,
    Pressed {
        entity: Entity,
        frame: IRect,
    },
    Dragging(Session),
    Landing {
        entity: Entity,
        since: Duration,
    },
}

struct Session {
    entity: Entity,
    home: (Entity, usize),
    home_display: Entity,
    home_size: Size,
    /// Width over the home display's usable width (the transfer rule).
    ratio: f64,
    at: (Entity, usize),
    cancelled: bool,
}

#[derive(Default, Resource)]
pub(crate) struct DropPreview {
    state: State,
}

impl DropPreview {
    /// The dragged window while a preview is live: handoffs leave only it alone.
    pub(crate) fn dragged(&self) -> Option<Entity> {
        match &self.state {
            State::Dragging(session) => Some(session.entity),
            State::Landing { entity, .. } => Some(*entity),
            _ => None,
        }
    }
}

pub struct DropPreviewPlugin;

impl Plugin for DropPreviewPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DropPreview>()
            .add_systems(Update, drop_preview);
    }
}

type Strips<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut LayoutStrip,
        &'static Position,
        &'static ChildOf,
        Has<SelectedVirtualMarker>,
    ),
    Without<Window>,
>;

type DragWindows<'w, 's> = Query<
    'w,
    's,
    (
        &'static Window,
        &'static Position,
        &'static mut Bounds,
        &'static mut LayoutPosition,
        Option<&'static Unmanaged>,
        Has<RepositionMarker>,
    ),
    Without<LayoutStrip>,
>;

/// How far `point` is outside `rect`; display bounds leave out the menu bar.
fn distance(rect: IRect, point: &CGPoint) -> f64 {
    let dx = (f64::from(rect.min.x) - point.x)
        .max(point.x - f64::from(rect.max.x))
        .max(0.0);
    let dy = (f64::from(rect.min.y) - point.y)
        .max(point.y - f64::from(rect.max.y))
        .max(0.0);
    dx.hypot(dy)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn drop_preview(
    mut messages: MessageReader<Event>,
    mut strips: Strips,
    mut windows: DragWindows,
    displays: Query<(Entity, &Display, Option<&DockPosition>)>,
    held: Query<Entity, With<MouseHeldMarker>>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    time: Res<Time>,
    mut preview: ResMut<DropPreview>,
    mut commands: Commands,
) {
    let now = time.elapsed();

    // Landing ends once the window reached its slot.
    if let State::Landing { entity, since } = preview.state {
        let moving = windows.get(entity).is_ok_and(|w| w.5);
        let elapsed = now.saturating_sub(since);
        // The layout only starts the move a frame or two after release.
        if (!moving && elapsed > LANDING_MIN) || elapsed > LANDING_MAX {
            if let Ok(mut cmd) = commands.get_entity(entity) {
                cmd.try_remove::<DropDragged>();
            }
            debug!("drop: landed {entity}");
            preview.state = State::Idle;
        }
    }

    let viewport = |display: Entity| {
        displays
            .get(display)
            .ok()
            .map(|(_, d, dock)| d.actual_display_bounds(dock, &config))
    };

    for event in messages.read() {
        match (event, &mut preview.state) {
            (Event::MouseDown { point, .. }, State::Idle | State::Landing { .. }) => {
                // Fresh input interrupts a landing: reshuffles may move it again.
                if let State::Landing { entity, .. } = preview.state
                    && let Ok(mut cmd) = commands.get_entity(entity)
                {
                    cmd.try_remove::<DropDragged>();
                }
                preview.state = State::Idle;
                let Ok(id) = window_manager.find_window_at_point(point) else {
                    continue;
                };
                // Only a tiled window in a column of its own.
                let Some(entity) = strips.iter().find_map(|(_, strip, ..)| {
                    strip.columns().find_map(|column| match column {
                        Column::Single(e)
                            if windows
                                .get(*e)
                                .is_ok_and(|w| w.0.id() == id && w.4.is_none()) =>
                        {
                            Some(*e)
                        }
                        _ => None,
                    })
                }) else {
                    continue;
                };
                let Ok((_, position, bounds, ..)) = windows.get(entity) else {
                    continue;
                };
                let frame = IRect::from_corners(position.0, position.0 + bounds.0);
                preview.state = State::Pressed { entity, frame };
            }

            (Event::MouseDragged { .. }, State::Pressed { entity, frame }) => {
                let (entity, frame) = (*entity, *frame);
                let Ok((_, position, bounds, ..)) = windows.get(entity) else {
                    preview.state = State::Idle;
                    continue;
                };
                if bounds.0 != frame.size() {
                    // A resize, not a move.
                    preview.state = State::Idle;
                    continue;
                }
                let delta = position.0 - frame.min;
                if delta.x.abs().max(delta.y.abs()) < DRAG_START_PX {
                    continue;
                }
                let Some((strip_entity, index, display)) =
                    strips.iter().find_map(|(se, strip, _, child, _)| {
                        strip
                            .index_of(entity)
                            .ok()
                            .map(|index| (se, index, child.parent()))
                    })
                else {
                    preview.state = State::Idle;
                    continue;
                };
                let width = viewport(display).map_or(0, |v| v.width());
                let ratio = if width > 0 {
                    f64::from(frame.width()) / f64::from(width)
                } else {
                    1.0
                };
                for marker in &held {
                    if let Ok(mut cmd) = commands.get_entity(marker) {
                        cmd.try_despawn();
                    }
                }
                if let Ok(mut cmd) = commands.get_entity(entity) {
                    cmd.try_insert(DropDragged {
                        landing: frame.size(),
                        hovering: true,
                    });
                }
                DRAG_ACTIVE.store(true, Ordering::Relaxed);
                debug!("drop: start {entity} from strip {strip_entity} idx {index}");
                preview.state = State::Dragging(Session {
                    entity,
                    home: (strip_entity, index),
                    home_display: display,
                    home_size: frame.size(),
                    ratio,
                    at: (strip_entity, index),
                    cancelled: false,
                });
            }

            (Event::MouseUp { .. }, State::Pressed { .. }) => preview.state = State::Idle,

            (Event::DropCancel, State::Dragging(session)) => {
                session.cancelled = true;
                debug!("drop: cancel {}", session.entity);
                let home = session.home;
                let landing = session.home_size;
                let entity = session.entity;
                if session.at != home {
                    session.at = home;
                    move_to(&mut strips, entity, home);
                }
                if let Ok(mut cmd) = commands.get_entity(entity) {
                    cmd.try_insert(DropDragged {
                        landing,
                        hovering: true,
                    });
                }
            }

            (Event::MouseDragged { point, .. }, State::Dragging(session)) if !session.cancelled => {
                let Some((display, display_id)) = displays
                    .iter()
                    .min_by(|a, b| {
                        distance(a.1.bounds(), point).total_cmp(&distance(b.1.bounds(), point))
                    })
                    .map(|(e, d, _)| (e, d.id()))
                else {
                    continue;
                };
                let Ok(space) = window_manager.active_display_space(display_id) else {
                    continue;
                };
                let Some((target, strip_x)) =
                    strips.iter().find_map(|(se, strip, pos, child, sel)| {
                        (child.parent() == display && sel && strip.id() == space)
                            .then_some((se, pos.0.x))
                    })
                else {
                    continue;
                };
                let Some(view) = viewport(display) else {
                    continue;
                };
                let landing = if display == session.home_display {
                    session.home_size
                } else {
                    Size::new(
                        round_px(session.ratio * f64::from(view.width())),
                        view.height(),
                    )
                };
                let entity = session.entity;
                let width_of = |e: Entity| -> Option<IRect> {
                    if e == entity {
                        return Some(IRect::from_corners(bevy::math::IVec2::ZERO, landing));
                    }
                    windows
                        .get(e)
                        .ok()
                        .map(|w| IRect::from_corners(w.1.0, w.1.0 + w.2.0))
                };
                let Ok((_, strip, ..)) = strips.get(target) else {
                    continue;
                };
                // Columns other than the dragged one whose shown centre is left
                // of the pointer. Shown: with the gap where it is now, so a
                // neighbour has to be passed by half its width to swap.
                #[allow(clippy::cast_possible_truncation)]
                let px = point.x.round() as i32;
                let index = strip
                    .column_positions(&width_of)
                    .filter(|(column, _)| !column_has(column, entity))
                    .filter(|(column, left)| {
                        let w = column.width(&width_of).unwrap_or(0);
                        strip_x + left + w / 2 < px
                    })
                    .count();
                if session.at == (target, index) {
                    continue;
                }
                debug!(
                    "drop: hover {entity} strip {target} idx {index} landing {}x{}",
                    landing.x, landing.y
                );
                session.at = (target, index);
                move_to(&mut strips, entity, (target, index));
                if let Ok(mut cmd) = commands.get_entity(entity) {
                    cmd.try_insert(DropDragged {
                        landing,
                        hovering: true,
                    });
                }
            }

            (Event::MouseUp { .. }, State::Dragging(session)) => {
                DRAG_ACTIVE.store(false, Ordering::Relaxed);
                let entity = session.entity;
                let (target, index) = session.at;
                let target_display = strips.get(target).ok().map(|s| s.3.parent());
                let landing = match target_display.and_then(viewport) {
                    Some(view)
                        if !session.cancelled && target_display != Some(session.home_display) =>
                    {
                        Size::new(
                            round_px(session.ratio * f64::from(view.width())),
                            view.height(),
                        )
                    }
                    _ => session.home_size,
                };
                debug!(
                    "drop: release {entity} strip {target} idx {index} cancelled {} landing {}x{}",
                    session.cancelled, landing.x, landing.y
                );
                if let Ok((_, _, mut bounds, mut layout, ..)) = windows.get_mut(entity) {
                    if bounds.0 != landing {
                        bounds.0 = landing;
                    }
                    layout.set_changed();
                }
                if let Ok((_, mut strip, strip_pos, child, _)) = strips.get_mut(target) {
                    strip.set_changed();
                    // Scroll just enough to show the landing slot.
                    let left = strip
                        .column_positions(&|e: Entity| {
                            if e == entity {
                                Some(IRect::from_corners(bevy::math::IVec2::ZERO, landing))
                            } else {
                                windows
                                    .get(e)
                                    .ok()
                                    .map(|w| IRect::from_corners(w.1.0, w.1.0 + w.2.0))
                            }
                        })
                        .find_map(|(column, left)| column_has(column, entity).then_some(left));
                    if let (Some(left), Some(view)) = (left, viewport(child.parent())) {
                        let exposed = origin_exposing(
                            bevy::math::IVec2::new(left, 0),
                            landing,
                            strip_pos.0,
                            view,
                        );
                        if exposed != strip_pos.0 {
                            commands.reposition_entity(target, exposed);
                        }
                    }
                }
                if let Ok(mut cmd) = commands.get_entity(entity) {
                    cmd.try_insert(DropDragged {
                        landing,
                        hovering: false,
                    });
                }
                commands.focus_entity(entity, true);
                preview.state = State::Landing { entity, since: now };
            }
            _ => {}
        }
    }
}

fn column_has(column: &Column, entity: Entity) -> bool {
    matches!(column, Column::Single(e) if *e == entity)
}

/// Moves `entity` out of whichever strip holds it into `to` at `index`.
fn move_to(strips: &mut Strips, entity: Entity, (to, index): (Entity, usize)) {
    for (strip_entity, mut strip, ..) in strips.iter_mut() {
        if strip_entity != to && strip.contains(entity) {
            strip.remove(entity);
        }
    }
    if let Ok((_, mut strip, ..)) = strips.get_mut(to) {
        strip.remove(entity);
        strip.insert_at(index, entity);
    }
}
