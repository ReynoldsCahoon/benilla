//! Within-map art residency: the world-art dedup caches drop what was last wanted more than a
//! radius from the view focus; a cross-map transition still clears them outright.
//!
//! A use (a [`SpatialCache::fetch`] hit or an insert) clears an entry's stamp to `None` and each
//! sweep stamps every `None` with the current focus, so no call site needs the camera and a cache
//! nothing sweeps never expires. Eviction only drops the dedup: a drawn entity keeps its handles.

use std::hash::Hash;

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::time::Real;

use benilla_assets::SpatialCache;
use benilla_formats::TILE_SIZE;

/// Real-clock seconds between sweeps (housekeeping, not world time); also the stamp's resolution.
const SWEEP_SECS: f32 = 1.0;

/// Five ADT tiles (2667 yd), the smallest round tile multiple clear of [`radius_floor`].
const DEFAULT_RADIUS_YD: f32 = 5.0 * TILE_SIZE;

/// The radius of the caches whose entries serve units and GameObjects ([`ArtSlot::entity_scoped`]),
/// which the server spawns and despawns by its visibility range (cmangos: 100 yd on continents,
/// 170 in instances), not the tile streamer: past it no unit the entry served is still in view, and
/// a dropped entry only costs a rebuild when that model shows again. A city's passers-by otherwise
/// stay resident for the whole map, a material and its textures each.
const ENTITY_RADIUS_YD: f32 = 250.0;

/// How long an entity-art cache entry nothing else holds stays before it drops: longer than a
/// despawn and respawn or a step out of view and back, so neither rebuilds. Inside a city the
/// radius never binds (the whole of Orgrimmar sits within it); use does.
const UNUSED_GRACE_SECS: f32 = 60.0;

/// [`UNUSED_GRACE_SECS`], or `$WOW_DISPLAY_GRACE` seconds: a test knob, so a route run's end census
/// need not wait out the full grace. The display-model caches in `benilla-app` share it. None while
/// the [`crate::memory_governor`] is short of memory: an unused entry drops at the next sweep.
pub fn unused_grace() -> f32 {
    if crate::memory_governor::tier() >= crate::memory_governor::MemoryTier::Tight {
        return 0.0;
    }
    static GRACE: std::sync::LazyLock<f32> = std::sync::LazyLock::new(|| {
        std::env::var("WOW_DISPLAY_GRACE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(UNUSED_GRACE_SECS)
    });
    *GRACE
}

/// Whether anything besides the cache's own copy holds `handle`: an entity drawing it, a display
/// model's parts, a booth scene, a pending spawn. A `Uuid` handle is never the cache's to drop.
fn held_elsewhere<A: Asset>(handle: &Handle<A>) -> bool {
    match handle {
        Handle::Strong(arc) => std::sync::Arc::strong_count(arc) > 1,
        Handle::Uuid(..) => true,
    }
}

/// A cache value the use rule can test: whether anything outside the cache holds it.
pub trait HeldValue {
    fn held_elsewhere(&self) -> bool;
}

impl<A: Asset> HeldValue for Handle<A> {
    fn held_elsewhere(&self) -> bool {
        held_elsewhere(self)
    }
}

/// An empty entry (a build that produced nothing) costs nothing and keeps a rebuild from retrying.
impl<A: Asset> HeldValue for Option<Handle<A>> {
    fn held_elsewhere(&self) -> bool {
        self.as_ref().is_none_or(held_elsewhere)
    }
}

/// The far corner of the widest tile block the residency window keeps (at the `farclip` clamp's
/// maximum), plus a tile: a stamp is where the viewer stood, so no smaller radius is safe.
fn radius_floor() -> f32 {
    crate::terrain_stream::window::max_resident_reach_yd(*crate::view::FARCLIP_RANGE.end())
}

/// `$WOW_ART_RADIUS` in yards (`0` or less turns eviction off); a value under [`radius_floor`] is
/// raised to it with a warning.
fn radius_from_env() -> f32 {
    let floor = radius_floor();
    match std::env::var("WOW_ART_RADIUS")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
    {
        Some(r) if r <= 0.0 => 0.0,
        Some(r) if r < floor => {
            warn!(
                "art-scope: WOW_ART_RADIUS={r} is inside the streamer's own reach ({floor:.0} yd \
                 at the widest view distance) — using {floor:.0}"
            );
            floor
        }
        Some(r) => r,
        None => DEFAULT_RADIUS_YD.max(floor),
    }
}

/// Which cache a census row is about, in journal-column order: the CSV only grows columns at the
/// end, so this order is part of the file format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArtSlot {
    /// `Placements::materials`: streamed doodad and WMO submesh materials.
    PlaceMats,
    /// `ModelMaterials`: every authored-batch material, units, GameObjects and booth scenes alike.
    ModelMats,
    /// `SkinComposites`: composited character body atlases.
    Skins,
    /// `WorldAssets::model_materials`: the ground-clutter material dedup.
    ClutterMats,
    /// `WorldAssets::textures`: decoded world BLPs by (path, wrap).
    Textures,
    /// `ClutterGeometry`: CPU submesh copies of every decoded detail M2.
    ClutterGeo,
}

