//! `std::sync::{Mutex, RwLock, Condvar}`, with a class. Same methods, same `LockResult`s; the
//! guards deref to the inner ones. Not bounded by the watchdog: no `.await` can sit under a std
//! guard, and a std guard held across a blocking wait is an order edge like any other.

use std::fmt;
use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::sync::{LockResult, PoisonError, TryLockError, TryLockResult, WaitTimeoutResult};
use std::time::{Duration, Instant};

use crate::{hooks, watchdog::WaitGuard, Held};

/// Take the lock at once, or register the wait with the watchdog thread and block.
fn take<G>(
    try_take: TryLockResult<G>,
    take: impl FnOnce() -> LockResult<G>,
    class: &'static str,
    site: &'static Location<'static>,
    instance: usize,
) -> LockResult<G> {
    match try_take {
        Ok(g) => Ok(g),
        Err(TryLockError::Poisoned(p)) => Err(p),
        Err(TryLockError::WouldBlock) => {
            let _wait = WaitGuard::begin(class, site, instance);
            take()
        }
    }
}

fn map_lock<G, H>(r: LockResult<G>, wrap: impl FnOnce(G) -> H) -> LockResult<H> {
    match r {
        Ok(g) => Ok(wrap(g)),
        Err(p) => Err(PoisonError::new(wrap(p.into_inner()))),
    }
}

fn map_try<G, H>(r: TryLockResult<G>, wrap: impl FnOnce(G) -> H) -> TryLockResult<H> {
    match r {
        Ok(g) => Ok(wrap(g)),
        Err(TryLockError::Poisoned(p)) => Err(TryLockError::Poisoned(PoisonError::new(wrap(p.into_inner())))),
        Err(TryLockError::WouldBlock) => Err(TryLockError::WouldBlock),
    }
}

/// A `std::sync::Mutex` with a class.
pub struct Mutex<T: ?Sized> {
    class: &'static str,
    inner: std::sync::Mutex<T>,
}

impl<T> Mutex<T> {
    pub const fn new(class: &'static str, value: T) -> Self {
        Mutex { class, inner: std::sync::Mutex::new(value) }
    }

    pub fn into_inner(self) -> LockResult<T> {
        self.inner.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    /// The lock's identity: what the checker and the watchdog name it by. Stable for the lock's
    /// life (a tokio lock's is its inner allocation, so it survives a move of the wrapper).
    pub fn instance(&self) -> usize {
        &self.inner as *const std::sync::Mutex<T> as *const () as usize
    }

    pub fn class(&self) -> &'static str {
        self.class
    }

    #[track_caller]
    pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        let site = Location::caller();
        hooks::attempt(self.class, self.instance(), site);
        let r = take(self.inner.try_lock(), || self.inner.lock(), self.class, site, self.instance());
        map_lock(r, |g| MutexGuard::new(g, self.class, self.instance(), site))
    }

    #[track_caller]
    pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
        let site = Location::caller();
        let r = self.inner.try_lock();
        map_try(r, |g| MutexGuard::new(g, self.class, self.instance(), site))
    }

    pub fn get_mut(&mut self) -> LockResult<&mut T> {
        self.inner.get_mut()
    }

    pub fn is_poisoned(&self) -> bool {
        self.inner.is_poisoned()
    }

    /// The std lock, past the checker — for a test that must see the lock without recording.
    #[cfg(all(test, debug_assertions))]
    pub(crate) fn raw(&self) -> &std::sync::Mutex<T> {
        &self.inner
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mutex").field("class", &self.class).field("inner", &&self.inner).finish()
    }
}

// A guard's fields drop in order: the record (`_held`) goes BEFORE the lock (`inner`), so a thread
// that takes the lock the moment it comes free never finds this holder still on file (review
// finding C-12). The same in every guard here and in `tokio_sync`.
pub struct MutexGuard<'a, T: ?Sized> {
    _held: Held,
    inner: std::sync::MutexGuard<'a, T>,
    // Kept beside the token, which is empty in a release build: a `Condvar` wait gives the token
    // up and takes it back under the same identity.
    class: &'static str,
    instance: usize,
}

