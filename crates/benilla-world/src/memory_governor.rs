//! The memory governor: a low-spec safety net, not a reference behaviour. 1.12.1 has no such
//! thing; it ran on machines whose memory paged rather than killed. iOS kills an app outright at a
//! fixed footprint (~1450 MB on the iPad Air 2), so the client watches its headroom and sheds
//! what it can before the kill, in tiers.
//!
//! Headroom is what the process may still allocate: on iOS `os_proc_available_memory`; elsewhere
//! `$WOW_MEMORY_BUDGET_MB` less the physical footprint (`task_info(TASK_VM_INFO)`), so a Mac run
//! can play a small device. With no budget off iOS the governor never leaves [`MemoryTier::Normal`].
//! On iOS a budget may lower the limit further, to rehearse the tiers early. `$WOW_MEMORY_GOVERNOR=0`
//! turns it off everywhere (a before/after measurement); `$WOW_MEMORY_TRACE=1` logs every reading.
//!
//! A tier is entered the moment headroom falls under its floor and left only once headroom has
//! stayed [`RELEASE_MARGIN_MB`] over it for [`RELEASE_SECS`], so a burst that frees itself does
//! not flap the policy. Each change logs a `memory-governor:` line.
//!
//! What each tier sheds, cumulative:
//! - [`MemoryTier::Tight`]: unused art drops at the next sweep: the use-swept caches lose their
//!   grace ([`crate::art_scope::unused_grace`]) (placement materials among them), and the other
//!   tile-streamed handle caches (clutter materials, world textures), otherwise swept by distance
//!   alone, take the use rule ([`crate::art_scope::ArtScope::apply_pressed`]).
//!   Nothing drawn changes; art shown again rebuilds.
//! - [`MemoryTier::Low`]: other players past the nearest 60 wait unbuilt (`benilla-app`'s
//!   `entities::body_cap`, a deviation written down there), and the view distance drops to
//!   [`LOW_FARCLIP`] yards, the tile window with it ([`govern_view`]).
//! - [`MemoryTier::Critical`]: the built players past the nearest 30 are taken down, the view
//!   distance drops to [`CRITICAL_FARCLIP`], the streamer requests only the tiles it must have
//!   (the inner window, not the ring ahead), and no new spell effect starts on a unit that is
//!   neither the player nor the target (`benilla-app`'s `entities::spell_fx`).
//!
//! The visible cuts latch: taken under Low or Critical, they are given back only at Normal. Giving
//! the view back reloads its tiles, which under Tight would push the governor straight back down.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use bevy::prelude::*;
use bevy::time::Real;

/// Real seconds between headroom reads (a syscall or two each): short, so a burst of loads (two
/// tiles entering The Drag add ~160 MB within a second or two) is seen before it lands whole.
const SAMPLE_SECS: f32 = 0.1;

/// Headroom under which each tier begins, in MB: [`MemoryTier::Tight`], [`MemoryTier::Low`],
/// [`MemoryTier::Critical`]. On the iPad Air 2 Orgrimmar starts ~310 MB under the kill.
const FLOORS_MB: [u64; 3] = [250, 150, 75];

/// How far over a tier's floor headroom must climb before the governor steps back out.
const RELEASE_MARGIN_MB: u64 = 60;

/// How long headroom must hold over a floor plus the margin before the tier is left.
const RELEASE_SECS: f32 = 10.0;

/// How hard the governor is shedding, mildest first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemoryTier {
    #[default]
    Normal,
    Tight,
    Low,
    Critical,
}

impl MemoryTier {
    const ALL: [MemoryTier; 4] = [
        MemoryTier::Normal,
        MemoryTier::Tight,
        MemoryTier::Low,
        MemoryTier::Critical,
    ];

    /// The tier `headroom` falls in, by the floors alone.
    fn of(headroom_mb: u64) -> MemoryTier {
        let deeper = FLOORS_MB.iter().filter(|&&f| headroom_mb < f).count();
        MemoryTier::ALL[deeper]
    }

