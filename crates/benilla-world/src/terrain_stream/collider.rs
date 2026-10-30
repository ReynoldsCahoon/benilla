//! Static colliders for streamed geometry: parry's trimesh build runs on the async compute pool
//! ([`build_collider_task`]) and [`finish_colliders`] attaches the results under a per-frame
//! budget, since a streamed burst finishes together and each attach is main-thread work.

use std::time::{Duration, Instant};

use avian3d::prelude::{Collider, CollisionLayers, RigidBody};
use benilla_assets::coords::wow_to_bevy;
use benilla_formats::CollisionMesh;
use bevy::ecs::system::SystemState;
use bevy::prelude::*;
use bevy::tasks::futures_lite::future;
use bevy::tasks::{block_on, AsyncComputeTaskPool, Task};

/// Wall clock per frame for attaching finished colliders (one costs ~0.004 ms plus ~1.8e-5 ms per
/// triangle); the rest wait, and the streamer loads far enough ahead that none is reachable yet.
const ATTACH_BUDGET: Duration = Duration::from_millis(2);

/// A test's override of [`ATTACH_BUDGET`]; `Duration::ZERO` attaches exactly one per call.
#[derive(Resource)]
pub(super) struct AttachBudget(pub(super) Duration);

/// A static collider building on the async compute pool, which [`finish_colliders`] attaches to
/// this entity.
#[derive(Component)]
pub struct PendingCollider {
    /// The in-flight parry build; taken the frame it completes.
    task: Option<Task<Collider>>,
    /// The finished shape, waiting out a deferred attach: a completed [`Task`] cannot be polled
    /// twice.
    built: Option<Collider>,
    /// WMO walk / camera collision layer; `None` = the default layer (terrain, doodads, props).
    layers: Option<CollisionLayers>,
    /// Insert `RigidBody::Static` too. `false` for a transport deck prop, whose bare collider
    /// rides the boat's kinematic body; a static one would stay at its spawn pose.
    static_body: bool,
}

impl PendingCollider {
    /// A collider build in flight on the async compute pool.
    pub fn new(task: Task<Collider>, layers: Option<CollisionLayers>, static_body: bool) -> Self {
        Self {
            task: Some(task),
            built: None,
            layers,
            static_body,
        }
    }

    /// An already-built collider, so a test stages a burst without a thread pool.
    #[cfg(test)]
    pub(super) fn ready(
        collider: Collider,
        layers: Option<CollisionLayers>,
        static_body: bool,
    ) -> Self {
        Self {
            task: None,
            built: Some(collider),
            layers,
            static_body,
        }
    }
}

/// [`finish_colliders`]'s query state.
pub(super) type PendingColliders =
    SystemState<Query<'static, 'static, (Entity, &'static mut PendingCollider)>>;

/// Attaches off-thread-built colliders, at most [`ATTACH_BUDGET`] worth per frame. Exclusive, so
/// the deadline can stop mid-burst where deferred commands would all apply in one block.
pub(super) fn finish_colliders(world: &mut World, state: &mut PendingColliders) {
    // Pass 1: poll every in-flight build; a completed task hands its shape to `built`. The ready
    // list is capped far above what a frame can attach (a cold zone load queues tens of
    // thousands); the rest stay queued for the next frame.
    const READY_SCAN_CAP: usize = 2048;
    let t0 = Instant::now();
    let mut ready: Vec<Entity> = Vec::new();
    let mut pending = 0usize;
    for (entity, mut pc) in state.get_mut(world).iter_mut() {
        pending += 1;
        if let Some(task) = pc.task.as_mut() {
            if let Some(collider) = block_on(future::poll_once(task)) {
                pc.task = None;
                pc.built = Some(collider);
            }
        }
        if pc.built.is_some() && ready.len() < READY_SCAN_CAP {
            ready.push(entity);
        }
    }
    // The queue depth, unflushed welds included: the loading screen and the settle release wait
    // on it, so no body is released onto geometry without its collider. An overcount only delays.
    let weld_backlog = world
        .get_resource::<super::weld::HullWelds>()
        .map_or(0, |w| w.unflushed());
    if let Some(mut progress) = world.get_resource_mut::<crate::terrain_stream::WorldLoadProgress>()
    {
        progress.colliders_pending = pending + weld_backlog;
    }
    if ready.is_empty() {
        // `get_`: the unit tests run without the perf layer.
        if let Some(mut activity) =
            world.get_resource_mut::<crate::terrain_stream::StreamActivity>()
        {
            activity.collider_ms += t0.elapsed().as_secs_f32() * 1000.0;
        }
        return;
    }

    // Pass 2: attach. The deadline is checked after each attach, so an oversized one still lands.
    let mut attached = 0u32;
    let budget = world
        .get_resource::<AttachBudget>()
        .map_or(ATTACH_BUDGET, |b| b.0);
    let deadline = Instant::now() + budget;
    for entity in ready {
        // The entity may have streamed out while its collider was building.
        let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
            continue;
        };
        let Some(mut pc) = entity_mut.take::<PendingCollider>() else {
            continue;
        };
        let Some(collider) = pc.built.take() else {
            continue;
        };
        entity_mut.insert(collider);
        if pc.static_body {
            entity_mut.insert(RigidBody::Static);
        }
        if let Some(layers) = pc.layers {
            entity_mut.insert(layers);
        }
        attached += 1;
        if Instant::now() >= deadline {
            break;
        }
    }
    if let Some(mut activity) = world.get_resource_mut::<crate::terrain_stream::StreamActivity>() {
        activity.colliders_attached += attached;
        activity.collider_ms += t0.elapsed().as_secs_f32() * 1000.0;
    }
    // Invalidates cached collision answers (`crate::collision::ColliderEpoch`).
    if attached > 0 {
        if let Some(mut epoch) = world.get_resource_mut::<crate::collision::ColliderEpoch>() {
            epoch.bump();
        }
    }
}

