//! `WOW_PROBE_CROWD`: a crowd of synthetic players around the probe, for what a populated city
//! costs in memory. Each is an `ObjectCreate` of a player dispatched through the net handlers
//! ([`crate::net::handlers::dispatch`]), the path a server's create takes, so its body, skin
//! composite, worn gear and weapons resolve as a real player's do; the item templates are asked
//! of the real server, as for any player. Nothing else about them is real: they stand still,
//! the server knows nothing of them, and nothing removes them but a map change.
//!
//! - `WOW_PROBE_CROWD="<n>[,<n>…]"`: the crowd's size at each step, cumulative.
//! - `WOW_PROBE_CROWD_AT` (default 15) seconds after world entry the first step lands, then one
//!   every `WOW_PROBE_CROWD_EVERY` (default 30); each logs `PROBE_CROWD: n=<size>`.
//! - `WOW_PROBE_CROWD_ITEMS="<slot>:<entry>,…;…"`: the gear pool by visible slot (0 head, 2
//!   shoulders, 3 shirt, 4 chest, 5 waist, 6 legs, 7 feet, 8 wrists, 9 hands, 14 back, 15 main
//!   hand, 16 off hand, 17 ranged, 18 tabard); `wow-optimize/tools/crowd_items.sh` draws it from
//!   the server's item table. Without it the crowd is naked.
//! - `WOW_PROBE_CROWD_SEED` (default 1): the same seed builds the same crowd, so two runs compare.
//! - `WOW_PROBE_CROWD_REACH` (default 50): how far, in yards, the fan of players reaches.
//!
//! Each player is a seeded draw: a race and sex, a class the race allows, appearance dials inside
//! the race's ranges (`CharCreateCatalog::ranges`), one item per visible slot from the pool
//! (an empty slot one time in four, as real players leave some bare). They stand in a fan in front
//! of the camera as it looks at the first step, all in view so frame rates compare: ±35° of its
//! look, from 6 yd to `WOW_PROBE_CROWD_REACH`, spread evenly over the area for the largest step's
//! count (nearest first, so each step fills outward), facing the camera. Each stands on the first
//! solid surface under its spot (terrain, or a city's WMO floor over it), cast from 15 yd above
//! the probe's height; the probe's height where nothing is hit.

use benilla_assets::coords::wow_to_bevy;
use benilla_protocol::field::{
    FIELD_OBJECT_TYPE, FIELD_PLAYER_BYTES, FIELD_PLAYER_BYTES_2,
    FIELD_PLAYER_VISIBLE_ITEM_1_CREATOR, FIELD_UNIT_BYTES_0, FIELD_UNIT_DISPLAYID,
    FIELD_UNIT_FACTIONTEMPLATE, FIELD_UNIT_HEALTH, FIELD_UNIT_LEVEL, FIELD_UNIT_MAXHEALTH,
    FIELD_UNIT_NATIVEDISPLAYID,
};
use benilla_protocol::messages::ObjectType;
use benilla_protocol::{EntityKind, ObjectFields, SessionEvent};
use bevy::ecs::system::SystemState;
use bevy::prelude::*;

/// The playable races of 1.12: Human, Orc, Dwarf, Night Elf, Undead, Tauren, Gnome, Troll.
const RACES: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

/// Synthetic player guids start here: high-guid 0 (a player), far above any real character's.
const GUID_BASE: u64 = 0x00F0_0000;

/// `OBJECT_FIELD_TYPE` of a player: OBJECT | UNIT | PLAYER.
const TYPE_PLAYER: u32 = 0x19;

/// The crowd steps and what has been spawned so far.
#[derive(Resource)]
struct ProbeCrowd {
    steps: Vec<u32>,
    at: f32,
    every: f32,
    seed: u64,
    /// `WOW_PROBE_CROWD_REACH`: the fan's far edge, in yards from the probe.
    reach: f32,
    /// The gear pool by visible slot.
    pool: Vec<(u16, Vec<u32>)>,
    spawned: u32,
    next_step: usize,
    /// WoW `(x, y, z)` of the probe and the camera's look (radians, WoW frame), fixed at the
    /// first step: the fan's apex and axis.
    centre: Option<[f32; 4]>,
}

fn env_f32(name: &str, default: f32) -> f32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

