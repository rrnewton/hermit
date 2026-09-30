//! `alloc::sync`, `core::sync::atomic`, and spin-backed `Mutex`, `LazyLock`
//! and `OnceLock` with std's signatures. A spin lock cannot be poisoned: a
//! panic while it is held does not unwind here (the kernel target aborts), so
//! `lock` always returns `Ok` and `.lock().unwrap()` never panics.

use core::fmt;
pub use core::sync::atomic;

pub use a::sync::*;

/// std's `PoisonError`; never produced by this facade's locks.
pub struct PoisonError<T> {
    guard: T,
}

impl<T> PoisonError<T> {
    /// Wraps a guard.
    pub fn new(guard: T) -> PoisonError<T> {
        PoisonError { guard }
    }

    /// The guard.
    pub fn into_inner(self) -> T {
        self.guard
    }

    /// The guard, by reference.
    pub fn get_ref(&self) -> &T {
        &self.guard
    }

    /// The guard, by mutable reference.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> fmt::Debug for PoisonError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoisonError").finish_non_exhaustive()
    }
}

impl<T> fmt::Display for PoisonError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("poisoned lock: another task failed inside")
    }
}

impl<T> core::error::Error for PoisonError<T> {}

/// std's `TryLockError`.
pub enum TryLockError<T> {
    /// Never produced by this facade.
    Poisoned(PoisonError<T>),
    /// The lock is held.
    WouldBlock,
}

impl<T> fmt::Debug for TryLockError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryLockError::Poisoned(..) => f.write_str("Poisoned(..)"),
            TryLockError::WouldBlock => f.write_str("WouldBlock"),
        }
    }
}

impl<T> fmt::Display for TryLockError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryLockError::Poisoned(..) => f.write_str("poisoned lock: another task failed inside"),
            TryLockError::WouldBlock => {
                f.write_str("try_lock failed because the operation would block")
            }
        }
    }
}

impl<T> core::error::Error for TryLockError<T> {}

impl<T> From<PoisonError<T>> for TryLockError<T> {
    fn from(err: PoisonError<T>) -> TryLockError<T> {
        TryLockError::Poisoned(err)
    }
}

/// std's `LockResult`.
pub type LockResult<G> = Result<G, PoisonError<G>>;
/// std's `TryLockResult`.
pub type TryLockResult<G> = Result<G, TryLockError<G>>;

/// A spinning mutex with std's `Mutex` signature.
#[derive(Default)]
pub struct Mutex<T: ?Sized> {
    inner: spin::Mutex<T>,
}

/// std's `MutexGuard`.
pub type MutexGuard<'a, T> = spin::MutexGuard<'a, T>;

impl<T> Mutex<T> {
    /// A new unlocked mutex.
    pub const fn new(t: T) -> Mutex<T> {
        Mutex {
            inner: spin::Mutex::new(t),
        }
    }

    /// The protected value.
    pub fn into_inner(self) -> LockResult<T> {
        Ok(self.inner.into_inner())
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Spins until the lock is acquired.
    pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        Ok(self.inner.lock())
    }

    /// Acquires the lock if it is free.
    pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
        self.inner.try_lock().ok_or(TryLockError::WouldBlock)
    }

    /// The protected value, through exclusive access.
    pub fn get_mut(&mut self) -> LockResult<&mut T> {
        Ok(self.inner.get_mut())
    }

    /// Always false.
    pub fn is_poisoned(&self) -> bool {
        false
    }

    /// Nothing to clear.
    pub fn clear_poison(&self) {}
}

impl<T> From<T> for Mutex<T> {
    fn from(t: T) -> Mutex<T> {
        Mutex::new(t)
    }
}

/// serde's impl for std's `Mutex`: the locked value. This lock cannot be
/// poisoned, so the poisoned-lock error serde's impl can return never occurs.
impl<T: ?Sized + serde::Serialize> serde::Serialize for Mutex<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.inner.lock().serialize(serializer)
    }
}

/// serde's impl for std's `Mutex`: a deserialized value, wrapped.
impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for Mutex<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Mutex<T>, D::Error> {
        T::deserialize(deserializer).map(Mutex::new)
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.inner, f)
    }
}

/// std's `LazyLock`: spin's `Lazy` has the same `new`, `force` and `Deref`.
pub type LazyLock<T, F = fn() -> T> = spin::Lazy<T, F>;

/// std's `OnceLock` over spin's `Once`.
pub struct OnceLock<T> {
    inner: spin::Once<T>,
}

impl<T> OnceLock<T> {
    /// An empty cell.
    pub const fn new() -> OnceLock<T> {
        OnceLock {
            inner: spin::Once::new(),
        }
    }

    /// The value, if initialised.
    pub fn get(&self) -> Option<&T> {
        self.inner.get()
    }

    /// The value, if initialised, through exclusive access.
    pub fn get_mut(&mut self) -> Option<&mut T> {
        self.inner.get_mut()
    }

    /// Initialises the cell, or returns `value` if it already was.
    pub fn set(&self, value: T) -> Result<(), T> {
        let mut value = Some(value);
        self.inner.call_once(|| {
            value
                .take()
                .expect("call_once runs its closure at most once")
        });
        match value {
            None => Ok(()),
            Some(value) => Err(value),
        }
    }

    /// The value, initialising it with `f` first if needed.
    pub fn get_or_init<F: FnOnce() -> T>(&self, f: F) -> &T {
        self.inner.call_once(f)
    }

    /// The value, if initialised.
    pub fn into_inner(self) -> Option<T> {
        self.inner.try_into_inner()
    }
}

impl<T> Default for OnceLock<T> {
    fn default() -> OnceLock<T> {
        OnceLock::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for OnceLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.get() {
            Some(v) => f.debug_tuple("OnceLock").field(v).finish(),
            None => f.write_str("OnceLock(<uninit>)"),
        }
    }
}
