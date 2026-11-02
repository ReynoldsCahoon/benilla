//! A WMO placement's groups, built and spawned while each group's box meets the far box and freed
//! [`super::super::WMO_FAR_TIMER_SECS`] after it leaves, as the reference loads a group when its box
//! meets the far box and frees it on the same timer (`0x698720`, terrain/mapupdate#map-object-
//! update; `0x6b3a14`, terrain/portals). A placement spawned the render forms of every group at
//! once: landing near Stormwind built the whole city's meshes in a few seconds, and that burst
//! killed the iPad Air 2 (wow-optimize FINDINGS, "The landing spike"). The placement itself (the
//! portal instance, the colliders, the lights and the props) still spawns whole with its model.

use std::sync::Arc;

use benilla_assets::coords::{bevy_to_wow, wow_to_bevy};
use benilla_assets::materials::WowModelMaterial;
use benilla_assets::{WmoGroupGeom, WmoModel};
use bevy::prelude::*;
use bevy::time::Real;

use super::super::merge::{MergeSite, StaticMerge};
use super::super::{
    meets_far_box, ModelHandle, Placements, TerrainStreamer, ViewFocus, WMO_FAR_TIMER_SECS,
};
use super::{emitter_fade, placement_priority, spawn_model_entities, tag_world_object};
use super::{FormSlices, ShadeSel, SpawnedModel};
use crate::collision::{GroundDecalSurface, PickOccluder};
use crate::interact::WorldObject;
use crate::lighting::SharedLightBuffer;
use crate::liquid::{spawn_wmo_liquids, LiquidAssets, WmoPool};
use crate::model_forms::{ModelForms, ModelKey, WANT_STATIC};
use crate::view::WorldCamera;
use crate::wmo_portal::{WmoGroupVis, WmoRoom};

/// Groups spawned per frame at most while the stream is budgeted: a city's groups entering
/// together spread over a few frames, as the placements do.
const GROUPS_PER_FRAME: usize = 24;

/// One group's spawn state in its placement.
#[derive(Default)]
struct GroupSpawn {
    ents: Vec<Entity>,
    spawned: bool,
    /// The group's walking collider, when it has faces to walk on.
    walk: Option<Entity>,
    /// Seconds the group's box has been outside the far box while spawned.
    out_secs: f32,
}

/// A spawned WMO placement's per-group render state.
pub(in crate::terrain_stream) struct WmoGroups {
    /// The root's `mpq://` path less `.wmo`: group `g`'s file is `<stem>_<g:03>.wmo`.
    stem: Option<String>,
    object: Arc<WorldObject>,
    has_portals: bool,
    /// Each group's MOGI box through the placement, in WoW world coords.
    boxes: Vec<([f32; 3], [f32; 3])>,
    /// One shared cull key per group, not per batch (a city has ~100k batches).
    keys: Vec<Arc<[u16]>>,
    state: Vec<GroupSpawn>,
}

impl WmoGroups {
    pub(in crate::terrain_stream) fn new(
        m: &WmoModel,
        stem: Option<String>,
        transform: &Transform,
        object: Arc<WorldObject>,
        has_portals: bool,
    ) -> Self {
        let boxes = m
            .group_nav
            .iter()
            .map(|g| world_box(g.bbox_min, g.bbox_max, transform))
            .collect();
        let keys = (0..m.group_nav.len() as u16)
            .map(|g| Arc::from([g].as_slice()))
            .collect();
        Self {
            stem,
            object,
            has_portals,
            boxes,
            keys,
            state: (0..m.group_nav.len())
                .map(|_| GroupSpawn::default())
                .collect(),
        }
    }

    /// How many groups have spawned: the residency key's share of this placement.
    pub(in crate::terrain_stream) fn spawned_count(&self) -> usize {
        self.state.iter().filter(|s| s.spawned).count()
    }

    /// Every group whose box meets the far box has spawned with its walking collider ready to stand
    /// on (`walk_built`): the loading bar's and the settle release's test. The reference loads a
    /// group's collision with the group (`0x6a4a00` gathers only loaded groups), so a body is never
    /// released over a group that draws but cannot be stood on; the walking collider builds a
    /// frame and an off-thread trimesh after the group spawns, and a release in that gap dropped
    /// the body through a building's floor and the terrain hole under it.
    pub(in crate::terrain_stream) fn in_range_spawned(
        &self,
        center: [f32; 3],
        far: f32,
        walk_built: &dyn Fn(Entity) -> bool,
    ) -> bool {
        self.boxes.iter().zip(&self.state).all(|(b, s)| {
            !meets_far_box(b, center, far) || (s.spawned && s.walk.is_none_or(walk_built))
        })
    }
}