impl<'a, T: ?Sized> MutexGuard<'a, T> {
    fn new(inner: std::sync::MutexGuard<'a, T>, class: &'static str, instance: usize, site: &'static Location<'static>) -> Self {
        MutexGuard { inner, _held: Held(hooks::acquired(class, instance, site)), class, instance }
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.inner, f)
    }
}

/// A `std::sync::Condvar` for this module's [`Mutex`] (Store no-hang §14.2).
///
/// lockdep's reading of a condvar wait: the mutex is released and taken again. std re-takes it
/// BEFORE `wait` returns, so a check made on return would come too late — a deadlock in the
/// re-acquisition never returns. So a wait, in order:
///
/// 1. gives up the guard's checker token (the waiter no longer holds the class);
/// 2. checks the re-acquisition's order against everything the thread still holds — the edges
///    are the same at the wake-up, since a thread blocked here takes and releases nothing else;
/// 3. hands the inner guard to std, which unlocks and waits atomically and re-locks on wake;
/// 4. takes the token back under the same class and instance, on a normal and a poisoned return.
///
/// std offers no hook between the notification and the re-acquisition, so the watchdog cannot see
/// a stall there, and `wait_timeout`'s bound covers the wait, not the re-acquisition. Step 2 is
/// what covers it.
///
/// `wait_while` and `wait_timeout_while` loop over `wait` and `wait_timeout`, so their predicate,
/// which runs under the mutex, runs holding the guard's record like any code under it.
#[derive(Debug, Default)]
pub struct Condvar {
    inner: std::sync::Condvar,
}

impl Condvar {
    pub const fn new() -> Self {
        Condvar { inner: std::sync::Condvar::new() }
    }

    pub fn notify_one(&self) {
        self.inner.notify_one();
    }

    pub fn notify_all(&self) {
        self.inner.notify_all();
    }

    #[track_caller]
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> LockResult<MutexGuard<'a, T>> {
        let site = Location::caller();
        let (inner, class, instance) = release_for_wait(guard, site);
        map_lock(self.inner.wait(inner), |g| MutexGuard::new(g, class, instance, site))
    }

    /// std's own `wait_while` is this loop over its `wait`. Written over this wrapper's `wait`, the
    /// predicate — which runs under the mutex — runs holding the guard's record, so a lock it takes
    /// orders after this one and re-taking this one there is reported (review finding X-4); std's
    /// would run it with the record given up.
    #[track_caller]
    pub fn wait_while<'a, T, F>(&self, mut guard: MutexGuard<'a, T>, mut condition: F) -> LockResult<MutexGuard<'a, T>>
    where
        F: FnMut(&mut T) -> bool,
    {
        while condition(&mut *guard) {
            guard = self.wait(guard)?;
        }
        Ok(guard)
    }

    #[track_caller]
    pub fn wait_timeout<'a, T>(&self, guard: MutexGuard<'a, T>, dur: Duration) -> LockResult<(MutexGuard<'a, T>, WaitTimeoutResult)> {
        let site = Location::caller();
        let (inner, class, instance) = release_for_wait(guard, site);
        map_lock(self.inner.wait_timeout(inner, dur), |(g, t)| (MutexGuard::new(g, class, instance, site), t))
    }

    /// std's own loop over its `wait_timeout`, over this wrapper's instead (see [`Self::wait_while`]):
    /// the predicate runs holding the guard's record, and `dur` bounds the whole wait, however
    /// often it is woken.
    #[track_caller]
    pub fn wait_timeout_while<'a, T, F>(
        &self,
        mut guard: MutexGuard<'a, T>,
        dur: Duration,
        mut condition: F,
    ) -> LockResult<(MutexGuard<'a, T>, WaitTimeoutResult)>
    where
        F: FnMut(&mut T) -> bool,
    {
        let start = Instant::now();
        loop {
            if !condition(&mut *guard) {
                return Ok((guard, wait_result(false)));
            }
            let Some(left) = dur.checked_sub(start.elapsed()) else {
                return Ok((guard, wait_result(true)));
            };
            guard = self.wait_timeout(guard, left)?.0;
        }
    }
}

