//! `WOW_PROBE_ROUTE`: walks the avatar along waypoints, so a memory or streaming run repeats the
//! same path every time with no one at the keys. It faces each point and holds MoveForward through
//! the bindings, the virtual stick's path ([`crate::virtual_stick::hold_directions`]), and writes
//! the facing as [`super::probe_look`] does: from there it is a player's own walk, its streaming and
//! its movement packets.
//!
//! - Replay: `WOW_PROBE_ROUTE="<x>,<y>[,<secs>][;…]"` in WoW yards. From `WOW_PROBE_ROUTE_AT` seconds
//!   after world entry (default 15) it walks to each point in turn, logs `PROBE_ROUTE: at <i>/<n>`
//!   there, holds the point's own seconds or `WOW_PROBE_ROUTE_DWELL` (default 5), and after the last
//!   logs
//!   `PROBE_ROUTE: DONE` and exits. A point not reached in [`STUCK_SECS`] logs `STUCK` and is
//!   skipped.
//! - Start barrier: with `WOW_PROBE_ROUTE_GO=<file>` it logs `PROBE_ROUTE: READY` once the start
//!   delay has passed and waits for that file to exist, so two clients loaded at different speeds
//!   set off together (`tools/route_pair.sh` in the wow-optimize workbench touches it).
//! - Record: `WOW_PROBE_ROUTE_RECORD=<yards>` logs `PROBE_ROUTE: point <x>,<y>` each time the
//!   avatar has moved that far from the last point, so a route walked once by hand replays after.

use benilla_ui::script::{BindingInput, HeldInput};
use bevy::prelude::*;

use crate::player::Player;
use benilla_world::schedule::WorldStage;

/// The probe fleet's wall clock (`capture::ProbeClock`), spelled out here so the walker also
/// compiles alone into a `route-probe` build.
type ProbeClock<'w> = Res<'w, Time<bevy::time::Real>>;

/// Arrived within this many yards of a point (in the plane).
const ARRIVE_YD: f32 = 1.5;
/// A point not reached in this long is logged and skipped.
const STUCK_SECS: f32 = 60.0;

#[derive(Resource)]
struct ProbeRoute {
    /// Waypoints in WoW `(x, y)`, each with its dwell in seconds.
    points: Vec<([f32; 2], f32)>,
    at: f32,
    /// `WOW_PROBE_ROUTE_GO`: the file whose existence starts the walk; `None` once it exists.
    go: Option<std::path::PathBuf>,
    /// The point being walked to, and when the walk to it began; `None` before the start.
    leg: Option<(usize, f32)>,
    /// Arrived at the current point at this time, dwelling.
    arrived: Option<f32>,
    walking: bool,
    done: bool,
}

#[derive(Resource)]
struct ProbeRouteRecord {
    every: f32,
    last: Option<[f32; 2]>,
}

fn env_f32(name: &str, default: f32) -> f32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

/// Parses `WOW_PROBE_ROUTE`; unparseable points are skipped with a warning.
fn from_env() -> Option<ProbeRoute> {
    let spec = std::env::var("WOW_PROBE_ROUTE").ok()?;
    let dwell = env_f32("WOW_PROBE_ROUTE_DWELL", 5.0);
    let points: Vec<([f32; 2], f32)> = spec
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let f: Vec<Option<f32>> = s.split(',').map(|v| v.trim().parse().ok()).collect();
            match f.as_slice() {
                [Some(x), Some(y)] => Some(([*x, *y], dwell)),
                [Some(x), Some(y), Some(d)] => Some(([*x, *y], *d)),
                _ => {
                    warn!("probe-route: unparseable point {s:?} (want x,y or x,y,secs) — skipped");
                    None
                }
            }
        })
        .collect();
    (!points.is_empty()).then(|| ProbeRoute {
        points,
        at: env_f32("WOW_PROBE_ROUTE_AT", 15.0),
        go: std::env::var_os("WOW_PROBE_ROUTE_GO").map(Into::into),
        leg: None,
        arrived: None,
        walking: false,
        done: false,
    })
}

/// The avatar's feet in WoW `(x, y)`: Bevy `(x, y, z)` is WoW `(-y, z, -x)`.
fn wow_xy(player: &Player) -> [f32; 2] {
    [-player.pos.z, -player.pos.x]
}

/// The Bevy yaw that faces from `from` toward `to` (both WoW `(x, y)`): forward is
/// `rotation_y(yaw) · −Z`, and the WoW step `(dx, dy)` is the Bevy step `(−dy, −dx)`.
fn yaw_toward(from: [f32; 2], to: [f32; 2]) -> f32 {
    let (bx, bz) = (-(to[1] - from[1]), -(to[0] - from[0]));
    (-bx).atan2(-bz)
}

fn hold_forward(binds: &mut crate::bindings::BindingsState, on: bool) {
    binds.apply_device_input(vec![match on {
        true => BindingInput::Start(HeldInput::MoveForward),
        false => BindingInput::Stop(HeldInput::MoveForward),
    }]);
}