/// A model-space box through the placement, as a WoW world box: its eight corners' hull.
fn world_box(lo: [f32; 3], hi: [f32; 3], transform: &Transform) -> ([f32; 3], [f32; 3]) {
    if lo.iter().zip(&hi).any(|(a, b)| a > b) {
        // An unreadable group's empty box (`+inf..-inf`): it never meets.
        return ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    }
    let (mut min, mut max) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    for corner in 0..8 {
        let pick = |a: usize| if corner >> a & 1 == 0 { lo[a] } else { hi[a] };
        let w = bevy_to_wow(transform.transform_point(wow_to_bevy([pick(0), pick(1), pick(2)])));
        for a in 0..3 {
            min[a] = min[a].min(w[a]);
            max[a] = max[a].max(w[a]);
        }
    }
    (min, max)
}

/// One model group's load, shared by every placement of the model: the reference's group lives in
/// its model (`CMapObj`), loaded while any of its placements wants it.
struct GroupLoad {
    handle: Option<Handle<WmoGroupGeom>>,
    installed: bool,
    /// The frame a placement last wanted it (in the far box, or spawned inside its timer).
    wanted: u64,
}

/// The model groups loading or installed, by model and group.
#[derive(Resource, Default)]
pub(in crate::terrain_stream) struct WmoGroupLoads {
    loads: std::collections::HashMap<(AssetId<WmoModel>, u16), GroupLoad>,
    frame: u64,
}

/// The render tables a group spawn writes, one parameter under Bevy's limit.
type GroupTables<'w> = (
    ResMut<'w, Assets<WowModelMaterial>>,
    ResMut<'w, Assets<Mesh>>,
    ResMut<'w, crate::doodad_anim::UvAnimMaterials>,
    ResMut<'w, crate::doodad_anim::TintAnimMaterials>,
    ResMut<'w, crate::mat_anim_table::MatAnimTable>,
    ResMut<'w, StaticMerge>,
    // `None` only under `WOW_STATIC_GX=0`.
    Option<ResMut<'w, crate::static_gx::StaticGx>>,
);

