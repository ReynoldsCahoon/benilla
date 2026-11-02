//! WMO asset loader: a root file plus its group files (`<stem>_NNN.wmo`), one [`ModelSubmesh`] per
//! group render batch. Groups are read with [`LoadContext::read_asset_bytes`], not `load()`, which
//! would recurse into this loader. The app builds the meshes, paced, as a city's thousands of
//! batches would land in one frame.

use crate::column_grid::ColumnGrid;
use std::sync::Arc;

use benilla_formats::{
    accumulate_wmo_group_camera_only_collision, accumulate_wmo_group_collision_flagged,
    parse_wmo_lights, parse_wmo_root, wmo_group_doodad_refs, wmo_group_footprint_tris,
    wmo_group_header, wmo_group_light_refs, wmo_group_liquid_mesh, wmo_group_submeshes,
    CollisionMesh, FootprintTris, LiquidMesh, WmoDoodad, WmoDoodadSet, WmoFog, WmoGroupInfo,
    WmoLight, WmoPortalInfo, WmoPortalRef,
};
use bevy::asset::io::Reader;
use bevy::asset::{Asset, AssetLoader, LoadContext};
use bevy::prelude::*;
use bevy::reflect::TypePath;

use crate::model::ModelSubmesh;

/// One WMO group's portal-cull data: MOGP flags (the flood defers on EXTERIOR `0x8`), the MOGI box
/// and its slice of [`WmoModel::portal_refs`]; an unreadable group never floods.
#[derive(Clone, Copy)]
pub struct WmoGroupNav {
    pub flags: u32,
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    pub ref_start: u16,
    pub ref_count: u16,
    /// The `WMOAreaTable.WMOGroupID` key, MOGP `uniqueID`; 0 when none.
    pub area_table_id: u32,
    /// MOGP fog indices (disk `+0x30`) into [`WmoModel::fogs`], for the fog selector (`0x69de20`).
    pub fog_indices: [u8; 4],
    /// MOGP `groupLiquid` (`0xf` = none): when set, the whole group reads submerged at every Z.
    pub group_liquid: u32,
}

/// One WMO group's faces, a slice of a shared indexed mesh. The walking set is a slice of
/// [`WmoModel::collision`] itself, so the per-group view holds no copy: it was a 36-byte-a-face
/// triangle list beside the same faces in the flat collider, ~45 MB for Stormwind loaded whole,
/// part of the landing spike that killed the iPad Air 2 (wow-optimize FINDINGS, "Teleports keep
/// the tables").
#[derive(Clone, Default, Debug)]
pub struct GroupTris {
    mesh: Arc<CollisionMesh>,
    /// The first of the group's indices in `mesh.indices` (three per face).
    start: u32,
    /// The group's face count.
    count: u32,
}

impl GroupTris {
    /// Consecutive groups over `mesh`: group `g` takes the next `faces[g]` faces.
    pub fn slices(mesh: &Arc<CollisionMesh>, faces: &[u32]) -> Vec<Self> {
        let mut start = 0;
        faces
            .iter()
            .map(|&count| {
                let g = Self {
                    mesh: mesh.clone(),
                    start,
                    count,
                };
                start += count * 3;
                g
            })
            .collect()
    }

    /// A group of its own from whole triangles (tests and the audit's synthetic buildings).
    pub fn from_tris(tris: impl IntoIterator<Item = [[f32; 3]; 3]>) -> Self {
        let mut mesh = CollisionMesh::default();
        for t in tris {
            let base = mesh.positions.len() as u32;
            mesh.positions.extend_from_slice(&t);
            mesh.indices.extend([base, base + 1, base + 2]);
        }
        let count = (mesh.indices.len() / 3) as u32;
        Self {
            mesh: Arc::new(mesh),
            start: 0,
            count,
        }
    }

    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// The index of the group's first face in the shared mesh, for per-face data kept beside it.
    pub fn first_face(&self) -> usize {
        self.start as usize / 3
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Face `i`'s corners. Panics past [`Self::len`], as indexing the old list did.
    pub fn tri(&self, i: usize) -> [[f32; 3]; 3] {
        assert!(i < self.len(), "face {i} of {}", self.len());
        let b = self.start as usize + 3 * i;
        let ix = &self.mesh.indices[b..b + 3];
        let p = &self.mesh.positions;
        [p[ix[0] as usize], p[ix[1] as usize], p[ix[2] as usize]]
    }

    pub fn get(&self, i: usize) -> Option<[[f32; 3]; 3]> {
        (i < self.len()).then(|| self.tri(i))
    }

    pub fn iter(&self) -> GroupTrisIter<'_> {
        self.into_iter()
    }

    /// The group's faces as a mesh of their own, holding only the vertices they use, renumbered
    /// in first-use order: what a per-group collider is built from.
    pub fn to_mesh(&self) -> CollisionMesh {
        let range = self.start as usize..self.start as usize + 3 * self.len();
        let mut remap: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        let mut out = CollisionMesh::default();
        out.indices.reserve_exact(range.len());
        for &i in &self.mesh.indices[range] {
            let next = out.positions.len() as u32;
            let local = *remap.entry(i).or_insert_with(|| {
                out.positions.push(self.mesh.positions[i as usize]);
                next
            });
            out.indices.push(local);
        }
        out.positions.shrink_to_fit();
        out
    }
}

/// [`GroupTris`]'s faces by value, in order.
#[derive(Clone)]
pub struct GroupTrisIter<'a> {
    group: &'a GroupTris,
    next: usize,
}

impl Iterator for GroupTrisIter<'_> {
    type Item = [[f32; 3]; 3];
    fn next(&mut self) -> Option<Self::Item> {
        let t = self.group.get(self.next)?;
        self.next += 1;
        Some(t)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.group.len() - self.next;
        (n, Some(n))
    }
}

impl ExactSizeIterator for GroupTrisIter<'_> {}

impl<'a> IntoIterator for &'a GroupTris {
    type Item = [[f32; 3]; 3];
    type IntoIter = GroupTrisIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        GroupTrisIter {
            group: self,
            next: 0,
        }
    }
}

impl From<Vec<[[f32; 3]; 3]>> for GroupTris {
    fn from(tris: Vec<[[f32; 3]; 3]>) -> Self {
        Self::from_tris(tris)
    }
}

impl FromIterator<[[f32; 3]; 3]> for GroupTris {
    fn from_iter<I: IntoIterator<Item = [[f32; 3]; 3]>>(tris: I) -> Self {
        Self::from_tris(tris)
    }
}

/// One WMO group's camera faces (every face but `NOCAMCOLLIDE`), held as the walking faces the
/// camera does not pass plus the camera-only faces, which together are exactly that set
/// ([`benilla_formats::accumulate_wmo_group_collision_flagged`]). A separate copy of a city's
/// camera faces, ~4× its walking faces, was part of the landing spike that killed the iPad Air 2
/// (wow-optimize FINDINGS, "The landing spike"); the camera collider builds from this on demand.
#[derive(Clone, Debug)]
pub struct CameraFaces {
    walk: GroupTris,
    /// Per walking face of the model's collision mesh: the camera passes it (`NOCAMCOLLIDE`).
    passes: Arc<[bool]>,
    only: GroupTris,
    /// `(min, max)` over the camera faces, WMO-local.
    pub bounds: ([f32; 3], [f32; 3]),
}