/// Attaches the held inner tiles' terrain and fence colliders this frame, waiting on any build
/// still in flight and past [`ATTACH_BUDGET`]: the reference builds an inner chunk with no limit
/// (`0x6986a5` → `0x6c2880`), so the next frame's mover never meets ground without collision.
/// After the spawn, whose builds it may wait on; its inserts apply within the frame, and avian's
/// insert observers size each AABB and file it in the collider tree on the spot, so the next
/// frame's casts meet it with no physics step between.
pub(super) fn attach_inner_colliders(
    mut commands: Commands,
    state: Res<super::TerrainStreamer>,
    mut pending: Query<&mut PendingCollider>,
    mut activity: Option<ResMut<crate::terrain_stream::StreamActivity>>,
    mut epoch: Option<ResMut<crate::collision::ColliderEpoch>>,
) {
    if state.inner.is_empty() {
        return;
    }
    let t0 = Instant::now();
    let mut attached = 0u32;
    let roots = state
        .inner
        .iter()
        .filter_map(|t| state.tiles.get(t))
        .flat_map(|t| t.entity.into_iter().chain(t.wall));
    for entity in roots {
        let Ok(mut pc) = pending.get_mut(entity) else {
            continue;
        };
        let collider = match (pc.built.take(), pc.task.take()) {
            (Some(collider), _) => collider,
            (None, Some(task)) => block_on(task),
            (None, None) => continue,
        };
        // In `finish_colliders`' order, so avian's observers see what they see there.
        let mut e = commands.entity(entity);
        e.remove::<PendingCollider>().insert(collider);
        if pc.static_body {
            e.insert(RigidBody::Static);
        }
        if let Some(layers) = pc.layers {
            e.insert(layers);
        }
        attached += 1;
    }
    if attached == 0 {
        return;
    }
    if let Some(activity) = activity.as_mut() {
        activity.colliders_attached += attached;
        activity.wait_ms += t0.elapsed().as_secs_f32() * 1000.0;
    }
    if let Some(epoch) = epoch.as_mut() {
        epoch.bump();
    }
}

/// `WOW_NO_DOODAD_BODIES=1` spawns no doodad or prop hulls: a measurement lever that drops their
/// collision and pick clamp for the run.
pub(crate) fn doodad_bodies_disabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WOW_NO_DOODAD_BODIES").is_some())
}

/// `WOW_BARE_DOODAD_COLLIDERS=1` spawns doodad and prop hulls without `RigidBody::Static`: avian
/// files a body-less collider as standalone, still spatial-queryable and a contact target.
pub(crate) fn doodad_hulls_bare() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WOW_BARE_DOODAD_COLLIDERS").is_some())
}

/// A model's collision hull in world space as `(vertices, triangles)`: `wow_to_bevy`, then the
/// placement [`Transform`].
pub fn placement_collider_data(
    hull: Option<&CollisionMesh>,
    transform: &Transform,
) -> Option<(Vec<Vec3>, Vec<[u32; 3]>)> {
    let hull = hull?;
    if hull.indices.len() < 3 {
        return None;
    }
    let verts: Vec<Vec3> = hull
        .positions
        .iter()
        .map(|p| transform.transform_point(wow_to_bevy(*p)))
        .collect();
    let tris: Vec<[u32; 3]> = hull
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|c| [c[0], c[1], c[2]])
        .collect();
    Some((verts, tris))
}

/// A terrain tile as one fan trimesh welded from its decoded MCNK chunks in draw space, the
/// loader's hole-masked indices rebased onto the running vertex count: what the tile's collider
/// was before [`TerrainField`], kept as the reference its test compares against.
#[cfg(test)]
pub(super) fn terrain_collider_data(
    chunks: &[benilla_formats::ChunkMesh],
) -> Option<(Vec<Vec3>, Vec<[u32; 3]>)> {
    let mut verts: Vec<Vec3> = Vec::new();
    let mut tris: Vec<[u32; 3]> = Vec::new();
    for chunk in chunks {
        let base = verts.len() as u32;
        verts.extend(chunk.positions.iter().map(|p| wow_to_bevy(*p)));
        tris.extend(
            chunk
                .indices
                .as_chunks::<3>()
                .0
                .iter()
                .map(|c| [base + c[0], base + c[1], base + c[2]]),
        );
    }
    (!tris.is_empty()).then_some((verts, tris))
}