/// A `WaitTimeoutResult` saying `timed_out`. std gives the type no constructor, so a std wait that
/// cannot block mints it: on a mutex of its own, which nothing else can hold, with a zero bound and
/// a constant predicate (`false` returns at once; `true` returns timed out once any time passed).
fn wait_result(timed_out: bool) -> WaitTimeoutResult {
    let m = std::sync::Mutex::new(());
    let cv = std::sync::Condvar::new();
    let g = m.lock().unwrap_or_else(PoisonError::into_inner);
    let (_g, result) = cv.wait_timeout_while(g, Duration::ZERO, |_| timed_out).unwrap_or_else(PoisonError::into_inner);
    debug_assert_eq!(result.timed_out(), timed_out);
    result
}

/// Steps 1 and 2 of a [`Condvar`] wait: give up the token, then check the re-acquisition.
fn release_for_wait<'a, T: ?Sized>(
    guard: MutexGuard<'a, T>,
    site: &'static Location<'static>,
) -> (std::sync::MutexGuard<'a, T>, &'static str, usize) {
    let MutexGuard { inner, _held, class, instance } = guard;
    drop(_held);
    hooks::attempt(class, instance, site);
    (inner, class, instance)
}

/// A `std::sync::RwLock` with a class.
pub struct RwLock<T: ?Sized> {
    class: &'static str,
    inner: std::sync::RwLock<T>,
}

impl<T> RwLock<T> {
    pub const fn new(class: &'static str, value: T) -> Self {
        RwLock { class, inner: std::sync::RwLock::new(value) }
    }

    pub fn into_inner(self) -> LockResult<T> {
        self.inner.into_inner()
    }
}

impl<T: ?Sized> RwLock<T> {
    /// The lock's identity: what the checker and the watchdog name it by. Stable for the lock's
    /// life (a tokio lock's is its inner allocation, so it survives a move of the wrapper).
    pub fn instance(&self) -> usize {
        &self.inner as *const std::sync::RwLock<T> as *const () as usize
    }

    pub fn class(&self) -> &'static str {
        self.class
    }

    #[track_caller]
    pub fn read(&self) -> LockResult<RwLockReadGuard<'_, T>> {
        let site = Location::caller();
        hooks::attempt(self.class, self.instance(), site);
        let r = take(self.inner.try_read(), || self.inner.read(), self.class, site, self.instance());
        map_lock(r, |g| RwLockReadGuard { inner: g, _held: Held(hooks::acquired(self.class, self.instance(), site)) })
    }

    #[track_caller]
    pub fn write(&self) -> LockResult<RwLockWriteGuard<'_, T>> {
        let site = Location::caller();
        hooks::attempt(self.class, self.instance(), site);
        let r = take(self.inner.try_write(), || self.inner.write(), self.class, site, self.instance());
        map_lock(r, |g| RwLockWriteGuard { inner: g, _held: Held(hooks::acquired(self.class, self.instance(), site)) })
    }

    #[track_caller]
    pub fn try_read(&self) -> TryLockResult<RwLockReadGuard<'_, T>> {
        let site = Location::caller();
        let r = self.inner.try_read();
        map_try(r, |g| RwLockReadGuard { inner: g, _held: Held(hooks::acquired(self.class, self.instance(), site)) })
    }

    #[track_caller]
    pub fn try_write(&self) -> TryLockResult<RwLockWriteGuard<'_, T>> {
        let site = Location::caller();
        let r = self.inner.try_write();
        map_try(r, |g| RwLockWriteGuard { inner: g, _held: Held(hooks::acquired(self.class, self.instance(), site)) })
    }

    pub fn get_mut(&mut self) -> LockResult<&mut T> {
        self.inner.get_mut()
    }

    pub fn is_poisoned(&self) -> bool {
        self.inner.is_poisoned()
    }

    /// The std lock, past the checker — for a test that must see the lock without recording.
    #[cfg(all(test, debug_assertions))]
    pub(crate) fn raw(&self) -> &std::sync::RwLock<T> {
        &self.inner
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwLock").field("class", &self.class).field("inner", &&self.inner).finish()
    }
}

pub struct RwLockReadGuard<'a, T: ?Sized> {
    _held: Held, // before `inner`: see `MutexGuard`
    inner: std::sync::RwLockReadGuard<'a, T>,
}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

pub struct RwLockWriteGuard<'a, T: ?Sized> {
    _held: Held, // before `inner`: see `MutexGuard`
    inner: std::sync::RwLockWriteGuard<'a, T>,
}

impl<T: ?Sized> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.inner, f)
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.inner, f)
    }
}