impl CameraFaces {
    /// `None` when the group has no camera face.
    pub fn new(walk: GroupTris, passes: Arc<[bool]>, only: GroupTris) -> Option<Self> {
        let mut faces = Self {
            walk,
            passes,
            only,
            bounds: ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]),
        };
        let mut any = false;
        let mut bounds = faces.bounds;
        for t in faces.iter() {
            any = true;
            for v in t {
                for (a, c) in v.into_iter().enumerate() {
                    bounds.0[a] = bounds.0[a].min(c);
                    bounds.1[a] = bounds.1[a].max(c);
                }
            }
        }
        faces.bounds = bounds;
        any.then_some(faces)
    }

    /// The camera faces: the walking faces the camera does not pass, then the camera-only faces.
    pub fn iter(&self) -> impl Iterator<Item = [[f32; 3]; 3]> + '_ {
        let first = self.walk.first_face();
        self.walk
            .iter()
            .enumerate()
            .filter(move |(i, _)| !self.passes.get(first + i).copied().unwrap_or(false))
            .map(|(_, t)| t)
            .chain(self.only.iter())
    }

    /// The camera faces as one mesh, three vertices a face, for the collider build.
    pub fn mesh(&self) -> CollisionMesh {
        let mut out = CollisionMesh::default();
        for t in self.iter() {
            let base = out.positions.len() as u32;
            out.positions.extend_from_slice(&t);
            out.indices.extend([base, base + 1, base + 2]);
        }
        out
    }
}

/// One WMO group's collision faces in WMO-local coords, with their AABB.
pub struct GroupCollision {
    pub mesh: CollisionMesh,
    /// `(min, max)` over [`CollisionMesh::positions`].
    pub bounds: ([f32; 3], [f32; 3]),
}

impl WmoModel {
    /// The range of [`Self::submeshes`] that group `group` draws: the loader appends each group's
    /// batches in group order, so [`Self::submesh_group`] ascends.
    pub fn group_submeshes(&self, group: u16) -> std::ops::Range<usize> {
        let start = self.submesh_group.partition_point(|&g| g < group);
        let end = self.submesh_group.partition_point(|&g| g <= group);
        start..end
    }
}