    /// The floor whose crossing entered this tier; `None` for Normal.
    fn floor_mb(self) -> Option<u64> {
        (self as usize).checked_sub(1).map(|i| FLOORS_MB[i])
    }
}

/// The current tier, for code with no world access (the cache graces).
static TIER: AtomicU8 = AtomicU8::new(MemoryTier::Normal as u8);

/// The last headroom read in MB; `u64::MAX` where there is none.
static HEADROOM_MB: AtomicU64 = AtomicU64::new(u64::MAX);

/// Headroom under which the streamer paces its loads even under the loading cover, in MB.
const PACE_LOADS_UNDER_MB: u64 = 400;

/// Whether loads must be paced: headroom is under [`PACE_LOADS_UNDER_MB`]. An unpaced load (a
/// world entry or a teleport, behind the loading cover) builds a whole window in a few long frames,
/// and no tier can act inside a frame: on the iPad Air 2 a same-map teleport into Orgrimmar from
/// ~315 MB of headroom failed an allocation inside one 0.7 s frame. Paced, the burst spreads over
/// short frames the governor reads between, and the cover stays up a little longer.
pub fn pace_loads() -> bool {
    HEADROOM_MB.load(Ordering::Relaxed) < PACE_LOADS_UNDER_MB
}

/// The tier the governor is in now.
pub fn tier() -> MemoryTier {
    MemoryTier::ALL[TIER.load(Ordering::Relaxed) as usize]
}

/// The governor's state, readable by the systems that shed.
#[derive(Resource, Default)]
pub struct MemoryGovernor {
    tier: MemoryTier,
    /// The last headroom read in MB; `None` where this platform has no reading.
    headroom_mb: Option<u64>,
    /// When headroom first held clear of the current tier's floor plus the margin.
    clear_since: Option<f32>,
    next_sample: f32,
}

impl MemoryGovernor {
    pub fn tier(&self) -> MemoryTier {
        self.tier
    }

    pub fn headroom_mb(&self) -> Option<u64> {
        self.headroom_mb
    }

    /// Take one headroom reading at real time `now`; the tier it settles on.
    fn observe(&mut self, headroom_mb: u64, now: f32) -> MemoryTier {
        self.headroom_mb = Some(headroom_mb);
        let raw = MemoryTier::of(headroom_mb);
        if raw >= self.tier {
            self.tier = raw;
            self.clear_since = None;
            return self.tier;
        }
        // Shallower by the floors: leave one tier at a time, each after a clear hold.
        let floor = self.tier.floor_mb().unwrap_or(0);
        if headroom_mb < floor + RELEASE_MARGIN_MB {
            self.clear_since = None;
            return self.tier;
        }
        let since = *self.clear_since.get_or_insert(now);
        if now - since >= RELEASE_SECS {
            // The next tier's hold starts here; the next reading checks it is clear too.
            self.tier = MemoryTier::ALL[self.tier as usize - 1];
            self.clear_since = Some(now);
        }
        self.tier
    }
}

/// `$WOW_MEMORY_BUDGET_MB`, the footprint this run may reach.
fn budget_mb() -> Option<u64> {
    static BUDGET: std::sync::LazyLock<Option<u64>> = std::sync::LazyLock::new(|| {
        std::env::var("WOW_MEMORY_BUDGET_MB")
            .ok()
            .and_then(|s| s.parse().ok())
    });
    *BUDGET
}

/// This process's physical footprint in bytes, the figure iOS's limit counts.
fn phys_footprint() -> Option<u64> {
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    {
        unsafe extern "C" {
            static mach_task_self_: u32;
            fn task_info(task: u32, flavor: i32, info: *mut u32, count: *mut u32) -> i32;
        }
        const TASK_VM_INFO: i32 = 22;
        // `task_vm_info_data_t` as 8-byte words: phys_footprint is word 18 (revision 1 and later,
        // 38 of its 4-byte counts).
        let mut words = [0u64; 64];
        let mut count = (core::mem::size_of_val(&words) / 4) as u32;
        // SAFETY: the buffer is larger than any `task_vm_info` revision and `count` says so.
        let kr = unsafe {
            task_info(
                mach_task_self_,
                TASK_VM_INFO,
                words.as_mut_ptr().cast(),
                &mut count,
            )
        };
        (kr == 0 && count >= 38).then_some(words[18])
    }
    #[cfg(not(any(target_os = "ios", target_os = "macos")))]
    None
}

