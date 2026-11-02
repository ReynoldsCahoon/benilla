//! WMO walking colliders, built per group and only while the group's box meets the far box. The
//! reference's collision gather skips a group that is not loaded (`0x6a4a00`, models/
//! collisionquery#group-enumeration), and a group loads only while its box meets the far box,
//! half-size the far clip around the position (`0x698720`, terrain/mapupdate#map-object-update).
//! One trimesh for a whole building made a city's every face collide at once: Stormwind's, built
//! on landing in Northshire, was part of the spike that killed the iPad Air 2 (wow-optimize
//! FINDINGS, "The landing spike"). The faces themselves stay in the model ([`GroupTris`], a
//! slice of its collision mesh); only the trimesh and its tree come and go.

use avian3d::prelude::{Collider, CollisionLayers, RigidBody};
use benilla_assets::coords::wow_to_bevy;
use benilla_assets::GroupTris;
use bevy::prelude::*;

use super::collider::{build_collider_task, placement_collider_data, PendingCollider};
use super::ViewFocus;
use crate::collision::walk_layers;
use crate::view::WorldCamera;

/// Yards past the far box before a built group drops: the gap keeps a focus on the edge from
/// rebuilding every frame, as the camera colliders' reach gap does.
const DETACH_MARGIN: f32 = 30.0;

/// One WMO group's walking collider in its placement, owned by the placement.
#[derive(Component)]
pub(super) struct WalkGroupCollider {
    faces: GroupTris,
    transform: Transform,
    /// The group's face box in WoW world space: its local box's corners through the placement.
    min: [f32; 3],
    max: [f32; 3],
}

impl WalkGroupCollider {
    /// `bounds` is the group's face box in model space (`WmoModel::group_collision_bounds`).
    pub(super) fn new(
        faces: GroupTris,
        bounds: ([f32; 3], [f32; 3]),
        transform: Transform,
    ) -> Self {
        let (lo, hi) = bounds;
        let (mut min, mut max) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
        for corner in 0..8 {
            let pick = |axis: usize| {
                if corner >> axis & 1 == 0 {
                    lo[axis]
                } else {
                    hi[axis]
                }
            };
            let world =
                benilla_assets::coords::bevy_to_wow(transform.transform_point(wow_to_bevy([
                    pick(0),
                    pick(1),
                    pick(2),
                ])));
            for a in 0..3 {
                min[a] = min[a].min(world[a]);
                max[a] = max[a].max(world[a]);
            }
        }
        Self {
            faces,
            transform,
            min,
            max,
        }
    }

    /// Whether a body may be released over this group ([`super::Placement::up`]): its collider
    /// has attached (`built`), or its faces lie outside the far box around `at`, where
    /// [`stage_walk_colliders`] builds nothing.
    pub(super) fn stands(&self, built: bool, at: [f32; 3], far: f32) -> bool {
        built || self.past_far_box(at, far) > 0.0
    }

    /// How far outside the far box around `at` the group's box lies: the largest per-axis gap
    /// beyond `far` (0 or less when the boxes meet, the reference's closed test).
    fn past_far_box(&self, at: [f32; 3], far: f32) -> f32 {
        (0..3)
            .map(|a| (self.min[a] - at[a]).max(at[a] - self.max[a]) - far)
            .fold(f32::NEG_INFINITY, f32::max)
    }
}