impl ArtSlot {
    /// Whether the cache serves units and GameObjects, swept at [`ENTITY_RADIUS_YD`]: the
    /// authored-batch materials and the composited skins. The rest serve tile-streamed art.
    pub(crate) fn entity_scoped(self) -> bool {
        matches!(self, ArtSlot::ModelMats | ArtSlot::Skins)
    }

    pub const ALL: [ArtSlot; 6] = [
        ArtSlot::PlaceMats,
        ArtSlot::ModelMats,
        ArtSlot::Skins,
        ArtSlot::ClutterMats,
        ArtSlot::Textures,
        ArtSlot::ClutterGeo,
    ];

    pub(crate) fn column(self) -> &'static str {
        match self {
            ArtSlot::PlaceMats => "pmat",
            ArtSlot::ModelMats => "emat",
            ArtSlot::Skins => "skin",
            ArtSlot::ClutterMats => "cmat",
            ArtSlot::Textures => "tex",
            ArtSlot::ClutterGeo => "cgeo",
        }
    }

    fn idx(self) -> usize {
        match self {
            ArtSlot::PlaceMats => 0,
            ArtSlot::ModelMats => 1,
            ArtSlot::Skins => 2,
            ArtSlot::ClutterMats => 3,
            ArtSlot::Textures => 4,
            ArtSlot::ClutterGeo => 5,
        }
    }
}

/// Per-cache live and dropped counts, refreshed by every [`ArtScope::apply`], so one journal row
/// shows which cache is growing.
#[derive(Resource, Default)]
pub struct ArtCensus {
    live: [usize; ArtSlot::ALL.len()],
    dropped: [usize; ArtSlot::ALL.len()],
}

impl ArtCensus {
    /// Live entries in one cache as of its last [`ArtScope::apply`].
    pub fn live(&self, slot: ArtSlot) -> usize {
        self.live[slot.idx()]
    }

    /// Entries dropped by distance across every cache since the run began.
    pub fn dropped_total(&self) -> usize {
        self.dropped.iter().sum()
    }
}

/// The sweep's shared state: the radius, this tick's focus, and whether this frame is a sweep frame.
#[derive(Resource, Default)]
pub struct ArtScopeState {
    /// Eviction radius in yards, `0` for none; zero until [`configure_art_scope`] runs.
    radius: f32,
    /// The view focus in WoW coords; `None` (no avatar, no camera) suspends stamping and sweeping.
    focus: Option<[f32; 3]>,
    /// Set on the one frame per [`SWEEP_SECS`] that sweeps.
    due: bool,
    last_sweep: f32,
    /// Real seconds since app start this frame; the cache's dwell floor is measured on it.
    now: f32,
}

impl ArtScopeState {
    /// This frame's view focus in WoW coords, which the sweep measures from. The FPS journal logs
    /// it because its avatar columns stand still through a detached free-fly.
    pub fn focus(&self) -> Option<[f32; 3]> {
        self.focus
    }
}

/// What an owning module needs to scope its caches: the policy, plus the census to report into.
#[derive(SystemParam)]
pub struct ArtScope<'w> {
    state: Res<'w, ArtScopeState>,
    census: ResMut<'w, ArtCensus>,
}

impl ArtScope<'_> {
    /// Whether this frame sweeps: once a [`SWEEP_SECS`], for a cache with its own sweep.
    pub fn due(&self) -> bool {
        self.state.due
    }

    /// Sweeps one cache on sweep frames and records its residency.
    pub fn apply<K: Eq + Hash, V>(&mut self, cache: &mut SpatialCache<K, V>, slot: ArtSlot) {
        let radius = match slot.entity_scoped() {
            true => self.state.radius.min(ENTITY_RADIUS_YD),
            false => self.state.radius,
        };
        let dropped = match self.state.focus {
            Some(focus) if self.state.due && radius > 0.0 => {
                cache.scope(focus, radius, self.state.now)
            }
            _ => 0,
        };
        let i = slot.idx();
        self.census.live[i] = cache.len();
        self.census.dropped[i] += dropped;
        if dropped > 0 {
            debug!(
                "art-scope: dropped {dropped} {} entries beyond {:.0} yd ({} live)",
                slot.column(),
                radius,
                cache.len()
            );
        }
    }
}

