//! `tokio::sync::{Mutex, RwLock}`, with a class. Same methods; the guards deref to the inner ones.
//! A contended acquisition registers its wait with the watchdog thread ([`crate::watchdog`]). The inner lock is kept
//! in an `Arc`, so the owned acquisitions (`lock_owned`, `read_owned`, …) take `&self`.

use std::fmt;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::sync::Arc;

pub use tokio::sync::TryLockError;

use crate::{hooks, watchdog, Held};

/// A `tokio::sync::Mutex` with a class.
pub struct Mutex<T: ?Sized> {
    class: &'static str,
    inner: Arc<tokio::sync::Mutex<T>>,
}

impl<T> Mutex<T> {
    pub fn new(class: &'static str, value: T) -> Self {
        Mutex { class, inner: Arc::new(tokio::sync::Mutex::new(value)) }
    }

    /// The value, when no owned guard is alive.
    pub fn into_inner(self) -> T {
        match Arc::try_unwrap(self.inner) {
            Ok(m) => m.into_inner(),
            Err(_) => panic!("lock_order::tokio_sync::Mutex::into_inner while an owned guard is alive"),
        }
    }
}

impl<T: ?Sized> Mutex<T> {
    fn instance(&self) -> usize {
        Arc::as_ptr(&self.inner) as *const () as usize
    }

    pub fn class(&self) -> &'static str {
        self.class
    }

    #[track_caller]
    pub fn lock(&self) -> impl Future<Output = MutexGuard<'_, T>> + '_ {
        let site = Location::caller();
        async move {
            let instance = self.instance();
            hooks::attempt(self.class, instance, site);
            let inner = watchdog::watched(self.class, site, instance, self.inner.lock()).await;
            MutexGuard { inner, _held: Held(hooks::acquired(self.class, instance, site)) }
        }
    }

    #[track_caller]
    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, TryLockError> {
        let site = Location::caller();
        let inner = self.inner.try_lock()?;
        Ok(MutexGuard { inner, _held: Held(hooks::acquired(self.class, self.instance(), site)) })
    }

    pub fn get_mut(&mut self) -> &mut T {
        Arc::get_mut(&mut self.inner).expect("get_mut while an owned guard is alive").get_mut()
    }
}

impl<T: ?Sized + Send + 'static> Mutex<T> {
    #[track_caller]
    pub fn lock_owned(&self) -> impl Future<Output = OwnedMutexGuard<T>> + 'static {
        let site = Location::caller();
        let (class, instance, inner) = (self.class, self.instance(), self.inner.clone());
        async move {
            hooks::attempt(class, instance, site);
            let inner = watchdog::watched(class, site, instance, inner.lock_owned()).await;
            OwnedMutexGuard { inner, held: Held(hooks::acquired(class, instance, site)) }
        }
    }

    #[track_caller]
    pub fn try_lock_owned(&self) -> Result<OwnedMutexGuard<T>, TryLockError> {
        let site = Location::caller();
        let inner = self.inner.clone().try_lock_owned()?;
        Ok(OwnedMutexGuard { inner, held: Held(hooks::acquired(self.class, self.instance(), site)) })
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mutex").field("class", &self.class).field("inner", &&self.inner).finish()
    }
}

pub struct MutexGuard<'a, T: ?Sized> {
    inner: tokio::sync::MutexGuard<'a, T>,
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

pub struct OwnedMutexGuard<T: ?Sized> {
    inner: tokio::sync::OwnedMutexGuard<T>,
    held: Held,
}

impl<T: ?Sized> OwnedMutexGuard<T> {
    /// This guard now held by the context running this — after it was handed to another task
    /// or thread.
    pub fn adopt(&mut self) {
        hooks::adopt(&mut self.held.0);
    }
}

impl<T: ?Sized> Deref for OwnedMutexGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> DerefMut for OwnedMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

/// A `tokio::sync::RwLock` with a class.
pub struct RwLock<T: ?Sized> {
    class: &'static str,
    inner: Arc<tokio::sync::RwLock<T>>,
}

impl<T> RwLock<T> {
    pub fn new(class: &'static str, value: T) -> Self {
        RwLock { class, inner: Arc::new(tokio::sync::RwLock::new(value)) }
    }

    /// The value, when no owned guard is alive.
    pub fn into_inner(self) -> T {
        match Arc::try_unwrap(self.inner) {
            Ok(l) => l.into_inner(),
            Err(_) => panic!("lock_order::tokio_sync::RwLock::into_inner while an owned guard is alive"),
        }
    }
}

impl<T: ?Sized> RwLock<T> {
    /// The lock's identity: the same for every handle to it, and what the checker and the wedge
    /// report name it by.
    pub fn instance(&self) -> usize {
        Arc::as_ptr(&self.inner) as *const () as usize
    }