/// Starts the build of every unbuilt group whose box meets the far box, and drops every built or
/// building group [`DETACH_MARGIN`] past it. A finished build attaches through
/// [`super::collider::finish_colliders`], on the walk layers, as the whole-building collider did.
pub(super) fn stage_walk_colliders(
    mut commands: Commands,
    focus: Res<ViewFocus>,
    camera: Query<&Transform, With<WorldCamera>>,
    view: Res<crate::view::ViewDistance>,
    groups: Query<(
        Entity,
        &WalkGroupCollider,
        Has<PendingCollider>,
        Has<Collider>,
    )>,
) {
    let at = focus.resolve(camera.single().ok().map(|c| c.translation));
    let far = view.far();
    for (entity, group, pending, built) in &groups {
        let past = group.past_far_box(at, far);
        if !pending && !built && past <= 0.0 {
            let mesh = group.faces.to_mesh();
            if let Some((verts, tris)) = placement_collider_data(Some(&mesh), &group.transform) {
                commands.entity(entity).insert(PendingCollider::new(
                    build_collider_task(verts, tris),
                    Some(walk_layers()),
                    true,
                ));
            }
        } else if (pending || built) && past > DETACH_MARGIN {
            // A removed `Collider` bumps `ColliderEpoch` (`track_collider_removals`).
            commands
                .entity(entity)
                .remove::<(PendingCollider, Collider, RigidBody, CollisionLayers)>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One group: a 2 yd quad at WoW `(x, 0..2, 0..2)`, two triangles.
    fn quad_at(x: f32) -> (GroupTris, ([f32; 3], [f32; 3])) {
        let (a, b, c, d) = ([x, 0.0, 0.0], [x, 2.0, 0.0], [x, 2.0, 2.0], [x, 0.0, 2.0]);
        (GroupTris::from(vec![[a, b, c], [a, c, d]]), (a, c))
    }

    fn app_at(focus_wow: [f32; 3]) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(ViewFocus::body(focus_wow, true))
            .insert_resource(crate::view::ViewDistance {
                farclip: 177.0,
                ..Default::default()
            })
            .add_systems(Update, stage_walk_colliders);
        app
    }

    fn spawn(app: &mut App, x: f32) -> Entity {
        let (faces, bounds) = quad_at(x);
        app.world_mut()
            .spawn(WalkGroupCollider::new(faces, bounds, Transform::IDENTITY))
            .id()
    }

    fn state(app: &App, e: Entity) -> (bool, bool) {
        let w = app.world();
        (
            w.get::<PendingCollider>(e).is_some(),
            w.get::<Collider>(e).is_some(),
        )
    }

    #[test]
    fn a_group_in_the_far_box_builds_and_one_beyond_does_not() {
        let mut app = app_at([0.0, 0.0, 0.0]);
        let near = spawn(&mut app, 170.0); // its face at x 170, inside the 177 box
        let far = spawn(&mut app, 300.0);
        app.update();
        assert_eq!(state(&app, near), (true, false), "the near group builds");
        assert_eq!(
            state(&app, far),
            (false, false),
            "the far group stays unbuilt"
        );
    }

    #[test]
    fn a_built_group_drops_past_the_margin_and_holds_inside_it() {
        let mut app = app_at([0.0, 0.0, 0.0]);
        let (faces, bounds) = quad_at(0.0);
        let e = app
            .world_mut()
            .spawn((
                WalkGroupCollider::new(faces, bounds, Transform::IDENTITY),
                Collider::sphere(1.0),
                RigidBody::Static,
                walk_layers(),
            ))
            .id();
        app.insert_resource(ViewFocus::body([200.0, 1.0, 1.0], true)); // 23 yd past the far box
        app.update();
        assert_eq!(
            state(&app, e),
            (false, true),
            "inside the margin, the collider holds"
        );
        app.insert_resource(ViewFocus::body([260.0, 1.0, 1.0], true));
        app.update();
        assert_eq!(state(&app, e), (false, false));
        assert!(app.world().get::<RigidBody>(e).is_none());
        app.insert_resource(ViewFocus::body([10.0, 1.0, 1.0], true));
        app.update();
        assert_eq!(
            state(&app, e),
            (true, false),
            "back in the far box: rebuilt"
        );
    }

    #[test]
    fn the_box_follows_the_placement() {
        let (faces, bounds) = quad_at(0.0);
        // WoW +x is Bevy −z: a placement 300 yd along Bevy −z carries the quad 300 yd along +x.
        let moved = WalkGroupCollider::new(faces, bounds, Transform::from_xyz(0.0, 0.0, -300.0));
        assert!(moved.past_far_box([300.0, 1.0, 1.0], 177.0) <= 0.0);
        assert!(moved.past_far_box([0.0, 1.0, 1.0], 177.0) > 100.0);
    }
}
