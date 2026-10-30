//! WMO camera colliders, built per group and only near the focus. The camera faces (DETAIL kept,
//! NOCAMCOLLIDE dropped) answer one query alone, [`crate::collision::WorldCollision::cast_camera`],
//! the boom from the avatar's head, which reaches at most 50 yd (`cameraDistanceMax ×
//! cameraDistanceMaxFactor`, held to 50 at `0x5112d6`): a group farther away never answers it. A
//! city's camera faces are ~4× its walking faces (Orgrimmar: 567K triangles, ~50 MB as trimeshes),
//! so a group's trimesh is built when the focus comes within [`ATTACH_REACH`] of its box and
//! dropped past [`DETACH_REACH`].

use std::sync::Arc;

use avian3d::prelude::{Collider, CollisionLayers, RigidBody};
use benilla_assets::coords::wow_to_bevy;
use benilla_assets::GroupCollision;
use bevy::prelude::*;

use super::collider::{build_collider_task, placement_collider_data, PendingCollider};
use super::ViewFocus;
use crate::collision::camera_layers;
use crate::view::WorldCamera;

/// Build within this many yards of a group's box: the boom's 50 yd ceiling plus margin for the
/// build's latency at mounted speed.
const ATTACH_REACH: f32 = 80.0;
/// Drop past this many yards; the gap keeps a focus on the edge from rebuilding every frame.
const DETACH_REACH: f32 = 100.0;

/// One WMO group's camera collider in its placement, owned by the placement's tile.
#[derive(Component)]
pub(super) struct CameraGroupCollider {
    groups: Arc<[GroupCollision]>,
    index: usize,
    transform: Transform,
    /// The group's box in Bevy world space: its local box's corners through the placement.
    min: Vec3,
    max: Vec3,
}

impl CameraGroupCollider {
    pub(super) fn new(groups: Arc<[GroupCollision]>, index: usize, transform: Transform) -> Self {
        let (lo, hi) = groups[index].bounds;
        let (mut min, mut max) = (Vec3::INFINITY, Vec3::NEG_INFINITY);
        for corner in 0..8 {
            let pick = |axis: usize| {
                if corner >> axis & 1 == 0 {
                    lo[axis]
                } else {
                    hi[axis]
                }
            };
            let world = transform.transform_point(wow_to_bevy([pick(0), pick(1), pick(2)]));
            min = min.min(world);
            max = max.max(world);
        }
        Self {
            groups,
            index,
            transform,
            min,
            max,
        }
    }

    fn distance_to(&self, at: Vec3) -> f32 {
        at.clamp(self.min, self.max).distance(at)
    }
}

/// Starts the build of every unbuilt group within [`ATTACH_REACH`] of the focus, and drops every
/// built or building group past [`DETACH_REACH`]. A finished build attaches through
/// [`super::collider::finish_colliders`], on the camera layer, as the merged collider did.
pub(super) fn stage_camera_colliders(
    mut commands: Commands,
    focus: Res<ViewFocus>,
    camera: Query<&Transform, With<WorldCamera>>,
    groups: Query<(
        Entity,
        &CameraGroupCollider,
        Has<PendingCollider>,
        Has<Collider>,
    )>,
) {
    let at = wow_to_bevy(focus.resolve(camera.single().ok().map(|c| c.translation)));
    for (entity, group, pending, built) in &groups {
        let distance = group.distance_to(at);
        if !pending && !built && distance <= ATTACH_REACH {
            let mesh = &group.groups[group.index].mesh;
            if let Some((verts, tris)) = placement_collider_data(Some(mesh), &group.transform) {
                commands.entity(entity).insert(PendingCollider::new(
                    build_collider_task(verts, tris),
                    Some(camera_layers()),
                    true,
                ));
            }
        } else if (pending || built) && distance > DETACH_REACH {
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
    use benilla_formats::CollisionMesh;

    /// One group: a 2 yd quad at WoW `(x, 0..2, 0..2)`, two triangles.
    fn quad_at(x: f32) -> GroupCollision {
        GroupCollision::new(CollisionMesh {
            positions: vec![[x, 0.0, 0.0], [x, 2.0, 0.0], [x, 2.0, 2.0], [x, 0.0, 2.0]],
            indices: vec![0, 1, 2, 0, 2, 3],
        })
        .expect("two faces")
    }

    fn app_at(focus_wow: [f32; 3]) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(ViewFocus::body(focus_wow, true))
            .add_systems(Update, stage_camera_colliders);
        app
    }

    fn state(app: &mut App, e: Entity) -> (bool, bool) {
        let w = app.world();
        (
            w.get::<PendingCollider>(e).is_some(),
            w.get::<Collider>(e).is_some(),
        )
    }

    #[test]
    fn a_group_within_reach_builds_and_one_beyond_does_not() {
        let groups: Arc<[GroupCollision]> = Arc::from(vec![quad_at(10.0), quad_at(500.0)]);
        let mut app = app_at([0.0, 0.0, 0.0]);
        let near = app
            .world_mut()
            .spawn(CameraGroupCollider::new(
                groups.clone(),
                0,
                Transform::IDENTITY,
            ))
            .id();
        let far = app
            .world_mut()
            .spawn(CameraGroupCollider::new(groups, 1, Transform::IDENTITY))
            .id();
        app.update();
        assert_eq!(
            state(&mut app, near),
            (true, false),
            "the near group starts building"
        );
        assert_eq!(
            state(&mut app, far),
            (false, false),
            "the far group stays unbuilt"
        );
    }

    #[test]
    fn a_built_group_drops_past_the_detach_reach_and_holds_inside_it() {
        let groups: Arc<[GroupCollision]> = Arc::from(vec![quad_at(0.0)]);
        let mut app = app_at([0.0, 0.0, 0.0]);
        let e = app
            .world_mut()
            .spawn((
                CameraGroupCollider::new(groups, 0, Transform::IDENTITY),
                Collider::sphere(1.0),
                RigidBody::Static,
                camera_layers(),
            ))
            .id();
        // Between the reaches: held.
        app.insert_resource(ViewFocus::body([90.0, 0.0, 0.0], true));
        app.update();
        assert_eq!(
            state(&mut app, e),
            (false, true),
            "inside the gap, the collider holds"
        );
        // Past the detach reach: dropped, with its body and layers.
        app.insert_resource(ViewFocus::body([150.0, 0.0, 0.0], true));
        app.update();
        assert_eq!(state(&mut app, e), (false, false));
        assert!(app.world().get::<RigidBody>(e).is_none());
        assert!(app.world().get::<CollisionLayers>(e).is_none());
        // Back within reach: rebuilt.
        app.insert_resource(ViewFocus::body([10.0, 0.0, 0.0], true));
        app.update();
        assert_eq!(state(&mut app, e), (true, false));
    }

    #[test]
    fn the_box_follows_the_placement() {
        let groups: Arc<[GroupCollision]> = Arc::from(vec![quad_at(0.0)]);
        // WoW +x is Bevy −z: a placement 300 yd along Bevy −z carries the quad there.
        let moved = CameraGroupCollider::new(groups, 0, Transform::from_xyz(0.0, 0.0, -300.0));
        assert!(moved.distance_to(wow_to_bevy([300.0, 1.0, 1.0])) < 0.01);
        assert!(moved.distance_to(wow_to_bevy([0.0, 1.0, 1.0])) > 290.0);
    }
}