impl ArtScope<'_> {
    /// [`Self::apply`], then on sweep frames also drop the entries of a handle cache that nothing
    /// outside it has held for [`unused_grace`]: the use rule, which binds inside a city where
    /// the radius never does.
    pub fn apply_unheld<K: Eq + Hash, V: HeldValue>(
        &mut self,
        cache: &mut SpatialCache<K, V>,
        slot: ArtSlot,
    ) {
        self.apply(cache, slot);
        if !self.state.due {
            return;
        }
        let dropped = cache.drop_unheld(self.state.now, unused_grace(), V::held_elsewhere);
        let i = slot.idx();
        self.census.live[i] = cache.len();
        self.census.dropped[i] += dropped;
        if dropped > 0 {
            debug!(
                "art-scope: dropped {dropped} unheld {} entries ({} live)",
                slot.column(),
                cache.len()
            );
        }
    }
}

impl ArtScope<'_> {
    /// [`Self::apply`], plus [`Self::apply_unheld`]'s use rule while the
    /// [`crate::memory_governor`] is short of memory: a handle cache otherwise swept only by
    /// distance (the tile-streamed art, which a city's walk revisits) gives up what nothing draws.
    pub fn apply_pressed<K: Eq + Hash, V: HeldValue>(
        &mut self,
        cache: &mut SpatialCache<K, V>,
        slot: ArtSlot,
    ) {
        match crate::memory_governor::tier() >= crate::memory_governor::MemoryTier::Tight {
            true => self.apply_unheld(cache, slot),
            false => self.apply(cache, slot),
        }
    }
}

fn configure_art_scope(mut state: ResMut<ArtScopeState>) {
    state.radius = radius_from_env();
    if state.radius > 0.0 {
        info!(
            "art-scope: within-map art evicts beyond {:.0} yd of the view focus",
            state.radius
        );
    } else {
        warn!("art-scope: eviction OFF ($WOW_ART_RADIUS=0) — within-map residency is unbounded");
    }
}

/// Publish this frame's focus and whether it sweeps. Unordered against every consumer: a stamp is
/// at most one sweep stale, and reading `due` a frame early only moves the sweep a frame.
fn track_art_scope(
    mut state: ResMut<ArtScopeState>,
    time: Res<Time<Real>>,
    focus: Res<crate::terrain_stream::ViewFocus>,
    camera: Query<&Transform, With<crate::view::WorldCamera>>,
) {
    let cam = camera.single().ok().map(|c| c.translation);
    state.focus = (focus.body_pos().is_some() || cam.is_some()).then(|| focus.resolve(cam));
    let now = time.elapsed_secs();
    state.now = now;
    state.due = now - state.last_sweep >= SWEEP_SECS;
    if state.due {
        state.last_sweep = now;
    }
}

/// Owns the sweep policy and the census; each cache's owner calls [`ArtScope::apply`].
pub(crate) struct ArtScopePlugin;

impl Plugin for ArtScopePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ArtCensus>()
            .init_resource::<ArtScopeState>()
            .add_systems(Startup, configure_art_scope)
            .add_systems(Update, track_art_scope);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unit and GameObject caches sweep inside the server's widest non-battleground visibility
    /// plus margin; the tile-streamed ones keep the streamer's floor.
    #[test]
    fn entity_caches_sweep_at_the_entity_radius() {
        let entity: Vec<ArtSlot> = ArtSlot::ALL
            .into_iter()
            .filter(|s| s.entity_scoped())
            .collect();
        assert_eq!(entity, [ArtSlot::ModelMats, ArtSlot::Skins]);
        assert!(ENTITY_RADIUS_YD > 170.0 && ENTITY_RADIUS_YD < radius_floor());
    }

    #[test]
    fn the_radius_clears_the_streamers_own_reach() {
        assert!(
            DEFAULT_RADIUS_YD > radius_floor(),
            "default {DEFAULT_RADIUS_YD} must clear the floor {}",
            radius_floor()
        );
        // (26 + 1 + 16) chunks each way at farclip 777, to the corner, plus a tile: ~2560 yd.
        assert!((radius_floor() - 2560.0).abs() < 1.0, "{}", radius_floor());
    }

    /// The cache's own copy alone is unheld; any clone outside it (an entity's material) holds it.
    #[test]
    fn only_a_clone_outside_the_cache_holds_a_handle() {
        let mut images = Assets::<Image>::default();
        let cached = images.add(Image::default());
        assert!(!held_elsewhere(&cached));
        let drawn = cached.clone();
        assert!(held_elsewhere(&cached));
        drop(drawn);
        assert!(!held_elsewhere(&cached));
    }

    /// `$WOW_ART_RADIUS` is unset in the test binary, as in every ordinary run.
    #[test]
    fn an_unconfigured_run_still_bounds_itself() {
        assert_eq!(radius_from_env(), DEFAULT_RADIUS_YD.max(radius_floor()));
    }
}