impl GroupCollision {
    /// `None` when the group has no faces.
    pub fn new(mesh: CollisionMesh) -> Option<Self> {
        if mesh.indices.len() < 3 {
            return None;
        }
        let mut lo = [f32::INFINITY; 3];
        let mut hi = [f32::NEG_INFINITY; 3];
        for p in &mesh.positions {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        Some(Self {
            mesh,
            bounds: (lo, hi),
        })
    }
}

/// A loaded WMO building; `Default` is an empty test scaffold, never a loader product.
#[derive(Asset, TypePath, Clone, Default)]
pub struct WmoModel {
    /// The `WMOAreaTable.WMOID` key (root `MOHD.wmoID`).
    pub wmo_id: u32,
    pub submeshes: Vec<ModelSubmesh>,
    /// The group of each entry in [`Self::submeshes`], so the portal cull can hide a whole group.
    pub submesh_group: Vec<u16>,
    /// The portal graph (MOPV, MOPT, MOPR) in model space; with none, every group stays visible.
    pub portal_vertices: Vec<[f32; 3]>,
    pub portal_infos: Vec<WmoPortalInfo>,
    pub portal_refs: Vec<WmoPortalRef>,
    /// Per-group cull data, indexed by absolute group index.
    pub group_nav: Vec<WmoGroupNav>,
    /// The root's MFOG records; record 0 is the building default.
    pub fogs: Vec<WmoFog>,
    /// The root's MOSB skybox model, drawn while the camera is in a group flagged `0x40000`.
    pub skybox: Option<String>,
    /// Per-group walking-collision faces (non-DETAIL, no orientation filter): the reference's
    /// walking BSP set (`0x6be250`, mask `0x84`) that the current-group down-ray casts on.
    pub group_collision_tris: Vec<GroupTris>,
    /// Per-group camera-only faces (DETAIL `0x04` set, NOCAMCOLLIDE `0x02` clear). Deviation: the
    /// down-ray tries them when the walking and portal legs miss and no terrain lies below the eye,
    /// because the reference reads outside there and blanks the building around a camera sealed in
    /// an all-DETAIL pocket.
    pub group_camera_only_tris: Vec<GroupTris>,
    /// Per-group AABB of [`Self::group_collision_tris`], from the faces rather than MOGI: an
    /// authored MOGI box can sit above its own floor (Northshire Abbey group 3, by 1.5 yd).
    pub group_collision_bounds: Vec<Option<([f32; 3], [f32; 3])>>,
    /// Per-group [`ColumnGrid`] over [`Self::group_collision_tris`]; `None` when too few to index.
    pub group_collision_grids: Vec<Option<ColumnGrid>>,
    /// The union of [`Self::group_collision_bounds`]: the whole-building broad phase.
    pub collision_bounds: Option<([f32; 3], [f32; 3])>,
    /// Walking collision across all groups in WMO-local coords, DETAIL faces excluded; shared with
    /// [`Self::group_collision_tris`], which slice it.
    pub collision: Option<Arc<CollisionMesh>>,
    /// Camera collision per group: DETAIL kept, NOCAMCOLLIDE dropped; `None` for a group without
    /// camera faces. Per group so a placement builds only the groups within the boom's reach
    /// (`terrain_stream::camera_collider`); held as views of the walking and camera-only faces.
    pub group_camera: Vec<Option<CameraFaces>>,
    /// The root's MODD doodads in WMO model space, spawned by doodad set.
    pub doodads: Vec<WmoDoodad>,
    /// The MODS ranges into [`Self::doodads`]: set 0 always, plus the one a placement picks.
    pub doodad_sets: Vec<WmoDoodadSet>,
    /// The root's MOLT fixture lights in WMO model space, spawned as point lights.
    pub lights: Vec<WmoLight>,
    /// Per-group MOGI interior flag and box, in WMO model space.
    pub group_bounds: Vec<WmoGroupInfo>,
    /// Per-MODD lighting base, parallel to [`Self::doodads`]. A MODD doodad never takes the
    /// footprint down-ray: its create (`0x694e90`) never calls `SetMatrix` (`0x698d20`).
    pub doodad_base: Vec<DoodadBase>,
    /// Per-MODD first referencing group, whose MOLR set the interior lane folds; the
    /// interior/exterior class reads [`Self::doodad_groups`].
    pub doodad_owner: Vec<Option<u16>>,
    /// Per-MODD every group whose MODR names it: the portal-cull key, as the reference admits a
    /// group's props each frame its portal walk visits it (`0x685d70` → `0x6838f0`).
    pub doodad_groups: Vec<Arc<[u16]>>,
    /// Per-group footprint faces (render faces, MOCV, MOPY flags); `None` for exterior groups and
    /// groups without MOCV. A GameObject M2 down-rays them for its MOCV lighting (`0x69e4c0`).
    pub group_footprints: Vec<Option<FootprintTris>>,
    /// Root MOMT `ground_type` per material: the `TerrainType.dbc` id of a face's MOPY material.
    pub material_ground_type: Vec<u32>,
    /// Root MOMT `diffColor` per material, RGB 0..1: an interior MLIQ pool's colour, by material.
    pub material_diff_color: Vec<[f32; 3]>,
    /// Per-group AABB of the faces [`Self::group_footprints`] uses: the footprint broad phase.
    pub group_footprint_bounds: Vec<Option<([f32; 3], [f32; 3])>>,
    /// Per-group [`ColumnGrid`] over [`Self::group_footprints`]; `None` when not indexed.
    pub group_footprint_grids: Vec<Option<ColumnGrid>>,
    /// Per-group MOLR refs into [`Self::lights`]: a MODD prop folds its owning group's list, a
    /// GameObject its footprint-hit group's.
    pub group_light_refs: Vec<Vec<u16>>,
    /// Per-group MLIQ liquid surface in WMO model space; `None` for a group without one.
    pub group_liquids: Vec<Option<LiquidMesh>>,
    /// The root's MCVP convex volume in WMO model space, which a transport's rider is held aboard
    /// while inside (`0x6a4990`, [`benilla_formats::point_in_convex_volume`]).
    pub convex_volume: Vec<[f32; 4]>,
}

/// One MODD doodad's lighting base. The reference fills an interior doodad's words at create from
/// the MODD colour: `0x694e90` calls `0x6a77e0`, diffuse floor `0x70`, ambient cap `0x60`.
#[derive(Clone, PartialEq, Debug)]
pub enum DoodadBase {
    /// The sky-lit lane of an exterior-group doodad.
    Exterior,
    /// An interior-group doodad: the MODD-colour base and the owning group's light refs.
    Interior(InteriorPropBase),
}

/// An interior MODD doodad's base light, independent of the time of day.
#[derive(Clone, PartialEq, Debug)]
pub struct InteriorPropBase {
    /// The ambient word, [`cap96`] of the MODD colour, RGB 0..1.
    pub ambient: [f32; 3],
    /// The diffuse word, [`floor112`] of the MODD colour, lit along the fixed axis
    /// (−0.30822, −0.30822, −0.9), never the sun.
    pub diffuse: [f32; 3],
    /// The owning group's MOLR refs into [`WmoModel::lights`], the only point lights it folds
    /// (range-gated by attenStart and attenEnd); empty means none, its own flame included.
    pub light_refs: Vec<u16>,
}

/// Per-group AABBs of the vertices the footprint faces index, not of every position; shared with
/// the test fixtures.
pub fn footprint_tri_bounds(
    footprints: &[Option<FootprintTris>],
) -> Vec<Option<([f32; 3], [f32; 3])>> {
    footprints
        .iter()
        .map(|fp| {
            let fp = fp.as_ref()?;
            let mut bounds: Option<([f32; 3], [f32; 3])> = None;
            for &i in &fp.indices {
                let Some(v) = fp.positions.get(i as usize) else {
                    continue;
                };
                let (min, max) = bounds.get_or_insert((*v, *v));
                for a in 0..3 {
                    min[a] = min[a].min(v[a]);
                    max[a] = max[a].max(v[a]);
                }
            }
            bounds
        })
        .collect()
}

/// Per-group column indexes over the footprint faces, shared with the test fixtures.
pub fn footprint_tri_grids(footprints: &[Option<FootprintTris>]) -> Vec<Option<ColumnGrid>> {
    footprints
        .iter()
        .map(|fp| {
            let fp = fp.as_ref()?;
            let tris: Vec<&[u16; 3]> = fp.indices.as_chunks::<3>().0.iter().collect();
            ColumnGrid::build(tris.len(), |i| {
                let mut lo = [f32::MAX; 2];
                let mut hi = [f32::MIN; 2];
                for &vi in tris[i] {
                    if let Some(v) = fp.positions.get(vi as usize) {
                        for a in 0..2 {
                            lo[a] = lo[a].min(v[a]);
                            hi[a] = hi[a].max(v[a]);
                        }
                    }
                }
                (lo, hi)
            })
        })
        .collect()
}

/// Drops a trailing partial face and any face with a corner past `positions`, keeping the rest in
/// order: what the per-group lists kept when they were built face by face.
fn keep_whole_faces(positions: &[[f32; 3]], indices: &mut Vec<u32>) {
    let n = positions.len() as u32;
    let mut kept = 0;
    for f in 0..indices.len() / 3 {
        let t = [indices[3 * f], indices[3 * f + 1], indices[3 * f + 2]];
        if t.iter().all(|&i| i < n) {
            indices[3 * kept..3 * kept + 3].copy_from_slice(&t);
            kept += 1;
        }
    }
    indices.truncate(3 * kept);
}

/// Per-group column indexes over a per-group triangle set.
pub fn collision_tri_grids(tris: &[GroupTris]) -> Vec<Option<ColumnGrid>> {
    tris.iter()
        .map(|group| {
            ColumnGrid::build(group.len(), |i| {
                let t = group.tri(i);
                (
                    [
                        t[0][0].min(t[1][0]).min(t[2][0]),
                        t[0][1].min(t[1][1]).min(t[2][1]),
                    ],
                    [
                        t[0][0].max(t[1][0]).max(t[2][0]),
                        t[0][1].max(t[1][1]).max(t[2][1]),
                    ],
                )
            })
        })
        .collect()
}

/// Per-group AABBs of a per-group triangle set, and their union; shared with the down-ray tests.
#[allow(clippy::type_complexity)]
pub fn collision_tri_bounds(
    tris: &[GroupTris],
) -> (
    Vec<Option<([f32; 3], [f32; 3])>>,
    Option<([f32; 3], [f32; 3])>,
) {
    let per_group: Vec<Option<([f32; 3], [f32; 3])>> = tris
        .iter()
        .map(|group| {
            let mut bounds: Option<([f32; 3], [f32; 3])> = None;
            for tri in group.iter() {
                for v in &tri {
                    let (min, max) = bounds.get_or_insert((*v, *v));
                    for a in 0..3 {
                        min[a] = min[a].min(v[a]);
                        max[a] = max[a].max(v[a]);
                    }
                }
            }
            bounds
        })
        .collect();
    let union = per_group.iter().flatten().fold(
        None::<([f32; 3], [f32; 3])>,
        |acc, (gmin, gmax)| match acc {
            None => Some((*gmin, *gmax)),
            Some((mut min, mut max)) => {
                for a in 0..3 {
                    min[a] = min[a].min(gmin[a]);
                    max[a] = max[a].max(gmax[a]);
                }
                Some((min, max))
            }
        },
    );
    (per_group, union)
}

/// The ambient cap of `0x6a77e0`, in its fixed point: a colour whose max channel exceeds 96 scales
/// per channel to `(c·scale + 255) >> 8`, `scale = round(96·255/max − 0.5)`; otherwise it is raw.
pub fn cap96(c: [u8; 3]) -> [f32; 3] {
    let max = c[0].max(c[1]).max(c[2]);
    if max <= 96 {
        return c.map(|v| f32::from(v) / 255.0);
    }
    let scale = ((96.0 * 255.0 / f32::from(max)) - 0.5).round_ties_even() as u32;
    c.map(|v| ((u32::from(v) * scale + 255) >> 8) as f32 / 255.0)
}

/// The diffuse floor of `0x6a77e0`: a colour whose max channel is below `thresh` is raised by the
/// reference's HSV round trip at value `thresh`, a scale by `thresh/max` truncated per channel.
fn floor_raise(c: [u8; 3], thresh: u8) -> [f32; 3] {
    let max = c[0].max(c[1]).max(c[2]);
    if max >= thresh || max == 0 {
        return c.map(|v| f32::from(v) / 255.0);
    }
    // Integer math: a float `v · (thresh/max)` can land under `thresh`, truncating to `thresh − 1`.
    c.map(|v| ((u32::from(v) * u32::from(thresh)) / u32::from(max)) as f32 / 255.0)
}

/// [`floor_raise`] at the MODD create site's diffuse threshold `0x70` (112).
pub fn floor112(c: [u8; 3]) -> [f32; 3] {
    floor_raise(c, 112)
}

/// [`floor_raise`] at the GameObject footprint attach site's diffuse threshold `0xA8` (168),
/// `0x69e4c0`, the entity twin of the ADT-MDDF attach at `0x6a8410`.
pub fn floor168(c: [u8; 3]) -> [f32; 3] {
    floor_raise(c, 168)
}

/// MODD index to its first referencing group, whose MOLR set the interior lane uses. The reference
/// creates a group's props once, when the group enters the streaming window (`0x698720` →
/// `0x695aa0`, MODR at `+0xe8`, count `+0x144`), and never one no group names.
fn modr_owners(doodad_count: usize, group_doodad_refs: &[Vec<u16>]) -> Vec<Option<u16>> {
    let mut owner: Vec<Option<u16>> = vec![None; doodad_count];
    for (gi, refs) in group_doodad_refs.iter().enumerate() {
        for &di in refs {
            if let Some(slot) = owner.get_mut(di as usize) {
                slot.get_or_insert(gi as u16);
            }
        }
    }
    owner
}

/// MODD index to every group whose MODR names it: the reference admits a group's props in each
/// frame its portal walk visits it (`0x685d70` → `0x6838f0`), so a prop shows while any is visible.
fn modr_refs(doodad_count: usize, group_doodad_refs: &[Vec<u16>]) -> Vec<Arc<[u16]>> {
    let mut refs: Vec<Vec<u16>> = vec![Vec::new(); doodad_count];
    for (gi, group) in group_doodad_refs.iter().enumerate() {
        for &di in group {
            if let Some(slot) = refs.get_mut(di as usize) {
                // MODR may name the same doodad twice in one group; the cull only needs the set.
                if slot.last() != Some(&(gi as u16)) {
                    slot.push(gi as u16);
                }
            }
        }
    }
    refs.into_iter().map(Arc::from).collect()
}

/// Every MODD doodad's lighting base, from MODR ownership, never a spatial test; an unnamed MODD
/// is Exterior. Exterior wins: the reference keeps one def per (MODD, placement) (`0x694e90`), and
/// its classify (`0x695aa0`) never re-marks an exterior def interior while an exterior group
/// clears the interior bit, so any exterior referrer makes the prop sky-lit.
fn resolve_doodad_bases(
    doodads: &[WmoDoodad],
    groups: &[WmoGroupInfo],
    owner: &[Option<u16>],
    refs: &[Arc<[u16]>],
    group_light_refs: &[Vec<u16>],
) -> Vec<DoodadBase> {
    let interior_group = |gi: &u16| -> bool {
        groups
            .get(*gi as usize)
            .is_some_and(|g: &WmoGroupInfo| g.interior)
    };
    doodads
        .iter()
        .enumerate()
        .map(|(di, d)| {
            let Some(gi) = owner.get(di).copied().flatten() else {
                return DoodadBase::Exterior;
            };
            // Exterior is absorbing across the whole referrer set.
            let all_interior = refs
                .get(di)
                .is_some_and(|gs| !gs.is_empty() && gs.iter().all(interior_group));
            if !all_interior {
                return DoodadBase::Exterior;
            }
            let rgb = [d.color[0], d.color[1], d.color[2]];
            DoodadBase::Interior(InteriorPropBase {
                ambient: cap96(rgb),
                diffuse: floor112(rgb),
                light_refs: group_light_refs
                    .get(gi as usize)
                    .cloned()
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Loads a WMO root and its group files into a [`WmoModel`].
#[derive(Default, TypePath)]
pub struct WmoModelLoader;

impl AssetLoader for WmoModelLoader {
    type Asset = WmoModel;
    type Settings = ();
    type Error = std::io::Error;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &(),
        ctx: &mut LoadContext<'_>,
    ) -> Result<WmoModel, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        let to_io = |e: anyhow::Error| std::io::Error::other(format!("{e:#}"));
        let root = parse_wmo_root(&bytes).map_err(to_io)?;

        // Lowercased so the `.wmo` strip matches; MPQ lookup is case-insensitive anyway.
        let root_path = ctx.path().path().to_string_lossy().to_ascii_lowercase();
        let stem = root_path
            .strip_suffix(".wmo")
            .unwrap_or(&root_path)
            .to_string();

        // Pre-sized to the group count so MOPR group indices line up when a group file is skipped;
        // boxes from the root MOGI, the rest from each MOGP header.
        let mut group_nav: Vec<WmoGroupNav> = (0..root.group_count() as usize)
            .map(|gi| {
                let (bbox_min, bbox_max) = root
                    .group_infos()
                    .get(gi)
                    .map_or(([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]), |g| {
                        (g.bbox_min, g.bbox_max)
                    });
                WmoGroupNav {
                    flags: 0,
                    bbox_min,
                    bbox_max,
                    ref_start: 0,
                    ref_count: 0,
                    area_table_id: 0,
                    fog_indices: [0; 4],
                    group_liquid: benilla_formats::NO_GROUP_LIQUID,
                }
            })
            .collect();
        let mut group_doodad_refs: Vec<Vec<u16>> = vec![Vec::new(); root.group_count() as usize];
        let mut group_light_refs: Vec<Vec<u16>> = vec![Vec::new(); root.group_count() as usize];
        let mut group_liquids: Vec<Option<LiquidMesh>> =
            (0..root.group_count()).map(|_| None).collect();
        for gi in 0..root.group_count() {
            let group_url = format!("mpq://{stem}_{gi:03}.wmo");
            let Ok(gbytes) = ctx.read_asset_bytes(group_url).await else {
                continue; // a missing or unreadable group is skipped
            };
            if let (Some(h), Some(nav)) =
                (wmo_group_header(&gbytes), group_nav.get_mut(gi as usize))
            {
                nav.flags = h.flags;
                nav.ref_start = h.portal_ref_start;
                nav.ref_count = h.portal_ref_count;
                nav.area_table_id = h.area_table_id;
                nav.fog_indices = h.fog_indices;
                nav.group_liquid = h.group_liquid;
            }
            // The reference walks MODR at `0x695aa0` and MOLR at `0x695c00`.
            if let Some(slot) = group_doodad_refs.get_mut(gi as usize) {
                *slot = wmo_group_doodad_refs(&gbytes);
            }
            if let Some(slot) = group_light_refs.get_mut(gi as usize) {
                *slot = wmo_group_light_refs(&gbytes);
            }
            if let Some(slot) = group_liquids.get_mut(gi as usize) {
                *slot = wmo_group_liquid_mesh(&gbytes);
            }
        }
        let doodad_owner = modr_owners(root.doodads().len(), &group_doodad_refs);
        let doodad_groups = modr_refs(root.doodads().len(), &group_doodad_refs);
        let doodad_base = resolve_doodad_bases(
            root.doodads(),
            root.group_infos(),
            &doodad_owner,
            &doodad_groups,
            &group_light_refs,
        );

        resolve_shared_liquid_cells(&mut group_liquids);
        let portals = root.portals();
        // A group's geometry loads with the group ([`WmoGroupGeom`], [`WmoModel::install_group`]):
        // until then the per-group entries are empty, as the reference's queries skip a group
        // that is not loaded (`0x6a4a00`).
        let n = root.group_count() as usize;
        Ok(WmoModel {
            wmo_id: benilla_formats::wmo_root_id(&bytes),
            submeshes: Vec::new(),
            submesh_group: Vec::new(),
            portal_vertices: portals.vertices.clone(),
            portal_infos: portals.infos.clone(),
            portal_refs: portals.refs.clone(),
            group_nav,
            fogs: root.fogs().to_vec(),
            skybox: root.skybox().map(str::to_owned),
            group_collision_tris: vec![GroupTris::default(); n],
            group_camera_only_tris: vec![GroupTris::default(); n],
            group_collision_bounds: vec![None; n],
            group_collision_grids: vec![None; n],
            collision_bounds: None,
            collision: None,
            group_camera: vec![None; n],
            doodads: root.doodads().to_vec(),
            doodad_sets: root.doodad_sets().to_vec(),
            lights: parse_wmo_lights(&bytes),
            group_bounds: root.group_infos().to_vec(),
            doodad_base,
            doodad_owner,
            doodad_groups,
            group_footprints: vec![None; n],
            material_ground_type: root.material_ground_types(),
            material_diff_color: root.material_diff_colors(),
            group_footprint_bounds: vec![None; n],
            group_footprint_grids: vec![None; n],
            group_light_refs,
            group_liquids,
            convex_volume: root.convex_volume().to_vec(),
        })
    }

    fn extensions(&self) -> &[&str] {
        &["wmo"]
    }
}

/// One WMO group's CPU data, loaded when the group is wanted and moved into its model
/// ([`WmoModel::install_group`]); the asset itself is dropped after the move. The reference reads
/// a group file when the group's box first meets the far box (`CMapObj::LoadGroup`, terrain/
/// mapupdate#map-object-update) and frees it on its timer. Every group of a city at once, as the
/// root loader held them, killed the iPad Air 2 landing in Stormwind (wow-optimize FINDINGS, "The
/// landing spike").
#[derive(Asset, TypePath, Default)]
pub struct WmoGroupGeom {
    pub submeshes: Vec<ModelSubmesh>,
    pub walk: GroupTris,
    pub camera_only: GroupTris,
    pub camera: Option<CameraFaces>,
    pub walk_bounds: Option<([f32; 3], [f32; 3])>,
    pub walk_grid: Option<ColumnGrid>,
    pub footprint: Option<FootprintTris>,
    pub footprint_bounds: Option<([f32; 3], [f32; 3])>,
    pub footprint_grid: Option<ColumnGrid>,
}

impl WmoGroupGeom {
    /// Builds the group's data from its file; `None` for an unreadable file, `ctx` taking the
    /// batches' texture loads.
    fn from_file(
        gbytes: &[u8],
        root: &benilla_formats::WmoRoot,
        ctx: &mut LoadContext<'_>,
    ) -> std::io::Result<Self> {
        let (mut wpos, mut widx, mut passes) = (Vec::new(), Vec::new(), Vec::new());
        accumulate_wmo_group_collision_flagged(gbytes, &mut wpos, &mut widx, &mut passes);
        keep_whole_faces(&wpos, &mut widx);
        passes.truncate(widx.len() / 3);
        let walk_faces = (widx.len() / 3) as u32;
        let walk = GroupTris::slices(
            &Arc::new(CollisionMesh {
                positions: wpos,
                indices: widx,
            }),
            &[walk_faces],
        )
        .remove(0);
        let (mut opos, mut oidx) = (Vec::new(), Vec::new());
        accumulate_wmo_group_camera_only_collision(gbytes, &mut opos, &mut oidx);
        keep_whole_faces(&opos, &mut oidx);
        let only_faces = (oidx.len() / 3) as u32;
        let camera_only = GroupTris::slices(
            &Arc::new(CollisionMesh {
                positions: opos,
                indices: oidx,
            }),
            &[only_faces],
        )
        .remove(0);
        let camera = CameraFaces::new(walk.clone(), passes.into(), camera_only.clone());
        let walk_set = [walk];
        let (walk_bounds, _) = collision_tri_bounds(&walk_set);
        let walk_grid = collision_tri_grids(&walk_set).remove(0);
        let [walk] = walk_set;
        let footprint = [wmo_group_footprint_tris(gbytes)];
        let footprint_bounds = footprint_tri_bounds(&footprint).remove(0);
        let footprint_grid = footprint_tri_grids(&footprint).remove(0);
        let [footprint] = footprint;
        Ok(Self {
            submeshes: group_model_submeshes(gbytes, root, ctx)?,
            walk,
            camera_only,
            camera,
            walk_bounds: walk_bounds.into_iter().next().flatten(),
            walk_grid,
            footprint,
            footprint_bounds,
            footprint_grid,
        })
    }
}

/// Loads one WMO group file (`<root stem>_NNN.wmo`) as a [`WmoGroupGeom`]; picked over
/// [`WmoModelLoader`] by the requested asset type.
#[derive(Default, TypePath)]
pub struct WmoGroupLoader;

impl AssetLoader for WmoGroupLoader {
    type Asset = WmoGroupGeom;
    type Settings = ();
    type Error = std::io::Error;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &(),
        ctx: &mut LoadContext<'_>,
    ) -> Result<WmoGroupGeom, Self::Error> {
        let mut gbytes = Vec::new();
        reader.read_to_end(&mut gbytes).await?;
        // The root holds the materials the batches read: `<stem>_NNN.wmo` names `<stem>.wmo`.
        let path = ctx.path().path().to_string_lossy().to_ascii_lowercase();
        let stem = path
            .strip_suffix(".wmo")
            .and_then(|p| p.rsplit_once('_'))
            .map(|(stem, _)| stem.to_string())
            .ok_or_else(|| std::io::Error::other(format!("not a group file: {path}")))?;
        let root_bytes = ctx
            .read_asset_bytes(format!("mpq://{stem}.wmo"))
            .await
            .map_err(std::io::Error::other)?;
        let root =
            parse_wmo_root(&root_bytes).map_err(|e| std::io::Error::other(format!("{e:#}")))?;
        WmoGroupGeom::from_file(&gbytes, &root, ctx)
    }

    fn extensions(&self) -> &[&str] {
        &["wmo"]
    }
}

impl WmoModel {
    /// Moves a loaded group's data into the model: its batches into [`Self::submeshes`] at the
    /// group's place in group order, its faces, grids and footprint into the per-group entries.
    pub fn install_group(&mut self, group: u16, geom: WmoGroupGeom) {
        let g = group as usize;
        if g >= self.group_nav.len() {
            return;
        }
        self.uninstall_group(group);
        let at = self.submesh_group.partition_point(|&x| x < group);
        let n = geom.submeshes.len();
        self.submeshes.splice(at..at, geom.submeshes);
        self.submesh_group
            .splice(at..at, std::iter::repeat_n(group, n));
        self.group_collision_tris[g] = geom.walk;
        self.group_camera_only_tris[g] = geom.camera_only;
        self.group_camera[g] = geom.camera;
        self.group_collision_bounds[g] = geom.walk_bounds;
        self.group_collision_grids[g] = geom.walk_grid;
        self.group_footprints[g] = geom.footprint;
        self.group_footprint_bounds[g] = geom.footprint_bounds;
        self.group_footprint_grids[g] = geom.footprint_grid;
        self.refresh_collision_bounds();
    }

    /// Empties a group's entries again: its batches leave [`Self::submeshes`].
    pub fn uninstall_group(&mut self, group: u16) {
        let g = group as usize;
        if g >= self.group_nav.len() {
            return;
        }
        let range = self.group_submeshes(group);
        self.submeshes.drain(range.clone());
        self.submesh_group.drain(range);
        self.group_collision_tris[g] = GroupTris::default();
        self.group_camera_only_tris[g] = GroupTris::default();
        self.group_camera[g] = None;
        self.group_collision_bounds[g] = None;
        self.group_collision_grids[g] = None;
        self.group_footprints[g] = None;
        self.group_footprint_bounds[g] = None;
        self.group_footprint_grids[g] = None;
        self.refresh_collision_bounds();
    }

    /// Whether a group's data is installed.
    pub fn group_installed(&self, group: u16) -> bool {
        self.group_collision_bounds
            .get(group as usize)
            .is_some_and(Option::is_some)
            || !self.group_submeshes(group).is_empty()
            || self
                .group_footprints
                .get(group as usize)
                .is_some_and(Option::is_some)
    }

    fn refresh_collision_bounds(&mut self) {
        self.collision_bounds = self.group_collision_bounds.iter().flatten().fold(
            None::<([f32; 3], [f32; 3])>,
            |acc, (lo, hi)| match acc {
                None => Some((*lo, *hi)),
                Some((mut a, mut b)) => {
                    for k in 0..3 {
                        a[k] = a[k].min(lo[k]);
                        b[k] = b[k].max(hi[k]);
                    }
                    Some((a, b))
                }
            },
        );
    }
}

/// One group file's render batches as model submeshes, their textures requested through `ctx`.
fn group_model_submeshes(
    gbytes: &[u8],
    root: &benilla_formats::WmoRoot,
    ctx: &mut LoadContext<'_>,
) -> std::io::Result<Vec<ModelSubmesh>> {
    let subs =
        wmo_group_submeshes(gbytes, root).map_err(|e| std::io::Error::other(format!("{e:#}")))?;
    let mut out = Vec::with_capacity(subs.len());
    for sub in subs {
        // Lowercased: Bevy's loader lookup is case-sensitive, and an uppercase `.BLP` falls
        // to type-based resolution, ambiguous with Bevy's own image loader.
        let texture = sub.texture.as_deref().map(|t| {
            ctx.load::<Image>(format!(
                "mpq://{}",
                t.replace('\\', "/").to_ascii_lowercase()
            ))
        });
        out.push(ModelSubmesh {
            texture,
            skin_slot: sub.skin_slot,
            geoset_id: 0,
            char_slot: None,
            icon_slot: false,
            blend: sub.blend,
            two_sided: sub.two_sided,
            interior: sub.interior,
            emissive: sub.emissive,
            sidn: sub.sidn,     // MOMT SIDN (0x10): the authored night-glow colour
            window: sub.window, // MOMT WINDOW (0x20): the interior midpoint light
            additive: sub.additive,
            env_map: false,
            stage1: None, // a WMO batch is one texture stage
            combine: None,
            no_depth_write: false,
            no_depth_test: false,
            fog_policy: sub.fog_policy,
            billboard: None,
            alpha_anim: None,
            uv_anim: None,
            uv_seq: None,
            uv_rot_seq: None,
            uv_scale_seq: None,
            rgb_anim: None,
            rgb_seq: None,
            wmo_batch: sub.wmo_batch, // the MOBA section: an interior group's lighting
            ground_quad: None,
            geometry: std::sync::Arc::new(sub),
        });
    }
    Ok(out)
}

/// The MLIQ shared-cell gate: a cell flagged `0x80` ([`LiquidMesh::shared`]) is authored in two
/// groups and only one may draw it, or the sheet composites twice. The reference picks the drawer
/// by the portal flood's depth parity each frame (`0x6b41c0`, `0x6b4074`, `0x6b61a0`); the lowest
/// group index draws the same pixels, as both claimants carry the same alpha and heights. Cells
/// match on their XY centre, quantised to 0.01 yd only to hash.
fn resolve_shared_liquid_cells(group_liquids: &mut [Option<LiquidMesh>]) {
    let key = |m: &LiquidMesh, c: usize| -> Option<(i32, i32)> {
        let (cols, rows) = (m.grid[0] as usize, m.grid[1] as usize);
        let (xt, yt) = (cols.checked_sub(1)?, rows.checked_sub(1)?);
        let (tx, ty) = (c % xt, c / xt);
        if ty >= yt {
            return None;
        }
        let a = m.positions.get(ty * cols + tx)?;
        let b = m.positions.get((ty + 1) * cols + tx + 1)?;
        #[allow(clippy::cast_possible_truncation)]
        Some((
            (0.5 * (a[0] + b[0]) * 100.0).round() as i32,
            (0.5 * (a[1] + b[1]) * 100.0).round() as i32,
        ))
    };
    // First claimant wins, walking groups in index order.
    let mut owner: bevy::platform::collections::HashMap<(i32, i32), usize> =
        bevy::platform::collections::HashMap::default();
    for (gi, slot) in group_liquids.iter().enumerate() {
        let Some(m) = slot else { continue };
        for c in 0..m.wet.len() {
            if !m.wet[c] || !m.shared[c] {
                continue;
            }
            if let Some(k) = key(m, c) {
                owner.entry(k).or_insert(gi);
            }
        }
    }
    if owner.is_empty() {
        return;
    }
    for (gi, slot) in group_liquids.iter_mut().enumerate() {
        let Some(m) = slot else { continue };
        let keys: Vec<Option<(i32, i32)>> = (0..m.wet.len()).map(|c| key(m, c)).collect();
        let shared = m.shared.clone();
        m.retain_cells(|c| !shared[c] || keys[c].is_none_or(|k| owner.get(&k) == Some(&gi)));
        if m.indices.is_empty() {
            *slot = None; // every cell this group held belonged to a neighbour
        }
    }
}

#[cfg(test)]
mod doodad_base_tests {
    use super::*;

    fn grp(interior: bool) -> WmoGroupInfo {
        WmoGroupInfo {
            interior,
            show_skybox: false,
            bbox_min: [-1.0; 3],
            bbox_max: [1.0; 3],
        }
    }

    fn prop(color: [u8; 4]) -> WmoDoodad {
        WmoDoodad {
            model: "candle.m2".into(),
            position: [0.0; 3],
            orientation: [0.0, 0.0, 0.0, 1.0],
            scale: 1.0,
            color,
        }
    }

    /// Northshire Abbey's candle stands MODD[18] and MODD[24], as the reference commits them
    /// through `0x6a77e0` (cap scales 182 and 173).
    #[test]
    fn cap96_matches_the_decoded_abbey_stands_bit_exact() {
        let as_bytes = |c: [f32; 3]| c.map(|v| (v * 255.0).round() as u8);
        assert_eq!(as_bytes(cap96([78, 76, 134])), [56, 55, 96]);
        assert_eq!(as_bytes(cap96([90, 86, 141])), [61, 59, 96]);
        assert_eq!(as_bytes(cap96([96, 40, 20])), [96, 40, 20]);
        assert_eq!(as_bytes(floor112([78, 76, 134])), [78, 76, 134]);
        assert_eq!(as_bytes(floor112([90, 86, 141])), [90, 86, 141]);
        let raised = as_bytes(floor112([56, 28, 14]));
        assert_eq!(raised[0], 112);
        assert_eq!(raised, [112, 56, 28]);
        // Black stays black in the reference too: it scales the HSV value by `thresh/max`
        // (`0x6a78a5`) and forces max to 1 only to keep that divide finite (`0x6a780e`).
        assert_eq!(as_bytes(floor112([0, 0, 0])), [0, 0, 0]);
    }

    #[test]
    fn floor168_matches_the_decoded_abbey_benches_bit_exact() {
        // The abbey inn benches as the reference commits them; 63·168/83 = 127.52 truncates to 127.
        let as_bytes = |c: [f32; 3]| c.map(|v| (v * 255.0).round() as u8);
        assert_eq!(as_bytes(cap96([59, 65, 92])), [59, 65, 92]);
        assert_eq!(as_bytes(cap96([69, 63, 83])), [69, 63, 83]);
        assert_eq!(as_bytes(floor168([59, 65, 92])), [107, 118, 168]);
        assert_eq!(as_bytes(floor168([69, 63, 83])), [139, 127, 168]);
        assert_eq!(as_bytes(floor168([200, 30, 10])), [200, 30, 10]);
    }

    #[test]
    fn modr_ownership_picks_the_lane_and_the_light_refs() {
        let doodads = vec![
            prop([90, 86, 141, 255]), // owned by interior group 0
            prop([90, 86, 141, 255]), // owned by exterior group 1
            prop([90, 86, 141, 255]), // referenced by no group
        ];
        let groups = [grp(true), grp(false)];
        let modr = vec![vec![0u16], vec![1u16]];
        let molr = vec![vec![7u16, 9u16], vec![3u16]];
        let owner = modr_owners(doodads.len(), &modr);
        let refs = modr_refs(doodads.len(), &modr);
        assert_eq!(owner, vec![Some(0), Some(1), None]);
        let bases = resolve_doodad_bases(&doodads, &groups, &owner, &refs, &molr);
        match &bases[0] {
            DoodadBase::Interior(b) => {
                assert_eq!(b.light_refs, vec![7, 9]);
                assert_eq!(b.diffuse.map(|v| (v * 255.0).round() as u8), [90, 86, 141]);
                assert_eq!(b.ambient.map(|v| (v * 255.0).round() as u8), [61, 59, 96]);
            }
            other => panic!("interior-group doodad must take the MODD-colour base, got {other:?}"),
        }
        assert_eq!(bases[1], DoodadBase::Exterior);
        assert_eq!(bases[2], DoodadBase::Exterior);
    }

    #[test]
    fn one_exterior_referrer_makes_the_whole_prop_exterior() {
        let doodads = vec![
            prop([0, 0, 0, 255]), // g0 (interior) names it first, g1 (exterior) also names it
            prop([0, 0, 0, 255]), // interior groups only
        ];
        let groups = [grp(true), grp(false), grp(true)];
        // g0 names both props, g1 (exterior) names only prop 0, g2 (interior) names only prop 1.
        let modr = vec![vec![0u16, 1u16], vec![0u16], vec![1u16]];
        let molr = vec![vec![7u16], Vec::new(), vec![4u16]];
        let owner = modr_owners(doodads.len(), &modr);
        let refs = modr_refs(doodads.len(), &modr);
        assert_eq!(
            owner,
            vec![Some(0), Some(0)],
            "g0 is the first referrer of both"
        );
        let bases = resolve_doodad_bases(&doodads, &groups, &owner, &refs, &molr);
        assert_eq!(
            bases[0],
            DoodadBase::Exterior,
            "the exterior referrer wins even though an interior group names it first"
        );
        match &bases[1] {
            DoodadBase::Interior(b) => {
                assert_eq!(b.ambient, [0.0; 3]);
                assert_eq!(b.diffuse, [0.0; 3]);
                assert_eq!(b.light_refs, vec![7]);
            }
            other => panic!("interior-only prop must keep the MODD-colour lane, got {other:?}"),
        }
    }

    /// Booty Bay's entrance arch, `BootyBay.wmo` MODD[3], named first by interior g22 and also by
    /// exterior g42, with a black MODD colour.
    #[test]
    fn booty_bays_entrance_arch_is_sky_lit_not_a_black_silhouette() {
        let data = benilla_formats::wow_data_or_skip!();
        let stem = "World\\wmo\\Azeroth\\Buildings\\Stranglethorn_BootyBay\\BootyBay";
        let chain = benilla_formats::Chain::open(&data).expect("open vanilla patch chain");
        let bytes = chain
            .read(&format!("{stem}.wmo"))
            .expect("read BootyBay.wmo");
        let root = parse_wmo_root(&bytes).expect("parse BootyBay.wmo");
        let refs: Vec<Vec<u16>> = (0..root.group_count())
            .map(|gi| {
                chain
                    .read(&format!("{stem}_{gi:03}.wmo"))
                    .map(|g| wmo_group_doodad_refs(&g))
                    .unwrap_or_default()
            })
            .collect();

        let arch = 3usize;
        assert!(
            root.doodads()[arch]
                .model
                .to_ascii_lowercase()
                .contains("bootybayentrance_02"),
            "MODD[3] should be the entrance arch, got {}",
            root.doodads()[arch].model
        );
        assert_eq!(
            root.doodads()[arch].color,
            [0, 0, 0, 255],
            "the arch's baked MODD colour is unlit black — that is what made the lane load-bearing"
        );
        let owner = modr_owners(root.doodads().len(), &refs);
        let groups = modr_refs(root.doodads().len(), &refs);
        assert_eq!(
            owner[arch],
            Some(22),
            "an INTERIOR group still names the arch first"
        );
        assert!(
            groups[arch]
                .iter()
                .any(|&g| !root.group_infos()[g as usize].interior),
            "…and an EXTERIOR group also names it — the case exterior-wins decides"
        );
        let bases = resolve_doodad_bases(
            root.doodads(),
            root.group_infos(),
            &owner,
            &groups,
            &vec![Vec::new(); root.group_count() as usize],
        );
        assert_eq!(
            bases[arch],
            DoodadBase::Exterior,
            "the entrance arch must take the sky-lit lane, not the all-zero MODD-colour base"
        );
    }

    /// Northshire Abbey's group 3 has no MOLR, and its candle stand takes no point light in the
    /// reference, its own flame included.
    #[test]
    fn no_molr_means_no_point_lights_at_all() {
        let bases = resolve_doodad_bases(
            &[prop([120, 110, 150, 255])],
            &[grp(true)],
            &[Some(0)],
            &[Arc::from(vec![0u16])],
            &[Vec::new()],
        );
        match &bases[0] {
            DoodadBase::Interior(b) => assert!(b.light_refs.is_empty()),
            other => panic!("expected interior base, got {other:?}"),
        }
    }

    #[test]
    fn a_shared_modd_takes_its_first_referencing_groups_molr() {
        // g0 names doodad 2; g1 names 0 and 2; g2 names 1, plus a ref past the end of the list.
        let modr = vec![vec![2u16], vec![0u16, 2u16], vec![1u16, 99u16]];
        assert_eq!(
            modr_owners(3, &modr),
            vec![Some(1), Some(2), Some(0)],
            "doodad 2 is named by g0 and g1 — g0 wins"
        );
        assert_eq!(modr_owners(2, &[]), vec![None, None]);
    }

    #[test]
    fn the_cull_key_is_every_referencing_group() {
        // The fixture above: g0 names 2; g1 names 0 and 2; g2 names 1 and an out-of-range ref.
        let modr = vec![vec![2u16], vec![0u16, 2u16], vec![1u16, 99u16]];
        let refs = modr_refs(3, &modr);
        assert_eq!(&*refs[0], &[1], "doodad 0 is named by g1 alone");
        assert_eq!(&*refs[1], &[2]);
        assert_eq!(
            &*refs[2],
            &[0, 1],
            "doodad 2 hangs in two rooms — either one draws it"
        );
        // A doodad named twice by one group counts once.
        assert_eq!(&*modr_refs(1, &[vec![0u16, 0u16]])[0], &[0]);
        assert!(modr_refs(2, &[])[0].is_empty());
    }
}

#[cfg(test)]
mod group_tris_tests {
    use super::*;

    /// Two groups' faces laid end to end in one mesh read back per group, corner for corner, as
    /// the per-group lists held them; a whole-triangle group reads its own triangles.
    #[test]
    fn group_slices_read_each_groups_faces_from_the_shared_mesh() {
        let p = |x: f32| [x, 0.0, 0.0];
        let mesh = Arc::new(CollisionMesh {
            positions: vec![p(0.0), p(1.0), p(2.0), p(3.0), p(10.0), p(11.0), p(12.0)],
            // group 0: faces (0,1,2), (1,2,3); group 1: none; group 2: (4,5,6)
            indices: vec![0, 1, 2, 1, 2, 3, 4, 5, 6],
        });
        let groups = GroupTris::slices(&mesh, &[2, 0, 1]);
        assert_eq!(
            groups.iter().map(GroupTris::len).collect::<Vec<_>>(),
            [2, 0, 1]
        );
        assert_eq!(groups[0].tri(1), [p(1.0), p(2.0), p(3.0)]);
        assert_eq!(groups[2].tri(0), [p(10.0), p(11.0), p(12.0)]);
        assert_eq!(groups[0].get(2), None);
        assert!(groups[1].iter().next().is_none());
        assert_eq!(
            (&groups[0]).into_iter().collect::<Vec<_>>(),
            [[p(0.0), p(1.0), p(2.0)], [p(1.0), p(2.0), p(3.0)]]
        );
        let alone = groups[0].to_mesh();
        assert_eq!(alone.positions, [p(0.0), p(1.0), p(2.0), p(3.0)]);
        assert_eq!(alone.indices, [0, 1, 2, 1, 2, 3]);
        let last = groups[2].to_mesh();
        assert_eq!(
            (last.positions.len(), last.indices.clone()),
            (3, vec![0, 1, 2])
        );
        let own = GroupTris::from(vec![[p(5.0), p(6.0), p(7.0)]]);
        assert_eq!((own.len(), own.tri(0)), (1, [p(5.0), p(6.0), p(7.0)]));
    }

    /// A group's camera faces are its walking faces whose bit says the camera stops, found by the
    /// group's own offset in the shared mesh, then its camera-only faces; the box covers them.
    #[test]
    fn camera_faces_skip_the_walking_faces_the_camera_passes() {
        let p = |x: f32| [x, 0.0, 0.0];
        let mesh = Arc::new(CollisionMesh {
            positions: (0..9).map(|i| p(i as f32)).collect(),
            indices: vec![0, 1, 2, 3, 4, 5, 6, 7, 8],
        });
        // group 0: face 0; group 1: faces 1 and 2, the camera passing face 1.
        let walk = GroupTris::slices(&mesh, &[1, 2]);
        let passes: Arc<[bool]> = vec![true, true, false].into();
        let only = GroupTris::from(vec![[p(20.0), p(21.0), p(22.0)]]);
        assert!(
            CameraFaces::new(walk[0].clone(), passes.clone(), GroupTris::default()).is_none(),
            "group 0's one face is passed: no camera face"
        );
        let faces = CameraFaces::new(walk[1].clone(), passes, only).unwrap();
        assert_eq!(
            faces.iter().collect::<Vec<_>>(),
            [[p(6.0), p(7.0), p(8.0)], [p(20.0), p(21.0), p(22.0)]]
        );
        assert_eq!((faces.bounds.0[0], faces.bounds.1[0]), (6.0, 22.0));
        assert_eq!(faces.mesh().indices.len(), 6);
    }

    /// Installing a group fills its entries and the model's box; uninstalling one empties only its
    /// own (batch order across groups: `tests::loads_a_wmo_model_through_the_pipeline`).
    #[test]
    fn groups_install_and_uninstall_alone() {
        let nav = WmoGroupNav {
            flags: 0,
            bbox_min: [0.0; 3],
            bbox_max: [1.0; 3],
            ref_start: 0,
            ref_count: 0,
            area_table_id: 0,
            fog_indices: [0; 4],
            group_liquid: benilla_formats::NO_GROUP_LIQUID,
        };
        let n = 3;
        let mut m = WmoModel {
            group_nav: vec![nav; n],
            group_collision_tris: vec![GroupTris::default(); n],
            group_camera_only_tris: vec![GroupTris::default(); n],
            group_camera: vec![None; n],
            group_collision_bounds: vec![None; n],
            group_collision_grids: vec![None; n],
            group_footprints: vec![None; n],
            group_footprint_bounds: vec![None; n],
            group_footprint_grids: vec![None; n],
            ..Default::default()
        };
        let batches = |_: usize| WmoGroupGeom {
            walk: GroupTris::from(vec![[[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]]),
            walk_bounds: Some(([0.0; 3], [1.0, 1.0, 0.0])),
            ..Default::default()
        };
        m.install_group(2, batches(1));
        m.install_group(0, batches(2));
        assert!(m.group_installed(0) && !m.group_installed(1) && m.group_installed(2));
        m.uninstall_group(0);
        assert!(!m.group_installed(0) && m.group_installed(2));
        assert!(m.group_collision_tris[0].is_empty() && m.group_collision_bounds[0].is_none());
        assert!(
            m.collision_bounds.is_some(),
            "group 2 still bounds the model"
        );
    }

    /// A face with a corner past the vertices, and a trailing partial face, drop; the rest keep
    /// their order: what the per-group lists kept when built face by face.
    #[test]
    fn only_whole_faces_are_kept() {
        let positions = [[0.0; 3]; 4];
        let mut indices = vec![0, 1, 2, 0, 9, 3, 1, 2, 3, 2];
        keep_whole_faces(&positions, &mut indices);
        assert_eq!(indices, [0, 1, 2, 1, 2, 3]);
    }
}
