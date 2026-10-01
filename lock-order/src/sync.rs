//! `std::sync::{Mutex, RwLock}`, with a class. Same methods, same `LockResult`s; the guards deref
//! to the inner ones. Not bounded by the watchdog: no `.await` can sit under a std guard, and a std
//! guard held across a blocking wait is an order edge like any other.

use std::fmt;
use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::sync::{LockResult, PoisonError, TryLockError, TryLockResult};

use crate::{hooks, watchdog::WaitGuard, Held};

/// Take the lock at once, or register the wait with the watchdog thread and block.
fn take<G>(try_take: TryLockResult<G>, take: impl FnOnce() -> LockResult<G>, class: &'static str, site: &'static Location<'static>, instance: usize) -> LockResult<G> {
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
        map_lock(r, |g| MutexGuard { inner: g, _held: Held(hooks::acquired(self.class, self.instance(), site)) })
    }

    #[track_caller]
    pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
        let site = Location::caller();
        let r = self.inner.try_lock();
        map_try(r, |g| MutexGuard { inner: g, _held: Held(hooks::acquired(self.class, self.instance(), site)) })
    }

    pub fn get_mut(&mut self) -> LockResult<&mut T> {
        self.inner.get_mut()
    }

    pub fn is_poisoned(&self) -> bool {
        self.inner.is_poisoned()
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mutex").field("class", &self.class).field("inner", &&self.inner).finish()
    }
}

pub struct MutexGuard<'a, T: ?Sized> {
    inner: std::sync::MutexGuard<'a, T>,
    _held: Held,
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
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwLock").field("class", &self.class).field("inner", &&self.inner).finish()
    }
}

pub struct RwLockReadGuard<'a, T: ?Sized> {
    inner: std::sync::RwLockReadGuard<'a, T>,
    _held: Held,
}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

pub struct RwLockWriteGuard<'a, T: ?Sized> {
    inner: std::sync::RwLockWriteGuard<'a, T>,
    _held: Held,
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