/// A tile's terrain as an exact heightfield: parry's `HeightField` needs no BVH (~0.3 MB a tile
/// against a trimesh's ~9 MB), and on a grid of half cells it reproduces the centre fan exactly.
/// Each cell of the 8×8 per chunk splits into 2×2 half cells: the outer verts and the centre land
/// on grid points, an edge's midpoint is the mean of its two ends (exact on a straight edge), and
/// each half cell's diagonal runs from its cell's corner to the centre (parry's
/// `ZIGZAG_SUBDIVISION` exactly where row and column parity agree), so its two triangles lie in
/// two of the four fan triangles. A hole drops its 2×2-cell block (4×4 half cells). Every triangle
/// faces up, as the fan's do. Grid points are placed by position, not by index arithmetic.
pub struct TerrainField {
    heights: avian3d::parry::utils::Array2<f32>,
    status: avian3d::parry::utils::Array2<avian3d::parry::shape::HeightFieldCellStatus>,
    scale: Vec3,
    /// The field's centre in draw space: the collider's translation (parry centres the field on
    /// its local origin).
    pub center: Vec3,
}

impl TerrainField {
    /// The collider, built on the async compute pool for [`finish_colliders`].
    pub fn build_task(self) -> Task<Collider> {
        AsyncComputeTaskPool::get().spawn(async move { self.collider() })
    }

    /// The collider, in the field's own frame (translate by [`Self::center`]).
    pub fn collider(self) -> Collider {
        let mut field = avian3d::parry::shape::HeightField::new(self.heights, self.scale);
        *field.cells_statuses_mut() = self.status;
        Collider::from(avian3d::parry::shape::SharedShape::new(field))
    }
}

/// [`TerrainField`] from a tile's decoded chunks; `None` with no drawn chunk.
pub(super) fn terrain_heightfield_data(
    chunks: &[benilla_formats::ChunkMesh],
) -> Option<TerrainField> {
    use avian3d::parry::shape::HeightFieldCellStatus as Cell;
    use avian3d::parry::utils::Array2;
    // A cell's outer verts are `r·17 + c` (9×9), its centre `r·17 + 9 + c` (8×8).
    let outer = |r: usize, c: usize| r * 17 + c;
    let drawn: Vec<&benilla_formats::ChunkMesh> = chunks
        .iter()
        .filter(|c| c.positions.len() == 145 && c.indices.len() >= 3)
        .collect();
    let first = *drawn.first()?;
    // Half a cell, from the first chunk's outer row; the grid's origin is the tile's min corner.
    let half = {
        let a = wow_to_bevy(first.positions[outer(0, 0)]);
        let b = wow_to_bevy(first.positions[outer(0, 1)]);
        0.5 * Vec2::new(b.x - a.x, b.z - a.z).length()
    };
    let (mut min, mut max) = (Vec2::splat(f32::INFINITY), Vec2::splat(f32::NEG_INFINITY));
    for chunk in &drawn {
        for p in &chunk.positions {
            let q = wow_to_bevy(*p);
            min = min.min(Vec2::new(q.x, q.z));
            max = max.max(Vec2::new(q.x, q.z));
        }
    }
    // Grid point (row along z, column along x) of a draw-space position.
    let cols = ((max.x - min.x) / half).round() as usize + 1;
    let rows = ((max.y - min.y) / half).round() as usize + 1;
    let at = |q: Vec3| -> (usize, usize) {
        (
            ((q.z - min.y) / half).round() as usize,
            ((q.x - min.x) / half).round() as usize,
        )
    };
    let mut heights = Array2::zeros(rows, cols);
    let mut known = Array2::repeat(rows, cols, false);
    let mut holed = Array2::repeat(rows - 1, cols - 1, true);
    for chunk in &drawn {
        let mut put = |i: usize| {
            let q = wow_to_bevy(chunk.positions[i]);
            let ij = at(q);
            heights[ij] = q.y;
            known[ij] = true;
            ij
        };
        for r in 0..9 {
            for c in 0..9 {
                put(outer(r, c));
            }
        }
        for r in 0..8 {
            for c in 0..8 {
                // The centre sits on the cell's middle grid point; its 2×2 half cells surround it.
                let (ci, cj) = put(r * 17 + 9 + c);
                if chunk.holes & (1u16 << ((r >> 1) * 4 + (c >> 1))) == 0 {
                    for (i, j) in [(ci - 1, cj - 1), (ci - 1, cj), (ci, cj - 1), (ci, cj)] {
                        holed[(i, j)] = false;
                    }
                }
            }
        }
    }
    // Edge midpoints: the mean of the two ends, along whichever axis the point is odd on.
    for i in 0..rows {
        for j in 0..cols {
            if known[(i, j)] {
                continue;
            }
            let ends = if i % 2 == 0 && j % 2 == 1 && j + 1 < cols {
                Some([(i, j - 1), (i, j + 1)])
            } else if i % 2 == 1 && j % 2 == 0 && i + 1 < rows {
                Some([(i - 1, j), (i + 1, j)])
            } else {
                None
            };
            if let Some([a, b]) = ends.filter(|[a, b]| known[*a] && known[*b]) {
                heights[(i, j)] = 0.5 * (heights[a] + heights[b]);
                known[(i, j)] = true;
            }
        }
    }
    let mut status = Array2::repeat(rows - 1, cols - 1, Cell::empty());
    for i in 0..rows - 1 {
        for j in 0..cols - 1 {
            let corners = [(i, j), (i + 1, j), (i, j + 1), (i + 1, j + 1)];
            status[(i, j)] = if holed[(i, j)] || corners.iter().any(|c| !known[*c]) {
                Cell::CELL_REMOVED
            } else if i % 2 == j % 2 {
                Cell::ZIGZAG_SUBDIVISION
            } else {
                Cell::empty()
            };
        }
    }
    // The measured extent, not `half` times the count: one pair's spacing carries f32 rounding
    // that 256 half cells would multiply (0.04 yd across a tile).
    let size = max - min;
    Some(TerrainField {
        heights,
        status,
        scale: Vec3::new(size.x, 1.0, size.y),
        center: Vec3::new(min.x + 0.5 * size.x, 0.0, min.y + 0.5 * size.y),
    })
}

