//! The crowd cap: when [`benilla_world::memory_governor`] runs low, other players' bodies past the
//! nearest few are not built, and at its last tier the farthest built ones are taken down.
//!
//! Deviation: the reference draws every player the server shows, however many; on a device that
//! kills the app at a fixed footprint a crowd would end the session (the iPad Air 2 dies at ~300
//! standing players, ~0.9 MB each). Under the governor's [`MemoryTier::Low`] a player ranked past
//! [`LOW_CAP`] by distance from the avatar waits unbuilt ([`BodyHeld`]), as one waiting on its gear
//! does; under [`MemoryTier::Critical`] the built ones past [`CRITICAL_CAP`] are torn down to wait
//! too. A held player is a unit with no body: no model, nothing to click. Only
//! [`MemoryTier::Normal`] releases them, a few a second, each fading in as on arrival; a cut that
//! leaves the governor in [`MemoryTier::Tight`] holds them, so a crowd never cycles between the two.

use benilla_protocol::EntityKind;
use benilla_world::memory_governor::{tier, MemoryTier};
use bevy::ecs::entity::EntityHashSet;
use bevy::prelude::*;
use bevy::time::Real;

use crate::net::{NetEntity, SelfPlayer};

use super::VisualAttached;

/// Players drawn at most, nearest first, before a new one waits under [`MemoryTier::Low`].
const LOW_CAP: usize = 60;

/// Players kept built under [`MemoryTier::Critical`]; the farther ones are taken down.
const CRITICAL_CAP: usize = 30;
const _: () = assert!(CRITICAL_CAP < LOW_CAP, "the last tier keeps fewer");

/// Real seconds between rankings: players move a few yards a second, and a ranking sorts them all.
const TICK_SECS: f32 = 1.0;

/// Held players released per tick back at [`MemoryTier::Normal`], so the rebuild is a trickle the
/// governor can watch, not one spike.
const RELEASE_PER_TICK: usize = 5;

/// A player whose body the crowd cap holds back; `attach_entity_visuals` skips it.
#[derive(Component)]
pub(crate) struct BodyHeld;

/// What each tier does to the players past its cap, nearest first.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Release up to [`RELEASE_PER_TICK`] held players.
    Release,
    /// Change nothing.
    Keep,
    /// Hold the players ranked at `cap` or farther; tear down the built ones among them when
    /// `strip`; release the held ones nearer.
    Cap { cap: usize, strip: bool },
}

fn plan(tier: MemoryTier) -> Plan {
    match tier {
        MemoryTier::Normal => Plan::Release,
        MemoryTier::Tight => Plan::Keep,
        MemoryTier::Low => Plan::Cap {
            cap: LOW_CAP,
            strip: false,
        },
        MemoryTier::Critical => Plan::Cap {
            cap: CRITICAL_CAP,
            strip: true,
        },
    }
}

/// The cap's memory between rankings.
#[derive(Default)]
pub(super) struct CapState {
    next_tick: f32,
    /// The players the last ranking placed inside the cap. Any other unbuilt player is held the
    /// frame it appears: its gear settles and `attach_entity_visuals` builds it within a frame or
    /// two, well inside a ranking's second.
    within: EntityHashSet,
    /// Players held as of the last ranking's log, so a line prints only on a change.
    logged_held: usize,
}

#[allow(clippy::type_complexity)]
pub(super) fn cap_player_bodies(
    mut commands: Commands,
    time: Res<Time<Real>>,
    mut state: Local<CapState>,
    avatar: Query<&GlobalTransform, With<SelfPlayer>>,
    players: Query<
        (
            Entity,
            &NetEntity,
            &GlobalTransform,
            Has<VisualAttached>,
            Has<BodyHeld>,
        ),
        (
            Without<SelfPlayer>,
            Without<benilla_world::model_fade::DespawnFade>,
        ),
    >,
) {
    let plan = plan(tier());
    let now = time.elapsed_secs();
    let tick = now >= state.next_tick;
    if tick {
        state.next_tick = now + TICK_SECS;
    }
    let Plan::Cap { cap, strip } = plan else {
        state.within.clear();
        state.logged_held = 0;
        if plan == Plan::Release && tick {
            let held = players.iter().filter(|p| p.4).take(RELEASE_PER_TICK);
            for (entity, ..) in held {
                commands.entity(entity).remove::<BodyHeld>();
            }
        }
        return;
    };
    let is_player =
        |p: &(Entity, &NetEntity, &GlobalTransform, bool, bool)| p.1.kind == EntityKind::Player;
    if !tick {
        // Between rankings: a newcomer waits for the next.
        for (entity, ..) in players
            .iter()
            .filter(|p| is_player(p) && !p.3 && !p.4 && !state.within.contains(&p.0))
        {
            commands.entity(entity).insert(BodyHeld);
        }
        return;
    }
    let Ok(origin) = avatar.single().map(GlobalTransform::translation) else {
        return;
    };
    let mut ranked: Vec<_> = players
        .iter()
        .filter(is_player)
        .map(|(e, _, gt, built, held)| (gt.translation().distance_squared(origin), e, built, held))
        .collect();
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0));
    state.within = ranked.iter().take(cap).map(|r| r.1).collect();
    let (mut held, mut stripped, mut released) = (0, 0, 0);
    for (rank, &(_, entity, built, was_held)) in ranked.iter().enumerate() {
        match (rank < cap, built, was_held) {
            (true, _, true) => {
                commands.entity(entity).remove::<BodyHeld>();
                released += 1;
            }
            (false, false, _) => {
                commands.entity(entity).insert(BodyHeld);
                held += 1;
            }
            (false, true, _) if strip => {
                super::live_display::tear_down_visual(&mut commands, entity).insert(BodyHeld);
                stripped += 1;
                held += 1;
            }
            _ => {}
        }
    }
    if held != state.logged_held || stripped + released > 0 {
        state.logged_held = held;
        info!(
            "crowd cap: {:?}, {} players, cap {cap}: {held} held ({stripped} taken down), \
             {released} released",
            tier(),
            ranked.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only Normal releases, Tight holds what it has, and the caps tighten with the tier.
    #[test]
    fn each_tier_plans_its_cut() {
        assert_eq!(plan(MemoryTier::Normal), Plan::Release);
        assert_eq!(plan(MemoryTier::Tight), Plan::Keep);
        assert_eq!(
            plan(MemoryTier::Low),
            Plan::Cap {
                cap: LOW_CAP,
                strip: false
            }
        );
        assert_eq!(
            plan(MemoryTier::Critical),
            Plan::Cap {
                cap: CRITICAL_CAP,
                strip: true
            }
        );
    }
}