    pub fn class(&self) -> &'static str {
        self.class
    }

    #[track_caller]
    pub fn read(&self) -> impl Future<Output = RwLockReadGuard<'_, T>> + '_ {
        let site = Location::caller();
        async move {
            let instance = self.instance();
            hooks::attempt(self.class, instance, site);
            let inner = watchdog::watched(self.class, site, instance, self.inner.read()).await;
            RwLockReadGuard { inner, _held: Held(hooks::acquired(self.class, instance, site)) }
        }
    }

    #[track_caller]
    pub fn write(&self) -> impl Future<Output = RwLockWriteGuard<'_, T>> + '_ {
        let site = Location::caller();
        async move {
            let instance = self.instance();
            hooks::attempt(self.class, instance, site);
            let inner = watchdog::watched(self.class, site, instance, self.inner.write()).await;
            RwLockWriteGuard { inner, held: Held(hooks::acquired(self.class, instance, site)) }
        }
    }

    #[track_caller]
    pub fn try_read(&self) -> Result<RwLockReadGuard<'_, T>, TryLockError> {
        let site = Location::caller();
        let inner = self.inner.try_read()?;
        Ok(RwLockReadGuard { inner, _held: Held(hooks::acquired(self.class, self.instance(), site)) })
    }

    #[track_caller]
    pub fn try_write(&self) -> Result<RwLockWriteGuard<'_, T>, TryLockError> {
        let site = Location::caller();
        let inner = self.inner.try_write()?;
        Ok(RwLockWriteGuard { inner, held: Held(hooks::acquired(self.class, self.instance(), site)) })
    }

    pub fn get_mut(&mut self) -> &mut T {
        Arc::get_mut(&mut self.inner).expect("get_mut while an owned guard is alive").get_mut()
    }
}

impl<T: ?Sized + Send + Sync + 'static> RwLock<T> {
    #[track_caller]
    pub fn read_owned(&self) -> impl Future<Output = OwnedRwLockReadGuard<T>> + 'static {
        let site = Location::caller();
        let (class, instance, inner) = (self.class, self.instance(), self.inner.clone());
        async move {
            hooks::attempt(class, instance, site);
            let inner = watchdog::watched(class, site, instance, inner.read_owned()).await;
            OwnedRwLockReadGuard { inner, held: Held(hooks::acquired(class, instance, site)) }
        }
    }

    #[track_caller]
    pub fn write_owned(&self) -> impl Future<Output = OwnedRwLockWriteGuard<T>> + 'static {
        let site = Location::caller();
        let (class, instance, inner) = (self.class, self.instance(), self.inner.clone());
        async move {
            hooks::attempt(class, instance, site);
            let inner = watchdog::watched(class, site, instance, inner.write_owned()).await;
            OwnedRwLockWriteGuard { inner, held: Held(hooks::acquired(class, instance, site)) }
        }
    }

    #[track_caller]
    pub fn try_read_owned(&self) -> Result<OwnedRwLockReadGuard<T>, TryLockError> {
        let site = Location::caller();
        let inner = self.inner.clone().try_read_owned()?;
        Ok(OwnedRwLockReadGuard { inner, held: Held(hooks::acquired(self.class, self.instance(), site)) })
    }

    #[track_caller]
    pub fn try_write_owned(&self) -> Result<OwnedRwLockWriteGuard<T>, TryLockError> {
        let site = Location::caller();
        let inner = self.inner.clone().try_write_owned()?;
        Ok(OwnedRwLockWriteGuard { inner, held: Held(hooks::acquired(self.class, self.instance(), site)) })
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwLock").field("class", &self.class).field("inner", &&self.inner).finish()
    }
}

pub struct RwLockReadGuard<'a, T: ?Sized> {
    inner: tokio::sync::RwLockReadGuard<'a, T>,
    _held: Held,
}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

pub struct RwLockWriteGuard<'a, T: ?Sized> {
    inner: tokio::sync::RwLockWriteGuard<'a, T>,
    held: Held,
}

impl<'a, T: ?Sized> RwLockWriteGuard<'a, T> {
    /// The same lock, now read-held: still held, so no order changes.
    pub fn downgrade(self) -> RwLockReadGuard<'a, T> {
        RwLockReadGuard { inner: tokio::sync::RwLockWriteGuard::downgrade(self.inner), _held: self.held }
    }
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

pub struct OwnedRwLockReadGuard<T: ?Sized> {
    inner: tokio::sync::OwnedRwLockReadGuard<T>,
    held: Held,
}

impl<T: ?Sized> OwnedRwLockReadGuard<T> {
    /// This guard now held by the context running this — after it was handed to another task
    /// or thread (a `spawn_blocking` walk).
    pub fn adopt(&mut self) {
        hooks::adopt(&mut self.held.0);
    }
}

impl<T: ?Sized> Deref for OwnedRwLockReadGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

pub struct OwnedRwLockWriteGuard<T: ?Sized> {
    inner: tokio::sync::OwnedRwLockWriteGuard<T>,
    held: Held,
}

impl<T: ?Sized> OwnedRwLockWriteGuard<T> {
    /// See [`OwnedRwLockReadGuard::adopt`].
    pub fn adopt(&mut self) {
        hooks::adopt(&mut self.held.0);
    }

    /// The same lock, now read-held.
    pub fn downgrade(self) -> OwnedRwLockReadGuard<T> {
        OwnedRwLockReadGuard { inner: tokio::sync::OwnedRwLockWriteGuard::downgrade(self.inner), held: self.held }
    }
}

impl<T: ?Sized> Deref for OwnedRwLockWriteGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> DerefMut for OwnedRwLockWriteGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

macro_rules! debug_guard {
    ($($g:ident),*) => {$(
        impl<T: ?Sized + fmt::Debug> fmt::Debug for $g<'_, T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&*self.inner, f)
            }
        }
    )*};
}
debug_guard!(MutexGuard, RwLockReadGuard, RwLockWriteGuard);

macro_rules! debug_owned_guard {
    ($($g:ident),*) => {$(
        impl<T: ?Sized + fmt::Debug> fmt::Debug for $g<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&*self.inner, f)
            }
        }
    )*};
}
debug_owned_guard!(OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard);