fn from_env() -> Option<ProbeCrowd> {
    let steps: Vec<u32> = std::env::var("WOW_PROBE_CROWD")
        .ok()?
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let pool = std::env::var("WOW_PROBE_CROWD_ITEMS")
        .unwrap_or_default()
        .split(';')
        .filter_map(|slot| {
            let (s, entries) = slot.split_once(':')?;
            let entries: Vec<u32> = entries
                .split(',')
                .filter_map(|e| e.trim().parse().ok())
                .collect();
            Some((s.trim().parse().ok()?, entries)).filter(|(_, e): &(u16, Vec<u32>)| !e.is_empty())
        })
        .collect();
    (!steps.is_empty()).then(|| ProbeCrowd {
        steps,
        at: env_f32("WOW_PROBE_CROWD_AT", 15.0),
        every: env_f32("WOW_PROBE_CROWD_EVERY", 30.0),
        seed: std::env::var("WOW_PROBE_CROWD_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1),
        reach: env_f32("WOW_PROBE_CROWD_REACH", 50.0),
        pool,
        spawned: 0,
        next_step: 0,
        centre: None,
    })
}

/// A seeded draw per player: splitmix64 over `(seed, index, salt)`.
fn draw(seed: u64, index: u32, salt: u64, below: u32) -> u32 {
    let mut z = seed
        .wrapping_add(u64::from(index).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(salt.wrapping_mul(0xD1B5_4A32_D192_ED03));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    if below == 0 {
        0
    } else {
        (z % u64::from(below)) as u32
    }
}

/// Player `index`'s create, standing at fan position `index` in front of `centre`.
fn player_create(
    crowd: &ProbeCrowd,
    catalog: &benilla_formats::CharCreateCatalog,
    index: u32,
    centre: [f32; 4],
    ground: &dyn Fn(f32, f32) -> Option<f32>,
) -> Option<SessionEvent> {
    let seed = crowd.seed;
    let race = RACES[draw(seed, index, 1, RACES.len() as u32) as usize];
    let sex = draw(seed, index, 2, 2) as u8;
    let classes = catalog.classes_for_race(race);
    let class = *classes.get(draw(seed, index, 3, classes.len() as u32) as usize)?;
    let display = catalog.body_display(race, sex)?;
    let ranges = catalog.ranges(race, sex)?;
    let dial = |salt: u64, count: u8| draw(seed, index, salt, u32::from(count)) as u8;
    let hair_style = dial(6, ranges.hair_style);
    let hair_colors = catalog.hair_colors(race, sex, hair_style).max(1);
    let look = u32::from(dial(4, ranges.skin))
        | u32::from(dial(5, ranges.face)) << 8
        | u32::from(hair_style) << 16
        | u32::from(dial(7, hair_colors)) << 24;

    let mut pairs: Vec<(u16, u32)> = vec![
        (FIELD_OBJECT_TYPE, TYPE_PLAYER),
        (FIELD_UNIT_HEALTH, 100),
        (FIELD_UNIT_MAXHEALTH, 100),
        (FIELD_UNIT_LEVEL, 60),
        // Horde and Alliance player faction templates, by race.
        (
            FIELD_UNIT_FACTIONTEMPLATE,
            match race {
                1 | 3 | 4 | 7 => 1,
                _ => 2,
            },
        ),
        (
            FIELD_UNIT_BYTES_0,
            u32::from(race) | u32::from(class) << 8 | u32::from(sex) << 16,
        ),
        (FIELD_UNIT_DISPLAYID, display),
        (FIELD_UNIT_NATIVEDISPLAYID, display),
        (FIELD_PLAYER_BYTES, look),
        (FIELD_PLAYER_BYTES_2, u32::from(dial(8, ranges.facial_hair))),
    ];
    let mut two_handed = false;
    for (slot, entries) in &crowd.pool {
        // A bare slot one time in four; an off hand only beside a one-handed main hand.
        if draw(seed, index, 100 + u64::from(*slot), 4) == 0 || (*slot == 16 && two_handed) {
            continue;
        }
        let entry =
            entries[draw(seed, index, 200 + u64::from(*slot), entries.len() as u32) as usize];
        // The pool marks a two-hander with the high bit (`crowd_items.sh`).
        if *slot == 15 && entry & 0x8000_0000 != 0 {
            two_handed = true;
        }
        pairs.push((
            FIELD_PLAYER_VISIBLE_ITEM_1_CREATOR + 2 + 12 * slot,
            entry & 0x7FFF_FFFF,
        ));
    }

    // The fan: distance by area (the square root spreads them evenly), bearing by the golden ratio.
    const NEAR: f32 = 6.0;
    const HALF_ARC: f32 = 35.0 * std::f32::consts::PI / 180.0;
    let total = crowd.steps.iter().copied().max().unwrap_or(1).max(1);
    let share = (index as f32 + 0.5) / total as f32;
    let distance = NEAR + (crowd.reach - NEAR).max(0.0) * share.sqrt();
    let bearing = centre[3] + HALF_ARC * (2.0 * (index as f32 * 0.618_034).fract() - 1.0);
    let (x, y) = (
        centre[0] + distance * bearing.cos(),
        centre[1] + distance * bearing.sin(),
    );
    // On the surface under the spot, else at the centre's height (nothing solid within reach).
    let position = [x, y, ground(x, y).unwrap_or(centre[2])];
    Some(SessionEvent::ObjectCreate {
        guid: GUID_BASE + u64::from(index),
        kind: EntityKind::Player,
        display_id: Some(display),
        position,
        // Facing back toward the camera.
        orientation: bearing + std::f32::consts::PI,
        scale: 1.0,
        speeds: None,
        mover: None,
        transport_progress: None,
        transport: None,
        spline: None,
        fields: ObjectFields::from_pairs(&pairs).into_created(ObjectType::Player),
    })
}

/// Lands each step on its time: the creates go through the net handlers like a server packet.
fn drive_probe_crowd(world: &mut World, mut entered: Local<Option<f32>>) {
    let now = world.resource::<Time<bevy::time::Real>>().elapsed_secs();
    let in_world = world
        .query_filtered::<(), With<crate::net::SelfPlayer>>()
        .iter(world)
        .next()
        .is_some();
    if !in_world {
        return;
    }
    let entered = *entered.get_or_insert(now);
    let Some(mut crowd) = world.remove_resource::<ProbeCrowd>() else {
        return;
    };
    let due = crowd.at + crowd.every * crowd.next_step as f32;
    if crowd.next_step < crowd.steps.len() && now - entered >= due {
        let target = crowd.steps[crowd.next_step];
        crowd.next_step += 1;
        if crowd.centre.is_none() {
            let p = world.resource::<crate::player::Player>().pos;
            // The camera's look on the ground: a chase camera behind the probe sees the crowd.
            let look = world
                .query_filtered::<&GlobalTransform, With<benilla_world::view::WorldCamera>>()
                .iter(world)
                .next()
                .map(|t| t.forward().as_vec3())
                .map(|f| Vec2::new(-f.z, -f.x).normalize_or_zero())
                .unwrap_or(Vec2::ZERO);
            // Bevy `(x, y, z)` is WoW `(-y, z, -x)`.
            crowd.centre = Some([-p.z, -p.x, p.y, look.y.atan2(look.x)]);
        }
        let centre = crowd.centre.expect("set above");
        let mut events = Vec::new();
        {
            let mut state = SystemState::<avian3d::prelude::SpatialQuery>::new(world);
            let spatial = state.get(world);
            // WoW `z` of the first solid surface below WoW `(x, y)`: terrain, a city's WMO floor, a
            // prop. Cast from 15 yd above the probe's feet: over a fan 50 yd deep the ground rises
            // more than a few yards, and a city's open ground has little overhead at that height.
            let ground = |x: f32, y: f32| {
                let from = wow_to_bevy([x, y, centre[2] + 15.0]);
                spatial
                    .cast_ray(
                        from,
                        Dir3::NEG_Y,
                        40.0,
                        true,
                        &avian3d::prelude::SpatialQueryFilter::default(),
                    )
                    .map(|hit| from.y - hit.distance)
            };
            if let Some(catalog) = world.get_resource::<crate::entities::CharCreate>() {
                for index in crowd.spawned..target {
                    events.extend(player_create(&crowd, &catalog.0, index, centre, &ground));
                }
            }
        }
        let made = events.len();
        events.push(SessionEvent::ObjectUpdateEnd);
        crate::net::handlers::dispatch(world, events);
        crowd.spawned = target.max(crowd.spawned);
        info!(
            "PROBE_CROWD: n={} (+{made}) t={:.0} at {:?}",
            crowd.spawned,
            now - entered,
            wow_to_bevy([centre[0], centre[1], centre[2]])
        );
    }
    world.insert_resource(crowd);
}

/// Registers `WOW_PROBE_CROWD`; inert when it is unset.
pub(crate) struct ProbeCrowdPlugin;

impl Plugin for ProbeCrowdPlugin {
    fn build(&self, app: &mut App) {
        if let Some(crowd) = from_env() {
            app.insert_resource(crowd).add_systems(
                Update,
                drive_probe_crowd
                    .in_set(benilla_world::schedule::WorldStage::Input)
                    .in_set(crate::client_state::InWorldGated),
            );
        }
    }
}
