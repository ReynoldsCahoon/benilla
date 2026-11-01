//! Hands back ECS table capacity no entity uses any more: a low-spec measure, not a reference
//! behaviour (the reference has no ECS). Bevy's tables only grow: an archetype's table that once
//! held a city's units keeps their columns after the units despawn, so each teleport or map change
//! left the peak of every table behind. On the iPad Air 2's teleport soak (wow-optimize FINDINGS,
//! "Teleports keep the tables") that was 76 MB ten minutes in and the floor rose ~20 MB per hop.
//! The vendored `bevy_ecs` adds [`World::shrink_tables`]; this calls it every [`TRIM_SECS`].
//! `$WOW_TABLE_TRIM=0` turns it off (a before/after measurement).

use bevy::prelude::*;
use bevy::time::Real;

/// Real seconds between passes: a length check per table (~1600 after a long session), and a
/// table that empties and refills within one pass keeps its allocation.
const TRIM_SECS: f32 = 2.0;

fn trim_tables(world: &mut World, mut last: Local<f32>) {
    let now = world.resource::<Time<Real>>().elapsed_secs();
    if now - *last < TRIM_SECS {
        return;
    }
    *last = now;
    world.shrink_tables();
}

pub(crate) fn plugin(app: &mut App) {
    if std::env::var("WOW_TABLE_TRIM").is_ok_and(|v| v == "0") {
        info!("table-trim: off ($WOW_TABLE_TRIM=0)");
        return;
    }
    app.add_systems(Last, trim_tables);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;

    #[derive(Component)]
    struct Unit(#[allow(dead_code)] [u64; 8]);

    fn capacity(app: &App) -> usize {
        let world = app.world();
        let unit = world.component_id::<Unit>().unwrap();
        world
            .storages()
            .tables
            .iter()
            .filter(|t| t.has_column(unit))
            .map(|t| t.capacity())
            .sum()
    }

    /// A city's worth of units despawned: the next pass gives their table's columns back.
    #[test]
    fn a_despawned_crowd_leaves_no_table_capacity_behind() {
        let mut app = App::new();
        app.add_plugins(bevy::time::TimePlugin).add_plugins(plugin);
        let units: Vec<Entity> = (0..500)
            .map(|_| app.world_mut().spawn(Unit([0; 8])).id())
            .collect();
        app.update();
        assert!(capacity(&app) >= 500);
        for e in units {
            app.world_mut().despawn(e);
        }
        app.world_mut()
            .resource_mut::<Time<Real>>()
            .update_with_instant(
                std::time::Instant::now() + std::time::Duration::from_secs_f32(TRIM_SECS + 1.0),
            );
        app.world_mut().run_system_once(trim_tables).unwrap();
        assert_eq!(capacity(&app), 0);
    }
}