/// Spawns each spawned WMO placement's groups whose box meets the far box once their forms are
/// built, and frees a spawned group whose box has stayed outside for the timer: its entities,
/// its retained items and its forms.
#[allow(clippy::too_many_arguments)]
pub(in crate::terrain_stream) fn spawn_wmo_groups(
    mut commands: Commands,
    placements: ResMut<Placements>,
    mut wmos: ResMut<Assets<WmoModel>>,
    // The group loads and their arrivals, which move into the model.
    loading: (
        ResMut<WmoGroupLoads>,
        ResMut<Assets<WmoGroupGeom>>,
        Res<AssetServer>,
    ),
    mut forms: ResMut<ModelForms>,
    tables: GroupTables,
    shared_light: Option<Res<SharedLightBuffer>>,
    liquid_assets: Option<Res<LiquidAssets>>,
    streamer: Res<TerrainStreamer>,
    focus: Res<ViewFocus>,
    camera: Query<&Transform, With<WorldCamera>>,
    view: Res<crate::view::ViewDistance>,
    time: Res<Time<Real>>,
) {
    let (mut materials, mut meshes, mut uv_reg, mut tint_reg, mut anim_table, mut merge, mut gx) =
        tables;
    let (mut loads, mut geoms, asset_server) = loading;
    loads.frame += 1;
    let frame = loads.frame;
    let Some(shared_light) = shared_light else {
        return;
    };
    let center = focus.resolve(camera.single().ok().map(|c| c.translation));
    let (far, dt) = (view.far(), time.delta_secs());
    let mut budget = if focus.budgeted() {
        GROUPS_PER_FRAME
    } else {
        usize::MAX
    };
    let Placements {
        by_id,
        materials: mat_cache,
        pending_spawns,
    } = placements.into_inner();
    for (&uid, p) in by_id.iter_mut() {
        let (ModelHandle::Wmo(h), Some(groups)) = (&p.model, p.wmo_groups.as_mut()) else {
            continue;
        };
        let Some(m) = wmos.get(h) else {
            continue;
        };
        let instance = p.portal_instance;
        let prio = placement_priority(&streamer, p.transform.translation);
        for g in 0..groups.state.len() {
            let gi = g as u16;
            let key = ModelKey::WmoGroup(h.id(), gi);
            let inside = meets_far_box(&groups.boxes[g], center, far);
            let st = &mut groups.state[g];
            // Wanted in the far box, and while spawned inside its timer: its data stays loaded.
            let installed = if inside || st.spawned {
                let load = loads.loads.entry((h.id(), gi)).or_insert(GroupLoad {
                    handle: None,
                    installed: false,
                    wanted: frame,
                });
                load.wanted = frame;
                if load.handle.is_none() && !load.installed {
                    if let Some(stem) = &groups.stem {
                        load.handle =
                            Some(asset_server.load::<WmoGroupGeom>(format!("{stem}_{gi:03}.wmo")));
                    }
                }
                load.installed
            } else {
                false
            };
            if !inside {
                if st.spawned {
                    st.out_secs += dt;
                    if st.out_secs >= WMO_FAR_TIMER_SECS {
                        let gone: std::collections::HashSet<Entity> = st.ents.drain(..).collect();
                        for &e in &gone {
                            commands.entity(e).try_despawn();
                        }
                        p.entities.retain(|e| !gone.contains(e));
                        st.spawned = false;
                        st.walk = None;
                        st.out_secs = 0.0;
                        if let (Some(gx), Some(i)) = (gx.as_deref_mut(), instance) {
                            gx.release_wmo_group(i, gi);
                        }
                        forms.release(key);
                    }
                }
                continue;
            }
            st.out_secs = 0.0;
            if st.spawned || budget == 0 || !installed {
                continue;
            }
            let range = m.group_submeshes(gi);
            if !range.is_empty() && !forms.require(key, WANT_STATIC, prio) {
                continue; // forms still building: next frame
            }
            budget -= 1;
            let fade = emitter_fade(p.transform, (f32::INFINITY, Vec3::ZERO), None, None);
            let mut ents = Vec::new();
            if !range.is_empty() {
                let SpawnedModel {
                    entities, by_batch, ..
                } = spawn_model_entities(
                    &mut commands,
                    mat_cache,
                    &mut materials,
                    &shared_light.0,
                    &m.submeshes[range.clone()],
                    FormSlices {
                        stat: forms.static_meshes(key).unwrap_or(&[]),
                        skin: None,
                    },
                    p.transform,
                    &groups.object,
                    ShadeSel::Matte, // unread: WMO lights on the FFP N·L path
                    None, // WMO groups carry their own per-submesh interior flag + batch class
                    Some(&fade),
                    f32::INFINITY,
                    Vec3::ZERO,
                    None, // no authored M2 box: group geometry never animates
                    None, // not an M2
                    &mut uv_reg,
                    &mut tint_reg,
                    &mut anim_table,
                    false, // world-static: not the entity-hosted lane
                    None,  // world-static placement: cards bake their world pivot
                    Some((
                        &mut *merge,
                        MergeSite::Wmo {
                            uid,
                            groups: &m.submesh_group[range.clone()],
                            portal_gated: groups.has_portals,
                        },
                    )),
                    // A retained region keyed by the instance; without one there is no PVS key.
                    instance.and_then(|i| {
                        gx.as_deref_mut().map(|gx| {
                            (
                                gx,
                                crate::static_gx::GxSite::Wmo {
                                    instance: i,
                                    groups: &m.submesh_group[range.clone()],
                                },
                            )
                        })
                    }),
                );
                // A world WMO is exterior scene too (`0x6856c0`); the camera's own building is
                // exempted per frame from `CameraInteriorClaim`.
                for e in &entities {
                    commands
                        .entity(*e)
                        .insert(crate::exterior_cull::ExteriorScene);
                }
                if let (Some(instance), true) = (instance, groups.has_portals) {
                    for e in by_batch.iter().flatten() {
                        commands.entity(*e).insert(WmoGroupVis {
                            instance,
                            groups: groups.keys[g].clone(),
                        });
                    }
                }
                ents = entities;
            }
            // The embedded MLIQ liquid of this group, taking the room's cull key.
            if let Some(lq) = m.group_liquids.get(g).and_then(Option::as_ref) {
                let first = ents.len();
                spawn_wmo_liquids(
                    &mut commands,
                    std::iter::once(lq),
                    liquid_assets.as_deref(),
                    &mut meshes,
                    p.transform,
                    // Interior (`MOGI & 0x48 == 0`): the pool takes the room's fog.
                    m.group_bounds.get(g).is_some_and(|b| b.interior),
                    WmoPool::new(
                        instance.map(|instance| WmoRoom {
                            instance,
                            group: gi,
                        }),
                        &p.transform,
                        m.group_bounds.get(g),
                    ),
                    // The root's MOMT diffColor table: an interior pool's body colour.
                    &m.material_diff_color,
                    &mut ents,
                );
                for &e in &ents[first..] {
                    commands
                        .entity(e)
                        .insert(crate::exterior_cull::ExteriorScene);
                    if let Some(instance) = instance {
                        commands.entity(e).insert(WmoGroupVis {
                            instance,
                            groups: groups.keys[g].clone(),
                        });
                    }
                }
            }
            // The walking faces build while the group's box meets the far box, the camera faces
            // near the boom ([`super::super::walk_collider`], [`super::super::camera_collider`]).
            let mut walk = None;
            if let (Some(faces), Some(Some(bounds))) = (
                m.group_collision_tris.get(g).filter(|f| !f.is_empty()),
                m.group_collision_bounds.get(g),
            ) {
                let id = commands
                    .spawn((
                        super::super::walk_collider::WalkGroupCollider::new(
                            faces.clone(),
                            *bounds,
                            p.transform,
                        ),
                        // The walk faces take the selection ring (floors, steps).
                        GroundDecalSurface,
                        // Clamps the mouse pick: the walk bake is the nearest occluder set.
                        PickOccluder,
                    ))
                    .id();
                ents.push(id);
                walk = Some(id);
            }
            if let Some(Some(faces)) = m.group_camera.get(g) {
                ents.push(
                    commands
                        .spawn(super::super::camera_collider::CameraGroupCollider::new(
                            faces.clone(),
                            p.transform,
                        ))
                        .id(),
                );
            }
            tag_world_object(&mut commands, &ents, &groups.object);
            p.entities.extend_from_slice(&ents);
            let st = &mut groups.state[g];
            st.ents = ents;
            st.spawned = true;
            st.walk = walk;
            // The props this group names load with it, as the reference creates a group's
            // doodads once the group is loaded; the spawner takes them as their models land.
            for d in p.doodads.iter_mut() {
                if !d.wanted && d.groups.contains(&gi) {
                    d.wanted = true;
                    d.handle = asset_server.load(d.url.clone());
                    *pending_spawns += 1;
                }
            }
        }
    }
    // Arrivals move into their model; a group no placement wanted this frame leaves it.
    loads.loads.retain(|&(model, gi), load| {
        if load.wanted != frame {
            if load.installed {
                if let Some(m) = wmos.get_mut_untracked(model) {
                    m.uninstall_group(gi);
                }
            }
            return false;
        }
        if !load.installed {
            if let Some(handle) = &load.handle {
                if let Some(geom) = geoms.remove(handle) {
                    if let Some(m) = wmos.get_mut_untracked(model) {
                        m.install_group(gi, geom);
                    }
                    load.installed = true;
                    load.handle = None;
                } else if asset_server.load_state(handle).is_failed() {
                    // An unreadable group has nothing to install: it spawns empty.
                    load.installed = true;
                    load.handle = None;
                }
            }
        }
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A group's model box through a placement: the world box holds the moved corners, and an
    /// unread group's empty box never meets the far box.
    #[test]
    fn a_group_box_follows_the_placement_and_an_empty_one_never_meets() {
        // WoW +x is Bevy −z: a placement 300 yd along Bevy −z carries the box 300 yd along +x.
        let t = Transform::from_xyz(0.0, 0.0, -300.0);
        let (lo, hi) = world_box([0.0, 0.0, 0.0], [10.0, 10.0, 10.0], &t);
        assert!((lo[0] - 300.0).abs() < 0.01 && (hi[0] - 310.0).abs() < 0.01);
        assert!(meets_far_box(&(lo, hi), [300.0, 5.0, 5.0], 1.0));
        let empty = world_box([f32::INFINITY; 3], [f32::NEG_INFINITY; 3], &t);
        assert!(!meets_far_box(&empty, [0.0; 3], 1.0e6));
    }

    /// The loading bar waits for the groups in the far box only: a far group never holds it.
    #[test]
    fn only_the_groups_in_the_far_box_hold_the_loading_bar() {
        let nav = |x: f32| benilla_assets::WmoGroupNav {
            flags: 0,
            bbox_min: [x, 0.0, 0.0],
            bbox_max: [x + 10.0, 10.0, 10.0],
            ref_start: 0,
            ref_count: 0,
            area_table_id: 0,
            fog_indices: [0; 4],
            group_liquid: benilla_formats::NO_GROUP_LIQUID,
        };
        let model = WmoModel {
            group_nav: vec![nav(0.0), nav(1000.0)],
            ..Default::default()
        };
        let object = Arc::new(WorldObject {
            kind: crate::model_render::ModelKind::Wmo,
            label: String::new(),
            id: 1,
            detail: String::new(),
        });
        let mut groups = WmoGroups::new(&model, None, &Transform::IDENTITY, object, false);
        let at = [5.0, 5.0, 5.0];
        assert!(
            !groups.in_range_spawned(at, 177.0, &|_| true),
            "the near group is not up yet"
        );
        groups.state[0].spawned = true;
        assert!(
            groups.in_range_spawned(at, 177.0, &|_| true),
            "the far group does not hold it"
        );
        // A near group that has walking faces is up only once they have attached.
        groups.state[0].walk = Some(Entity::from_raw_u32(9).unwrap());
        assert!(
            !groups.in_range_spawned(at, 177.0, &|_| false),
            "a near group whose walking collider is still building holds it"
        );
        assert!(groups.in_range_spawned(at, 177.0, &|e| e == Entity::from_raw_u32(9).unwrap()));
    }
}