/// How far an impassable chunk's fence rises from the chunk AABB's min z (`chunk+0x4c`): the
/// reference's `(0, 0, 32000.0)` (`0x46fa0000` at `0x6ab599`, emitter `0x6ab530`). Nothing extends
/// below, so a mover under the chunk floor (a mine, the sea bed) passes beneath.
const FENCE_REACH: f32 = 32000.0;

/// A tile's impassable-chunk fences as `(vertices, triangles)`, the reference's emitter
/// `0x6ab530` reached from the movement box gather `0x6721b0 → 0x6aa8b0 → 0x6aadc0`. The flag is
/// `CMapChunk+0xc & 0x40` (set at `0x6af5f0` from MCNK header bit 1, read only at `0x6aae2a`),
/// tested once per chunk.
///
/// - Additive: the terrain under a flagged chunk stays walkable; the flag only adds a fence.
/// - All four sides of every flagged chunk, neighbours unread, so a mover inside the band cannot
///   cross within it either and no tile seam is ever consulted.
/// - Outward normals: with the facing gate (`n·dir ≤ −1e-5`) you cannot walk in, and can always
///   walk out.
/// - `n.z == 0`: a fence is never a floor or a step.
///
/// The segment and ray path never reads the flag: camera pull-in, mouse-pick and LOS are
/// wall-blind, and terrain height is not gated.
pub(super) fn impassable_wall_data(
    chunks: &[benilla_formats::ChunkMesh],
) -> Option<(Vec<Vec3>, Vec<[u32; 3]>)> {
    let mut verts: Vec<Vec3> = Vec::new();
    let mut tris: Vec<[u32; 3]> = Vec::new();
    for chunk in chunks.iter().filter(|c| c.impassable) {
        // `chunk+0x4c`: the min corner of the AABB over the chunk's 145 vertices (`0x6b0e50`).
        let Some(floor) = chunk.positions.iter().map(|p| p[2]).reduce(f32::min) else {
            continue; // a chunk with no vertices has no AABB to stand a fence on
        };
        // The four sides from the 9×9 outer grid's corners (row `r` starts at `r·17`), each walked
        // along `t = ẑ × n` (+X north, +Y west) so `(b−a)×(c−a)` points out of the chunk.
        const SIDES: [[usize; 2]; 4] = [
            [8, 0],     // north (row 0), walked east → west
            [136, 144], // south (row 8), walked west → east
            [0, 136],   // west (col 0), walked north → south
            [144, 8],   // east (col 8), walked south → north
        ];
        for [from, to] in SIDES {
            let (Some(a), Some(b)) = (chunk.positions.get(from), chunk.positions.get(to)) else {
                continue; // a chunk without its outer grid has no side to stand a fence on
            };
            let base = verts.len() as u32;
            for (p, z) in [
                (a, floor),
                (b, floor),
                (b, floor + FENCE_REACH),
                (a, floor + FENCE_REACH),
            ] {
                verts.push(wow_to_bevy([p[0], p[1], z]));
            }
            tris.push([base, base + 1, base + 2]);
            tris.push([base, base + 2, base + 3]);
        }
    }
    (!tris.is_empty()).then_some((verts, tris))
}

