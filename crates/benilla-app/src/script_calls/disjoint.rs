//! Parameter sets whose members may conflict with one another, each member's access gathered once.
//! Bevy's `ParamSet` gathers each member's access twice, the first time on a clone of the access
//! the system has built so far, so a set nested `d` deep gathers its leaves `2^d` times; the call
//! host's appliers are one flat set of these ([`disjoint_set!`]), and a system that holds the host
//! beside state of its own pairs them in a [`Pair`].

use bevy::ecs::change_detection::Tick;
use bevy::ecs::query::FilteredAccessSet;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{SystemMeta, SystemParam, SystemParamItem, SystemParamValidationError};
use bevy::ecs::world::unsafe_world_cell::UnsafeWorldCell;
use bevy::ecs::world::{DeferredWorld, World};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

/// Gather the access of `P`, one member of a set, alone, check it against what the system holds
/// outside the set, as `ParamSet` does, and add it to `gathered`. Members may conflict with one
/// another: the set lends one at a time.
pub(crate) fn gather<P: SystemParam>(
    state: &P::State,
    meta: &mut SystemMeta,
    outside: &FilteredAccessSet,
    gathered: &mut FilteredAccessSet,
    world: &mut World,
    member: &str,
) {
    let mut alone = FilteredAccessSet::new();
    P::init_access(state, meta, &mut alone, world);
    assert!(
        outside.is_compatible(&alone),
        "{member} in {} conflicts with a parameter outside its set: {:?}",
        meta.name(),
        outside.get_conflicts(&alone),
    );
    gathered.extend(alone);
}

/// Where a set keeps its member states ([`SetKind`]): `Flag<false>` in each holding system,
/// `Flag<true>` (a `shared` set) as one copy per world that every holder lends in turn. The call
/// host's appliers are held by every system that runs Lua, and each copy of their states holds
/// hundreds of queries, each with its own access bitsets and its own list of the archetypes it
/// matches, grown by every new archetype. Their members keep nothing of a holder's own (no
/// `Commands`, no message cursors; their `Local`s are memos keyed on what they read), so one copy
/// answers every holder alike. Holders never run at once (the set writes, so Bevy orders them
/// apart), and a lend while another is out panics.
pub(crate) struct Flag<const SHARED: bool>;

/// How a set keeps and lends its member states ([`Flag`]).
pub(crate) trait SetKind {
    type States<T: Send + Sync + 'static>: Send + Sync + 'static;
    type Lend<'s, T: Send + Sync + 'static>: DerefMut<Target = T>;
    type Peek<'s, T: Send + Sync + 'static>: Deref<Target = T>;
    /// The states for a new holder, built by `make` where none are kept yet.
    fn init<T: Send + Sync + 'static>(
        world: &mut World,
        make: impl FnOnce(&mut World) -> T,
    ) -> Self::States<T>;
    /// The states, lent to this holder alone while the lend lives.
    fn lend<T: Send + Sync + 'static>(states: &mut Self::States<T>) -> Self::Lend<'_, T>;
    /// The states, read.
    fn peek<T: Send + Sync + 'static>(states: &Self::States<T>) -> Self::Peek<'_, T>;
}

impl SetKind for Flag<false> {
    type States<T: Send + Sync + 'static> = T;
    type Lend<'s, T: Send + Sync + 'static> = &'s mut T;
    type Peek<'s, T: Send + Sync + 'static> = &'s T;
    fn init<T: Send + Sync + 'static>(world: &mut World, make: impl FnOnce(&mut World) -> T) -> T {
        make(world)
    }
    fn lend<T: Send + Sync + 'static>(states: &mut T) -> &mut T {
        states
    }
    fn peek<T: Send + Sync + 'static>(states: &T) -> &T {
        states
    }
}

/// The one copy of a shared set's states in a world.
#[derive(Resource)]
struct SharedStates<T: Send + 'static>(Arc<Mutex<T>>);

impl SetKind for Flag<true> {
    type States<T: Send + Sync + 'static> = Arc<Mutex<T>>;
    type Lend<'s, T: Send + Sync + 'static> = MutexGuard<'s, T>;
    type Peek<'s, T: Send + Sync + 'static> = MutexGuard<'s, T>;
    fn init<T: Send + Sync + 'static>(
        world: &mut World,
        make: impl FnOnce(&mut World) -> T,
    ) -> Arc<Mutex<T>> {
        if let Some(copy) = world.get_resource::<SharedStates<T>>() {
            return copy.0.clone();
        }
        let copy = Arc::new(Mutex::new(make(world)));
        world.insert_resource(SharedStates(copy.clone()));
        copy
    }
    fn lend<T: Send + Sync + 'static>(states: &mut Arc<Mutex<T>>) -> MutexGuard<'_, T> {
        lock(states)
    }
    fn peek<T: Send + Sync + 'static>(states: &Arc<Mutex<T>>) -> MutexGuard<'_, T> {
        lock(states)
    }
}