fn drive_probe_route(
    mut route: ResMut<ProbeRoute>,
    time: ProbeClock,
    mut player: ResMut<Player>,
    mut binds: ResMut<crate::bindings::BindingsState>,
    self_player: Query<(), With<crate::net::SelfPlayer>>,
    mut entered: Local<Option<f32>>,
    mut ready: Local<bool>,
    mut exit: MessageWriter<AppExit>,
    idle: Option<ResMut<crate::ui_chat::idle::LastInput>>,
) {
    if self_player.is_empty() || route.done {
        return;
    }
    // The probe stands in for a player at the keyboard: without input the idle handler marks
    // them AFK at 5 minutes and camps them at 30 (`ui_chat::idle`), which ends a long memory soak.
    if let Some(mut idle) = idle {
        idle.stamp_present(time.elapsed());
    }
    let now = time.elapsed_secs();
    let entered = *entered.get_or_insert(now);
    if now - entered < route.at {
        return;
    }
    if let Some(go) = &route.go {
        if !go.exists() {
            if !*ready {
                info!(
                    "PROBE_ROUTE: READY t={now:.0}, waiting for {}",
                    go.display()
                );
                *ready = true;
            }
            return;
        }
        info!("PROBE_ROUTE: GO t={now:.0}");
        route.go = None;
    }
    let here = wow_xy(&player);
    let (i, since) = *route.leg.get_or_insert((0, now));
    let n = route.points.len();
    if let Some(t) = route.arrived {
        if now - t < route.points[i].1 {
            return;
        }
        route.arrived = None;
        if i + 1 == n {
            info!("PROBE_ROUTE: DONE ({n} points) t={now:.0}");
            route.done = true;
            exit.write(AppExit::Success);
            return;
        }
        route.leg = Some((i + 1, now));
        return;
    }
    let to = route.points[i].0;
    let dist = ((to[0] - here[0]).powi(2) + (to[1] - here[1]).powi(2)).sqrt();
    if dist <= ARRIVE_YD || now - since > STUCK_SECS {
        if route.walking {
            hold_forward(&mut binds, false);
            route.walking = false;
        }
        match dist <= ARRIVE_YD {
            true => info!(
                "PROBE_ROUTE: at {}/{n} ({:.1}, {:.1}) t={now:.0}",
                i + 1,
                to[0],
                to[1]
            ),
            false => warn!(
                "PROBE_ROUTE: STUCK at {}/{n}: {dist:.1} yd short of ({:.1}, {:.1}) after {STUCK_SECS} s",
                i + 1,
                to[0],
                to[1]
            ),
        }
        route.arrived = Some(now);
        return;
    }
    let want = yaw_toward(here, to);
    let turn = (want - player.facing() + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU)
        - std::f32::consts::PI;
    player.turn_aim(turn);
    // A Start each frame, as the stick sends: no new edge on a held bit, and it restores one a
    // loading cover's bulk clear dropped.
    hold_forward(&mut binds, true);
    route.walking = true;
}

fn record_probe_route(
    mut rec: ResMut<ProbeRouteRecord>,
    player: Res<Player>,
    self_player: Query<(), With<crate::net::SelfPlayer>>,
) {
    if self_player.is_empty() {
        return;
    }
    let here = wow_xy(&player);
    let far = rec
        .last
        .is_none_or(|l| ((here[0] - l[0]).powi(2) + (here[1] - l[1]).powi(2)).sqrt() >= rec.every);
    if far {
        info!("PROBE_ROUTE: point {:.1},{:.1}", here[0], here[1]);
        rec.last = Some(here);
    }
}

/// Registers `WOW_PROBE_ROUTE` and `WOW_PROBE_ROUTE_RECORD` before
/// [`crate::player::PlayerControlSet`]; inert when neither is set.
pub(crate) struct ProbeRoutePlugin;

impl Plugin for ProbeRoutePlugin {
    fn build(&self, app: &mut App) {
        if let Some(route) = from_env() {
            app.insert_resource(route).add_systems(
                Update,
                drive_probe_route
                    .in_set(WorldStage::Input)
                    .before(crate::player::PlayerControlSet)
                    .in_set(crate::client_state::InWorldGated),
            );
        }
        let every = env_f32("WOW_PROBE_ROUTE_RECORD", 0.0);
        if every > 0.0 {
            app.insert_resource(ProbeRouteRecord { every, last: None })
                .add_systems(
                    Update,
                    record_probe_route
                        .in_set(WorldStage::Input)
                        .in_set(crate::client_state::InWorldGated),
                );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_yaw_faces_the_point() {
        // Forward is `rotation_y(yaw) · −Z`; the Bevy step it makes, back in WoW axes, must point
        // at the target.
        for to in [
            [10.0, 0.0],
            [0.0, 10.0],
            [-10.0, 0.0],
            [0.0, -10.0],
            [7.0, -3.0],
        ] {
            let yaw = yaw_toward([0.0, 0.0], to);
            let f = Quat::from_rotation_y(yaw) * Vec3::NEG_Z;
            let wow = [-f.z, -f.x];
            let len = (to[0] * to[0] + to[1] * to[1]).sqrt();
            assert!((wow[0] - to[0] / len).abs() < 1e-5 && (wow[1] - to[1] / len).abs() < 1e-5);
        }
    }
}