/// What iOS will still let this process allocate, in bytes (`os_proc_available_memory`, iOS 13).
fn os_available() -> Option<u64> {
    #[cfg(target_os = "ios")]
    {
        unsafe extern "C" {
            fn os_proc_available_memory() -> usize;
        }
        // SAFETY: a libSystem query with no arguments.
        Some(unsafe { os_proc_available_memory() } as u64)
    }
    #[cfg(not(target_os = "ios"))]
    None
}

/// `$WOW_MEMORY_GOVERNOR=0`: never shed.
fn disabled() -> bool {
    static OFF: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("WOW_MEMORY_GOVERNOR").is_ok_and(|v| v == "0"));
    *OFF
}

/// `$WOW_MEMORY_TRACE=1`: log every reading (10 a second), to see a burst's shape.
fn trace() -> bool {
    static TRACE: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("WOW_MEMORY_TRACE").is_ok_and(|v| v == "1"));
    *TRACE
}

/// The headroom in MB: the OS's figure, the budget's, or the smaller of the two; `None` when off.
fn headroom_mb() -> Option<u64> {
    if disabled() {
        return None;
    }
    let os = os_available().map(|b| b >> 20);
    let budget = budget_mb().and_then(|b| Some(b.saturating_sub(phys_footprint()? >> 20)));
    match (os, budget) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn govern(mut gov: ResMut<MemoryGovernor>, time: Res<Time<Real>>) {
    let now = time.elapsed_secs();
    if now < gov.next_sample {
        return;
    }
    gov.next_sample = now + SAMPLE_SECS;
    let Some(headroom) = headroom_mb() else {
        return;
    };
    HEADROOM_MB.store(headroom, Ordering::Relaxed);
    if trace() {
        let footprint = phys_footprint().map_or(-1, |b| (b >> 20) as i64);
        info!("memory-trace: headroom {headroom} MB, footprint {footprint} MB");
    }
    let was = gov.tier;
    let tier = gov.observe(headroom, now);
    if tier != was {
        TIER.store(tier as u8, Ordering::Relaxed);
        let footprint = phys_footprint().map_or(-1, |b| (b >> 20) as i64);
        if tier > was {
            warn!("memory-governor: {was:?} -> {tier:?} (headroom {headroom} MB, footprint {footprint} MB)");
        } else {
            info!("memory-governor: {was:?} -> {tier:?} (headroom {headroom} MB, footprint {footprint} MB)");
        }
    }
}

/// The view distance under [`MemoryTier::Low`], in yards: a 6-chunk tile window in place of 8.
pub const LOW_FARCLIP: f32 = 130.0;

/// The view distance under [`MemoryTier::Critical`]: a 5-chunk tile window.
pub const CRITICAL_FARCLIP: f32 = 90.0;

/// The view cap this tier asks for, given the one in force: Low and Critical tighten it, Normal
/// lifts it, Tight keeps it.
fn view_cap(tier: MemoryTier, now: Option<f32>) -> Option<f32> {
    let tighten = |cap: f32| Some(now.map_or(cap, |c| c.min(cap)));
    match tier {
        MemoryTier::Normal => None,
        MemoryTier::Tight => now,
        MemoryTier::Low => tighten(LOW_FARCLIP),
        MemoryTier::Critical => tighten(CRITICAL_FARCLIP),
    }
}

/// Deviation: the reference's view distance is the player's `farclip`, never under 177 yd. Short
/// of memory, the world runs at [`view_cap`]'s shorter one (`ViewDistance::governed`), so the
/// streamer releases the tiles past it; the setting itself is untouched.
fn govern_view(gov: Res<MemoryGovernor>, mut view: ResMut<crate::view::ViewDistance>) {
    let cap = view_cap(gov.tier, view.governed);
    if cap != view.governed {
        info!(
            "memory-governor: view distance {} ({:?})",
            cap.map_or("restored".to_string(), |c| format!("capped at {c:.0} yd")),
            gov.tier
        );
        view.governed = cap;
    }
}

fn announce() {
    if disabled() {
        info!("memory-governor: off ($WOW_MEMORY_GOVERNOR=0)");
        return;
    }
    match (os_available().is_some(), budget_mb()) {
        (_, Some(b)) => info!("memory-governor: on, budget {b} MB, floors {FLOORS_MB:?} MB"),
        (true, None) => info!("memory-governor: on, the OS limit, floors {FLOORS_MB:?} MB"),
        (false, None) => {
            info!("memory-governor: off (no OS limit; $WOW_MEMORY_BUDGET_MB sets one)")
        }
    }
}

pub(crate) struct MemoryGovernorPlugin;

impl Plugin for MemoryGovernorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MemoryGovernor>()
            .add_systems(Startup, announce)
            .add_systems(First, (govern, govern_view).chain());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floors_place_each_tier() {
        assert_eq!(MemoryTier::of(400), MemoryTier::Normal);
        assert_eq!(MemoryTier::of(250), MemoryTier::Normal);
        assert_eq!(MemoryTier::of(249), MemoryTier::Tight);
        assert_eq!(MemoryTier::of(149), MemoryTier::Low);
        assert_eq!(MemoryTier::of(10), MemoryTier::Critical);
    }

    /// The view cap tightens under Low and Critical, holds under Tight, and lifts only at Normal.
    #[test]
    fn the_view_cap_latches_until_normal() {
        assert_eq!(view_cap(MemoryTier::Tight, None), None);
        let low = view_cap(MemoryTier::Low, None);
        assert_eq!(low, Some(LOW_FARCLIP));
        assert_eq!(view_cap(MemoryTier::Tight, low), low);
        let crit = view_cap(MemoryTier::Critical, low);
        assert_eq!(crit, Some(CRITICAL_FARCLIP));
        // Back to Low keeps the tighter cap.
        assert_eq!(view_cap(MemoryTier::Low, crit), crit);
        assert_eq!(view_cap(MemoryTier::Normal, crit), None);
        assert!(
            CRITICAL_FARCLIP < LOW_FARCLIP && LOW_FARCLIP < *crate::view::FARCLIP_RANGE.start()
        );
    }

    /// Down at once, even several tiers; back up one tier at a time, each after a clear hold over
    /// its floor plus the margin.
    #[test]
    fn enters_at_once_and_leaves_after_a_clear_hold() {
        let mut g = MemoryGovernor::default();
        assert_eq!(g.observe(100, 0.0), MemoryTier::Low);
        assert_eq!(g.observe(60, 1.0), MemoryTier::Critical);
        // Over Low's raw range but not Critical's floor plus the margin: stays.
        assert_eq!(g.observe(130, 2.0), MemoryTier::Critical);
        assert_eq!(g.observe(400, 3.0), MemoryTier::Critical);
        assert_eq!(g.observe(400, 12.9), MemoryTier::Critical);
        assert_eq!(g.observe(400, 13.0), MemoryTier::Low);
        // A dip under the floor plus margin restarts the hold.
        assert_eq!(g.observe(200, 14.0), MemoryTier::Low);
        assert_eq!(g.observe(400, 15.0), MemoryTier::Low);
        assert_eq!(g.observe(400, 25.0), MemoryTier::Tight);
        assert_eq!(g.observe(400, 35.0), MemoryTier::Normal);
        assert_eq!(g.observe(400, 99.0), MemoryTier::Normal);
    }
}