/// A shared set's copy, which no other holder has out: two at once would be one system's
/// states lent twice.
fn lock<T>(copy: &Mutex<T>) -> MutexGuard<'_, T> {
    match copy.try_lock() {
        Ok(states) => states,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            panic!("a shared set's states lent to two holders at once")
        }
    }
}

/// A set of named members that may conflict with one another, lent one at a time through a
/// method of the member's name, each member's access gathered once ([`gather`]). Each member's
/// type is written with `'static` lifetimes; its method lends it for the borrow of the set. A
/// member may carry one `cfg` after its docs. After the state's name, `watched` marks a set the
/// census watches, and `; shared` (after any `watched`) one whose holders share one copy of its
/// member states ([`Flag`]).
macro_rules! disjoint_set {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident, $state:ident $(, $watched:ident)? $(; $shared:ident)? {
            $($(#[doc = $doc:literal])* $(#[cfg($cfg:meta)])? $field:ident: $ty:ty,)*
        }
    ) => {
        $(#[$meta])*
        $vis struct $name<'w, 's> {
            states: <crate::script_calls::disjoint::Flag<{ $state::SHARED }> as crate::script_calls::disjoint::SetKind>::Lend<'s, $state>,
            meta: bevy::ecs::system::SystemMeta,
            world: bevy::ecs::world::unsafe_world_cell::UnsafeWorldCell<'w>,
            change_tick: bevy::ecs::change_detection::Tick,
            /// The tick and member of the lend running now, while the census watches writes.
            #[cfg(debug_assertions)]
            lend: Option<(bevy::ecs::change_detection::Tick, &'static str)>,
        }

        #[doc(hidden)]
        $vis struct $state {
            $($(#[cfg($cfg)])? $field: <$ty as bevy::ecs::system::SystemParam>::State,)*
        }

        impl $state {
            /// Whether every holder lends one copy of these states ([`crate::script_calls::disjoint::Flag`]).
            const SHARED: bool = false $(|| { let _ = stringify!($shared); true })?;
        }

        // SAFETY: the set registers every member's access, each checked against what the system
        // holds outside the set ([`crate::script_calls::disjoint::gather`]), and lends one member
        // at a time, through `&mut self`.
        unsafe impl bevy::ecs::system::SystemParam for $name<'_, '_> {
            type State = <crate::script_calls::disjoint::Flag<{ $state::SHARED }> as crate::script_calls::disjoint::SetKind>::States<$state>;
            type Item<'w, 's> = $name<'w, 's>;

            fn init_state(world: &mut bevy::ecs::world::World) -> Self::State {
                <crate::script_calls::disjoint::Flag<{ $state::SHARED }> as crate::script_calls::disjoint::SetKind>::init(world, |world| $state {
                    $(
                        $(#[cfg($cfg)])?
                        $field: <$ty as bevy::ecs::system::SystemParam>::init_state(world),
                    )*
                })
            }

            fn init_access(
                state: &Self::State,
                meta: &mut bevy::ecs::system::SystemMeta,
                access: &mut bevy::ecs::query::FilteredAccessSet,
                world: &mut bevy::ecs::world::World,
            ) {
                let state = &*<crate::script_calls::disjoint::Flag<{ $state::SHARED }> as crate::script_calls::disjoint::SetKind>::peek(state);
                let mut gathered = bevy::ecs::query::FilteredAccessSet::new();
                $(
                    $(#[cfg($cfg)])?
                    crate::script_calls::disjoint::gather::<$ty>(
                        &state.$field,
                        meta,
                        access,
                        &mut gathered,
                        world,
                        stringify!($field),
                    );
                )*
                access.extend(gathered);
            }

            fn apply(
                state: &mut Self::State,
                meta: &bevy::ecs::system::SystemMeta,
                world: &mut bevy::ecs::world::World,
            ) {
                let state = &mut *<crate::script_calls::disjoint::Flag<{ $state::SHARED }> as crate::script_calls::disjoint::SetKind>::lend(state);
                $(
                    $(#[cfg($cfg)])?
                    <$ty as bevy::ecs::system::SystemParam>::apply(&mut state.$field, meta, world);
                )*
            }

            fn queue(
                state: &mut Self::State,
                meta: &bevy::ecs::system::SystemMeta,
                mut world: bevy::ecs::world::DeferredWorld,
            ) {
                let state = &mut *<crate::script_calls::disjoint::Flag<{ $state::SHARED }> as crate::script_calls::disjoint::SetKind>::lend(state);
                $($(#[cfg($cfg)])? <$ty as bevy::ecs::system::SystemParam>::queue(
                    &mut state.$field,
                    meta,
                    world.reborrow(),
                );)*
            }

            unsafe fn validate_param(
                state: &mut Self::State,
                meta: &bevy::ecs::system::SystemMeta,
                world: bevy::ecs::world::unsafe_world_cell::UnsafeWorldCell,
            ) -> Result<(), bevy::ecs::system::SystemParamValidationError> {
                let state = &mut *<crate::script_calls::disjoint::Flag<{ $state::SHARED }> as crate::script_calls::disjoint::SetKind>::lend(state);
                // SAFETY: upheld by the caller, for every member's access as for the set's.
                unsafe {
                    $($(#[cfg($cfg)])? <$ty as bevy::ecs::system::SystemParam>::validate_param(
                        &mut state.$field,
                        meta,
                        world,
                    )?;)*
                }
                Ok(())
            }

            unsafe fn get_param<'w, 's>(
                state: &'s mut Self::State,
                meta: &bevy::ecs::system::SystemMeta,
                world: bevy::ecs::world::unsafe_world_cell::UnsafeWorldCell<'w>,
                change_tick: bevy::ecs::change_detection::Tick,
            ) -> Self::Item<'w, 's> {
                $name {
                    states: <crate::script_calls::disjoint::Flag<{ $state::SHARED }> as crate::script_calls::disjoint::SetKind>::lend(state),
                    meta: meta.clone(),
                    world,
                    change_tick,
                    #[cfg(debug_assertions)]
                    lend: None,
                }
            }
        }

        impl $name<'_, '_> {
            /// Whether the census watches this set's lends: one marked `watched`, the state a call
            /// writes, and not a set of reads.
            #[cfg(debug_assertions)]
            const WATCHED: bool = false $(|| { let _ = stringify!($watched); true })?;

            /// The lend that ended with the last member's item has written or not, for the census
            /// ([`crate::script_calls::disjoint::end_lend`]): the host calls this where its
            /// members are all unlent, such as the end of an apply and ahead of a signal point.
            #[cfg(debug_assertions)]
            #[allow(dead_code)]
            pub(crate) fn end_lend(&mut self) {
                crate::script_calls::disjoint::end_lend(self.world, &mut self.lend);
            }

            $(
                $(#[doc = $doc])*
                $(#[cfg($cfg)])?
                pub(crate) fn $field(
                    &mut self,
                ) -> bevy::ecs::system::SystemParamItem<'_, '_, $ty> {
                    #[cfg(debug_assertions)]
                    let change_tick = if Self::WATCHED {
                        crate::script_calls::disjoint::begin_lend(
                            self.world,
                            &mut self.lend,
                            self.change_tick,
                            stringify!($field),
                        )
                    } else {
                        self.change_tick
                    };
                    #[cfg(not(debug_assertions))]
                    let change_tick = self.change_tick;
                    // SAFETY: the set registered this member's access, and `&mut self` keeps
                    // every other member of the set unlent while this one lives.
                    unsafe {
                        <$ty as bevy::ecs::system::SystemParam>::get_param(
                            &mut self.states.$field,
                            &self.meta,
                            self.world,
                            change_tick,
                        )
                    }
                }
            )*
        }
    };
}
pub(crate) use disjoint_set;

/// Two params that may conflict, lent one at a time, as a two-member `ParamSet` with each
/// member's access gathered once ([`gather`]).
pub(crate) struct Pair<'w, 's, A: SystemParam, B: SystemParam> {
    states: &'s mut (A::State, B::State),
    meta: SystemMeta,
    world: UnsafeWorldCell<'w>,
    change_tick: Tick,
    /// The tick and name of the first member's lend running now, while the census watches writes.
    #[cfg(debug_assertions)]
    lend: Option<(Tick, &'static str)>,
}

// SAFETY: as [`disjoint_set!`]'s: both members' access registered and checked against what the
// system holds outside the pair, one member lent at a time.
unsafe impl<A: SystemParam, B: SystemParam> SystemParam for Pair<'_, '_, A, B> {
    type State = (A::State, B::State);
    type Item<'w, 's> = Pair<'w, 's, A, B>;

    fn init_state(world: &mut World) -> Self::State {
        (A::init_state(world), B::init_state(world))
    }

    fn init_access(
        state: &Self::State,
        meta: &mut SystemMeta,
        access: &mut FilteredAccessSet,
        world: &mut World,
    ) {
        let mut gathered = FilteredAccessSet::new();
        gather::<A>(&state.0, meta, access, &mut gathered, world, "p0");
        gather::<B>(&state.1, meta, access, &mut gathered, world, "p1");
        access.extend(gathered);
    }

    fn apply(state: &mut Self::State, meta: &SystemMeta, world: &mut World) {
        A::apply(&mut state.0, meta, world);
        B::apply(&mut state.1, meta, world);
    }

    fn queue(state: &mut Self::State, meta: &SystemMeta, mut world: DeferredWorld) {
        A::queue(&mut state.0, meta, world.reborrow());
        B::queue(&mut state.1, meta, world.reborrow());
    }

    unsafe fn validate_param(
        state: &mut Self::State,
        meta: &SystemMeta,
        world: UnsafeWorldCell,
    ) -> Result<(), SystemParamValidationError> {
        // SAFETY: upheld by the caller, for each member's access as for the pair's.
        unsafe {
            A::validate_param(&mut state.0, meta, world)?;
            B::validate_param(&mut state.1, meta, world)
        }
    }

    unsafe fn get_param<'w, 's>(
        state: &'s mut Self::State,
        meta: &SystemMeta,
        world: UnsafeWorldCell<'w>,
        change_tick: Tick,
    ) -> Self::Item<'w, 's> {
        Pair {
            states: state,
            meta: meta.clone(),
            world,
            change_tick,
            #[cfg(debug_assertions)]
            lend: None,
        }
    }
}

impl<A: SystemParam, B: SystemParam> Pair<'_, '_, A, B> {
    /// The first member, the second unlent while it lives.
    pub(crate) fn p0(&mut self) -> SystemParamItem<'_, '_, A> {
        #[cfg(debug_assertions)]
        let change_tick = begin_lend(self.world, &mut self.lend, self.change_tick, "p0");
        #[cfg(not(debug_assertions))]
        let change_tick = self.change_tick;
        // SAFETY: the pair registered `A`'s access, and `&mut self` keeps `B` unlent.
        unsafe { A::get_param(&mut self.states.0, &self.meta, self.world, change_tick) }
    }

    /// The first member's lend has ended, for the census ([`end_lend`]): its item has dropped.
    #[cfg(debug_assertions)]
    pub(crate) fn end_lend(&mut self) {
        end_lend(self.world, &mut self.lend);
    }

    /// The second member, the first unlent while it lives.
    pub(crate) fn p1(&mut self) -> SystemParamItem<'_, '_, B> {
        // SAFETY: the pair registered `B`'s access, and `&mut self` keeps `A` unlent.
        unsafe { B::get_param(&mut self.states.1, &self.meta, self.world, self.change_tick) }
    }
}

/// Begin a lend of a member, for the census: the one before it is ended, and while the census
/// watches writes ([`benilla_ui::script::census::watching_writes`]) the lend takes a change tick
/// of its own, which only its writes carry, to be found by [`end_lend`]. `system` is the tick the
/// system runs at, which every lend carries otherwise.
#[cfg(debug_assertions)]
pub(crate) fn begin_lend(
    world: UnsafeWorldCell,
    lend: &mut Option<(Tick, &'static str)>,
    system: Tick,
    member: &'static str,
) -> Tick {
    use benilla_ui::script::census;
    end_lend(world, lend);
    if !census::watching_writes() {
        return system;
    }
    census::lend_begins();
    let tick = world.increment_change_tick();
    *lend = Some((tick, member));
    tick
}

/// End the lend begun last, for the census: when a handler is held, it reports whether the lend
/// wrote, and a write that follows the handler's signal panics, unless the member only publishes
/// ([`super::census::EPILOGUE`]). Every item the lend gave has dropped: the lends are `&mut`
/// borrows of the set, and the next begins, or the host ends this one, only once they have ended.
#[cfg(debug_assertions)]
pub(crate) fn end_lend(world: UnsafeWorldCell, lend: &mut Option<(Tick, &'static str)>) {
    use benilla_ui::script::census;
    let Some((tick, member)) = lend.take() else {
        return;
    };
    let after = census::lend_ends(census::window_open() && written_at(world, tick), member);
    if after && !super::census::EPILOGUE.contains(&member) && !std::thread::panicking() {
        panic!(
            "census: `{member}` written after a handler its apply signalled was held, which the \
             reference runs before the write: `SignalEvent` (0x703e50) runs every listener before \
             the verb goes on"
        );
    }
}

/// Whether anything in the world carries `tick` as its change tick: a resource, or a table
/// component of any entity. A write through a `ResMut` or a `Mut` stamps the tick its lend was
/// given, so a lend with a tick of its own wrote exactly if something carries it. Commands
/// queued by the lend and sparse-set components are not seen.
#[cfg(debug_assertions)]
fn written_at(world: UnsafeWorldCell, tick: Tick) -> bool {
    // SAFETY: only the change ticks are read, as plain values, with no member of the set lent
    // (the caller's contract above) and no other thread running.
    let storages = unsafe { world.storages() };
    let resources = storages
        .resources
        .iter()
        .filter_map(|(_, r)| r.get_ticks())
        .chain(
            storages
                .non_send_resources
                .iter()
                .filter_map(|(_, r)| r.get_ticks()),
        );
    for ticks in resources {
        if ticks.changed.get() == tick.get() {
            return true;
        }
    }
    let components = world.components();
    storages.tables.iter().any(|table| {
        components.iter_registered().any(|info| {
            table
                .get_changed_ticks_slice_for(info.id())
                // SAFETY: as above.
                .is_some_and(|ticks| {
                    ticks
                        .iter()
                        .any(|t| unsafe { *t.get() }.get() == tick.get())
                })
        })
    })
}

#[cfg(test)]
mod tests {
    use bevy::prelude::*;

    use super::Pair;

    #[derive(Resource, Default)]
    struct Cell(u32);

    /// The members of a pair may conflict with one another, as the pair lends one at a time.
    #[test]
    fn a_pairs_members_may_conflict_with_each_other() {
        let mut app = App::new();
        app.init_resource::<Cell>().add_systems(
            Update,
            |mut pair: Pair<ResMut<'static, Cell>, Res<'static, Cell>>| {
                pair.p0().0 += 1;
                assert_eq!(pair.p1().0, 1);
            },
        );
        app.update();
    }

    /// A member that conflicts with a parameter outside its set is refused as the system's state
    /// is built, as `ParamSet` refuses it.
    #[test]
    #[should_panic(expected = "conflicts with a parameter outside its set")]
    fn a_member_conflicting_with_a_parameter_outside_the_set_panics() {
        let mut app = App::new();
        app.init_resource::<Cell>().add_systems(
            Update,
            |_outside: ResMut<Cell>, _pair: Pair<Res<'static, Cell>, Res<'static, Cell>>| {},
        );
        app.update();
    }

    #[derive(bevy::ecs::system::SystemParam)]
    struct Count<'s> {
        n: Local<'s, u32>,
    }

    super::disjoint_set! {
        struct OwnSet, OwnSetState {
            count: Count<'static>,
        }
    }

    super::disjoint_set! {
        struct SharedSet, SharedSetState; shared {
            count: Count<'static>,
        }
    }

    fn own_bump(mut set: OwnSet) -> u32 {
        let mut count = set.count();
        *count.n += 1;
        *count.n
    }

    fn shared_bump(mut set: SharedSet) -> u32 {
        let mut count = set.count();
        *count.n += 1;
        *count.n
    }

    /// What two systems of `bump` read, run in turn twice.
    fn reads<M>(bump: impl IntoSystem<(), u32, M> + Copy) -> Vec<u32> {
        let mut world = World::new();
        let mut a = IntoSystem::into_system(bump);
        let mut b = IntoSystem::into_system(bump);
        a.initialize(&mut world);
        b.initialize(&mut world);
        let mut seen = Vec::new();
        for _ in 0..2 {
            seen.push(a.run((), &mut world).unwrap());
            seen.push(b.run((), &mut world).unwrap());
        }
        seen
    }

    #[test]
    fn a_shared_sets_holders_lend_one_copy_and_an_own_sets_each_their_own() {
        assert_eq!(reads(own_bump), [1, 1, 2, 2]);
        assert_eq!(reads(shared_bump), [1, 2, 3, 4]);
    }
}