/// Builds a static trimesh [`Collider`] on the async compute pool, for [`finish_colliders`].
pub fn build_collider_task(verts: Vec<Vec3>, tris: Vec<[u32; 3]>) -> Task<Collider> {
    AsyncComputeTaskPool::get().spawn(async move { Collider::trimesh(verts, tris) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use avian3d::prelude::PhysicsPlugins;
    use bevy::app::TaskPoolPlugin;
    use bevy::ecs::system::RunSystemOnce;

    use benilla_formats::{ChunkMesh, CHUNK_SIZE};

    /// The heightfield is the fan trimesh: on real tiles (Orgrimmar's two, which include The
    /// Drag, and Elwynn's wall pair) 20000 vertical rays at random points inside the tile hit or
    /// miss alike (holes included) at the same height, and every heightfield triangle faces up.
    /// "Same" is within 2.5 cm: the source verts' x/y are each computed in f32 and sit up to a
    /// fraction of a millimetre off the regular grid the field puts them on, which a steep slope
    /// turns into ~1-2 cm of height (measured: 1.1 cm inside a chunk, 1.7 cm on a border).
    #[test]
    fn the_terrain_heightfield_is_the_fan_trimesh_under_every_ray() {
        use avian3d::parry::query::Ray;
        let data = benilla_formats::wow_data_or_skip!();
        let mut chain = benilla_formats::open_chain(&data).expect("open chain");
        for (map, tx, ty) in [
            ("Kalimdor", 40, 28),
            ("Kalimdor", 40, 29),
            ("Azeroth", 32, 44),
            ("Azeroth", 33, 44),
        ] {
            let tile = benilla_formats::load_tile_mesh(&mut chain, map, tx, ty).expect("tile");
            let (verts, tris) = terrain_collider_data(&tile.chunks).expect("trimesh");
            let mesh = Collider::trimesh(verts, tris);
            let field = terrain_heightfield_data(&tile.chunks).expect("heightfield");
            let center = field.center;
            let hf = field.collider();

            let hf_shape = hf.shape().as_heightfield().expect("a heightfield");
            let mut down_facing = 0;
            for (_, tri) in hf_shape.triangles().enumerate() {
                if tri.normal().is_some_and(|n| n.y <= 0.0) {
                    down_facing += 1;
                }
            }
            assert_eq!(down_facing, 0, "{map} {tx},{ty}: triangles facing down");

            let bounds = mesh.shape().compute_local_aabb();
            let (mut worst, mut hits, mut misses, mut disagree) = (0.0f32, 0, 0, Vec::new());
            let mut on_edge = 0;
            // Within a millimetre of a half-cell grid line, in the field's frame.
            // `ncols()` counts cells, which is what a grid line's index runs over.
            let (sc, n) = (hf_shape.scale(), hf_shape.ncols() as f32);
            let on_line = |lx: f32, lz: f32| {
                let near = |v: f32, size: f32| {
                    let g = (v / size + 0.5) * n;
                    (g - g.round()).abs() * size / n < 1e-3
                };
                near(lx, sc.x) || near(lz, sc.z)
            };
            let mut seed = 0x9e3779b9u32 ^ (tx * 64 + ty);
            let mut next = || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as f32 / u32::MAX as f32
            };
            for _ in 0..20000 {
                // Off the tile's outer edge, which the neighbour tile covers in play.
                let x = bounds.mins.x + 0.01 + next() * (bounds.maxs.x - bounds.mins.x - 0.02);
                let z = bounds.mins.z + 0.01 + next() * (bounds.maxs.z - bounds.mins.z - 0.02);
                let from = Vec3::new(x, bounds.maxs.y + 10.0, z);
                let a = mesh
                    .shape()
                    .cast_local_ray(&Ray::new(from, Vec3::NEG_Y), f32::MAX, true);
                let b = hf.shape().cast_local_ray(
                    &Ray::new(from - center, Vec3::NEG_Y),
                    f32::MAX,
                    true,
                );
                match (a, b) {
                    (Some(a), Some(b)) => {
                        hits += 1;
                        worst = worst.max((a - b).abs());
                    }
                    (None, None) => misses += 1,
                    // A ray exactly on a hole's edge line: the trimesh owns the shared edge, the
                    // field's cell lookup the hole. A zero-width line; a cast body has width.
                    _ if on_line(x - center.x, z - center.z) => on_edge += 1,
                    _ => disagree.push((x, z, a, b)),
                }
            }
            println!(
                "{map} {tx},{ty}: {hits} hits, {misses} misses, {on_edge} on a hole's edge line, \
                 worst {worst:.6} yd, {} disagree",
                disagree.len()
            );
            assert!(
                disagree.is_empty(),
                "{map} {tx},{ty}: hit/miss differ at {:?}",
                &disagree[..disagree.len().min(5)]
            );
            assert!(
                worst < 0.025,
                "{map} {tx},{ty}: heights differ by {worst} yd"
            );
        }
    }

    /// One flat MCNK at `(ix, iy)` with only what the wall builder reads. The tile's NW corner is
    /// the WoW origin: rows step −x and columns −y, as `adt_to_tile_mesh` lays them out.
    fn chunk(ix: u32, iy: u32, impassable: bool) -> ChunkMesh {
        let (nw_x, nw_y) = (-(iy as f32) * CHUNK_SIZE, -(ix as f32) * CHUNK_SIZE);
        let mut positions = vec![[0.0_f32; 3]; 145];
        for r in 0..9usize {
            for c in 0..9usize {
                positions[r * 17 + c] = [
                    nw_x - r as f32 * CHUNK_SIZE / 8.0,
                    nw_y - c as f32 * CHUNK_SIZE / 8.0,
                    0.0,
                ];
            }
        }
        ChunkMesh {
            positions,
            normals: Vec::new(),
            uvs: Vec::new(),
            indices: Vec::new(),
            holes: 0,
            base_texture: None,
            layer_textures: Vec::new(),
            layer_effect_ids: Vec::new(),
            alpha_map: None,
            shadow: None,
            pred_tex: [0; 64],
            no_effect_doodad: [false; 64],
            index_x: ix,
            index_y: iy,
            area_id: 0,
            impassable,
            liquids: Vec::new(),
        }
    }

    /// The centre of chunk `(ix, iy)`'s footprint in Bevy space, at z = 0.
    fn chunk_centre(ix: u32, iy: u32) -> Vec3 {
        wow_to_bevy([
            -(iy as f32 + 0.5) * CHUNK_SIZE,
            -(ix as f32 + 0.5) * CHUNK_SIZE,
            0.0,
        ])
    }

    #[test]
    fn a_flagged_chunk_is_boxed_in_by_outward_facing_walls() {
        let chunks: Vec<ChunkMesh> = [(1, 1, true), (0, 1, false), (1, 0, false)]
            .into_iter()
            .map(|(ix, iy, f)| chunk(ix, iy, f))
            .collect();
        let (verts, tris) = impassable_wall_data(&chunks).expect("one flagged chunk, four walls");
        assert_eq!(tris.len(), 8, "four sides, two triangles each");

        let centre = chunk_centre(1, 1);
        for t in &tris {
            let (a, b, c) = (
                verts[t[0] as usize],
                verts[t[1] as usize],
                verts[t[2] as usize],
            );
            let n = (b - a).cross(c - a).normalize();
            assert!(n.y.abs() < 1e-5, "a wall face is vertical: {n:?}");
            let face = (a + b + c) / 3.0;
            assert!(
                n.dot(face - centre) > 0.0,
                "face at {face:?} is wound back INTO the chunk (normal {n:?})"
            );
        }
    }

    /// Two flagged neighbours carry a doubled fence on their shared side, wound apart.
    #[test]
    fn every_flagged_chunk_fences_all_four_of_its_own_sides() {
        let chunks: Vec<ChunkMesh> = [(1, 1, true), (2, 1, true), (0, 1, false), (3, 1, false)]
            .into_iter()
            .map(|(ix, iy, f)| chunk(ix, iy, f))
            .collect();
        let (verts, tris) = impassable_wall_data(&chunks).expect("two flagged chunks");
        assert_eq!(tris.len(), 16, "four sides each, two triangles a side");

        // The shared plane: WoW y = −2·CHUNK_SIZE, Bevy x = +2·CHUNK_SIZE.
        let shared = 2.0 * CHUNK_SIZE;
        let normals: Vec<Vec3> = tris
            .iter()
            .filter(|t| {
                t.iter()
                    .all(|&i| (verts[i as usize].x - shared).abs() < 1e-3)
            })
            .map(|t| {
                let (a, b, c) = (
                    verts[t[0] as usize],
                    verts[t[1] as usize],
                    verts[t[2] as usize],
                );
                (b - a).cross(c - a).normalize()
            })
            .collect();
        assert_eq!(normals.len(), 4, "two coplanar quads on the shared side");
        assert!(
            normals.iter().any(|n| n.x > 0.9) && normals.iter().any(|n| n.x < -0.9),
            "the shared side is fenced from both directions: {normals:?}"
        );
    }

    #[test]
    fn a_fence_stands_on_the_chunk_floor_and_only_rises() {
        // Dished 12 yd below its corners, so the floor is not the corners' height.
        let mut c = chunk(1, 1, true);
        c.positions[4 * 17 + 4][2] = -12.0;
        let (verts, _) = impassable_wall_data(&[c]).expect("walls");
        let (lo, hi) = verts.iter().fold((f32::MAX, f32::MIN), |(lo, hi), v| {
            (lo.min(v.y), hi.max(v.y))
        });
        assert!(
            (lo - -12.0).abs() < 1e-3,
            "based at the chunk floor, got {lo}"
        );
        assert!(
            (hi - (-12.0 + FENCE_REACH)).abs() < 1e-3,
            "rises {FENCE_REACH} from the floor, got {hi}"
        );
    }

    #[test]
    fn a_tile_with_no_flagged_chunk_builds_no_wall() {
        let chunks = vec![chunk(0, 0, false), chunk(1, 0, false)];
        assert!(impassable_wall_data(&chunks).is_none());
    }

    /// On the shipped data, through the real cast: a body walking east from the pin stops at the
    /// flagged chunk's boundary 1.46 yd away, as 1.12.1 stops it, and walks through without walls.
    #[test]
    fn a_body_walking_east_from_the_pin_is_stopped_at_the_wall() {
        /// The pin, and the MCNK boundary the flagged chunk starts at (WoW y; east is −y).
        const PIN: [f32; 3] = [-6601.98, -531.87, 335.60];
        const WALL_Y: f32 = 32.0 * benilla_formats::TILE_SIZE - 528.0 * CHUNK_SIZE;
        const R: f32 = 0.5;
        const SKIN: f32 = 0.01;

        let data = benilla_formats::wow_data_or_skip!();
        let mut chain = benilla_formats::open_chain(&data).expect("open chain");
        // Both sides of the seam: the pin's tile and the one that authors the wall.
        let tiles: Vec<benilla_formats::TileMesh> = [(32, 44), (33, 44)]
            .into_iter()
            .map(|(tx, ty)| {
                benilla_formats::load_tile_mesh(&mut chain, "Azeroth", tx, ty)
                    .expect("Azeroth tile")
            })
            .collect();
        let ground = tiles
            .iter()
            .find_map(|t| benilla_formats::terrain_height_at(&t.chunks, PIN))
            .expect("terrain under the pin");

        // How far a body walks east from height `z`, with or without the walls.
        let run_east = |walls: bool, z: f32| -> Option<f32> {
            let mut app = App::new();
            // `WorldCollision` reads the mover's trace exclusions, which the world plugins own.
            app.init_resource::<crate::collision::MoverTraceExclusions>();
            app.add_plugins((
                MinimalPlugins,
                bevy::transform::TransformPlugin,
                bevy::asset::AssetPlugin::default(),
                bevy::scene::ScenePlugin,
                avian3d::prelude::PhysicsPlugins::new(bevy::app::PostUpdate),
            ));
            app.init_asset::<Mesh>();
            for t in &tiles {
                // The ground as play builds it: a heightfield at its tile's centre.
                if let Some(field) = terrain_heightfield_data(&t.chunks) {
                    let center = field.center;
                    app.world_mut().spawn((
                        RigidBody::Static,
                        field.collider(),
                        Transform::from_translation(center),
                    ));
                }
                if let Some((v, i)) = walls.then(|| impassable_wall_data(&t.chunks)).flatten() {
                    app.world_mut().spawn((
                        RigidBody::Static,
                        Collider::trimesh(v, i),
                        crate::collision::walk_layers(),
                        Transform::default(),
                    ));
                }
            }
            app.update(); // builds Position/Rotation and the spatial-query trees
            app.world_mut()
                .run_system_once(move |world: crate::collision::WorldCollision| {
                    let capsule = Collider::capsule(R, 1.0);
                    // East is −y in WoW, +x in Bevy.
                    let from = wow_to_bevy([PIN[0], PIN[1], z]);
                    world
                        .cast_body(&capsule, from, Vec3::X * 5.0, SKIN)
                        .map(|h| h.distance)
                })
                .unwrap()
        };

        // Feet on the ground at the pin.
        let walking = ground + 1.0 + R;
        // Without the walls nothing stops the body within the 5 yd it walks.
        assert!(
            run_east(false, walking).is_none(),
            "the pin itself — the terrain here does not stop a body, which is why the flag has to"
        );
        // With them, the capsule's leading surface stops at the chunk boundary.
        let d = run_east(true, walking).expect("the impassable chunk's fence stops the body");
        let leading_edge = -(wow_to_bevy([PIN[0], PIN[1], 0.0]).x + d) - R;
        assert!(
            (leading_edge - (WALL_Y + SKIN)).abs() < 0.05,
            "stopped at y={leading_edge}, wall is at y={WALL_Y} (travelled {d} yd)"
        );

        // Below the flagged chunk's floor only a fence reaching down could stop the body, and the
        // reference's does not.
        let floor = tiles
            .iter()
            .flat_map(|t| t.chunks.iter())
            .filter(|c| c.impassable)
            .filter_map(|c| c.positions.iter().map(|p| p[2]).reduce(f32::min))
            .fold(f32::MAX, f32::min);
        assert!(
            run_east(true, floor - 20.0).is_none(),
            "the fence reached below the chunk floor (floor {floor})"
        );
    }

    /// A tile-sized grid: 256 quads -> 512 triangles per chunk, `chunks` chunks.
    fn grid(chunks: usize) -> (Vec<Vec3>, Vec<[u32; 3]>) {
        let (mut verts, mut tris) = (Vec::new(), Vec::new());
        for c in 0..chunks {
            let base = verts.len() as u32;
            for r in 0..17u32 {
                for q in 0..17u32 {
                    verts.push(Vec3::new(
                        q as f32,
                        ((q + r + c as u32) % 5) as f32,
                        r as f32,
                    ));
                }
            }
            for r in 0..16u32 {
                for q in 0..16u32 {
                    let i = base + r * 17 + q;
                    tris.push([i, i + 1, i + 17]);
                    tris.push([i + 1, i + 18, i + 17]);
                }
            }
        }
        (verts, tris)
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins((
            TaskPoolPlugin::default(),
            bevy::asset::AssetPlugin::default(),
        ));
        app.init_asset::<Mesh>();
        // Avian's collider hooks, so an attach costs what it does in the client; its schedules
        // never run.
        app.add_plugins(PhysicsPlugins::default());
        app.finish();
        app.cleanup();
        app
    }

    /// Polled until done, not slept for: the timing is the pool's and is not asserted.
    #[test]
    fn a_finished_task_hands_its_shape_over_and_attaches() {
        let mut app = test_app();
        let (verts, tris) = grid(4);
        let task = build_collider_task(verts, tris);
        let e = app
            .world_mut()
            .spawn((Transform::default(), PendingCollider::new(task, None, true)))
            .id();
        for _ in 0..100_000 {
            app.world_mut().run_system_once(finish_colliders).unwrap();
            if app.world().entity(e).contains::<Collider>() {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            app.world().entity(e).contains::<Collider>(),
            "the pool finished the build (or never did — a hung pool, not a budget)"
        );
        assert!(
            app.world().entity(e).contains::<RigidBody>(),
            "a static body rides the attach"
        );
        assert!(app.world().get::<PendingCollider>(e).is_none());
    }

    /// A held inner tile's terrain collider is attached in its frame, its build waited on, and a
    /// cast meets it before any physics step has run: the next frame's mover stands on it.
    #[test]
    fn a_held_inner_tiles_collider_is_attached_and_castable_in_its_frame() {
        use avian3d::prelude::{SpatialQuery, SpatialQueryFilter};

        let mut app = test_app();
        let (verts, tris) = grid(16);
        // Sixteen chunks of 512 triangles, likely still building on the next line; it attaches now.
        let task = build_collider_task(verts, tris);
        let inner = app
            .world_mut()
            .spawn((Transform::default(), PendingCollider::new(task, None, true)))
            .id();
        let mut state = crate::terrain_stream::TerrainStreamer::default();
        state.tiles.insert(
            (32, 44),
            crate::terrain_stream::TileState {
                entity: Some(inner),
                ..crate::terrain_stream::TileState::requested(Default::default())
            },
        );
        state.inner = vec![(32, 44)];
        app.insert_resource(state);

        app.world_mut()
            .run_system_once(attach_inner_colliders)
            .unwrap();
        assert!(app.world().entity(inner).contains::<Collider>());
        assert!(app.world().entity(inner).contains::<RigidBody>());
        assert!(app.world().get::<PendingCollider>(inner).is_none());
        let hit = app
            .world_mut()
            .run_system_once(|spatial: SpatialQuery| {
                spatial
                    .cast_ray(
                        Vec3::new(8.5, 100.0, 8.5),
                        Dir3::NEG_Y,
                        200.0,
                        true,
                        &SpatialQueryFilter::default(),
                    )
                    .map(|h| h.entity)
            })
            .unwrap();
        assert_eq!(
            hit,
            Some(inner),
            "the collider tree holds it without a step"
        );
    }

    /// Staged already built with a zero budget, so each call attaches exactly one.
    #[test]
    fn attach_budget_spreads_a_burst_without_losing_colliders() {
        const N: usize = 40;
        let mut app = test_app();
        app.insert_resource(AttachBudget(std::time::Duration::ZERO));
        let (verts, tris) = grid(16);
        let entities: Vec<Entity> = (0..N)
            .map(|_| {
                let collider = Collider::trimesh(verts.clone(), tris.clone());
                app.world_mut()
                    .spawn((
                        Transform::default(),
                        PendingCollider::ready(collider, None, true),
                    ))
                    .id()
            })
            .collect();

        let attached = |app: &mut App| {
            entities
                .iter()
                .filter(|e| app.world().entity(**e).contains::<Collider>())
                .count()
        };

        app.world_mut().run_system_once(finish_colliders).unwrap();
        assert_eq!(
            attached(&mut app),
            1,
            "a zero budget is past after the first attach: exactly one lands per call"
        );

        // Drain: every remaining build must still land, none lost to the deferral.
        for _ in 0..N {
            if attached(&mut app) == N {
                break;
            }
            app.world_mut().run_system_once(finish_colliders).unwrap();
        }
        assert_eq!(attached(&mut app), N, "a finished collider was dropped");
        for e in &entities {
            assert!(app.world().entity(*e).contains::<RigidBody>());
            assert!(!app.world().entity(*e).contains::<PendingCollider>());
        }
    }
}
