use core::ops::{Deref, DerefMut};
use core::{alloc::AllocError, fmt};
use core::{
    cell::UnsafeCell,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use spin::Mutex as Spinlock;

use crate::slpque::SleepQueue;

struct Inner {
    locked: bool,
    slpque: SleepQueue,
}

/// An async mutex.
///
/// The locking mechanism uses a FIFO wait queue to avoid starvation.
///
/// # Examples
///
/// ```
/// # ksched::sched::spawn(async {
/// use ksched::mutex::Mutex;
///
/// let m: Mutex<usize> = Mutex::new(1);
///
/// let mut guard = m.lock().await.expect("oom");
/// *guard = 2;
/// drop(guard);
///
/// let guard = m.lock().await.expect("oom");
/// assert_eq!(*guard, 2);
/// # });
/// # ksched::sched::run_all();
/// ```
pub struct Mutex<T: ?Sized> {
    /// Guard towards status and waiting queue.
    inner: Spinlock<Inner>,

    /// The value inside the mutex.
    data: UnsafeCell<T>,
}

unsafe impl<T: Send + ?Sized> Send for Mutex<T> {}
unsafe impl<T: Send + ?Sized> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// Creates a new async mutex.
    ///
    /// # Examples
    ///
    /// ```
    /// use ksched::mutex::Mutex;
    ///
    /// let mutex: Mutex<usize> = Mutex::new(0);
    /// ```
    pub fn new(data: T) -> Mutex<T> {
        Mutex {
            inner: Spinlock::new(Inner {
                locked: false,
                slpque: SleepQueue::new(),
            }),
            data: UnsafeCell::new(data),
        }
    }

    /// Consumes the mutex, returning the underlying data.
    ///
    /// # Examples
    ///
    /// ```
    /// use ksched::mutex::Mutex;
    ///
    /// let mutex: Mutex<usize> = Mutex::new(10);
    /// assert_eq!(mutex.into_inner(), 10);
    /// ```
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Acquires the mutex.
    ///
    /// Since inserting current task to the wait queue requires memory
    /// allocation, this function may return [AllocError] on oom.
    /// Otherwise, returns a guard that releases the mutex when dropped.
    ///
    /// # Examples
    ///
    /// ```
    /// # ksched::sched::spawn(async {
    /// use ksched::mutex::Mutex;
    ///
    /// let mutex: Mutex<usize> = Mutex::new(10);
    /// let guard = mutex.lock().await.expect("oom");
    /// assert_eq!(*guard, 10);
    /// # });
    /// # ksched::sched::run_all();
    /// ```
    #[inline]
    pub async fn lock(&self) -> Result<MutexGuard<'_, T>, AllocError> {
        struct Lock<'a, T: ?Sized> {
            mutex: &'a Mutex<T>,
        }

        impl<'a, T: ?Sized> Future for Lock<'a, T> {
            type Output = Result<MutexGuard<'a, T>, AllocError>;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let mut g = self.mutex.inner.lock();
                let result = if g.locked {
                    match g.slpque.sleep(cx.waker().clone()) {
                        Ok(_) => Poll::Pending,
                        Err(_) => Poll::Ready(Err(AllocError)),
                    }
                } else {
                    g.locked = true;
                    Poll::Ready(Ok(MutexGuard(self.mutex)))
                };
                result
            }
        }

        Lock { mutex: self }.await
    }

    /// Attempts to acquire the mutex.
    ///
    /// If the mutex could not be acquired at this time, then [`None`] is returned. Otherwise, a
    /// guard is returned that releases the mutex when dropped.
    ///
    /// # Examples
    ///
    /// ```
    /// use ksched::sync::Mutex;
    ///
    /// let mutex = Mutex::new(10);
    /// if let Some(guard) = mutex.try_lock() {
    ///     assert_eq!(*guard, 10);
    /// }
    /// # ;
    /// ```
    #[inline]
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        if let Some(mut g) = self.inner.try_lock() {
            g.locked = true;
            Some(MutexGuard(self))
        } else {
            None
        }
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the mutex mutably, no actual locking takes place -- the mutable
    /// borrow statically guarantees the mutex is not already acquired.
    ///
    /// # Examples
    ///
    /// ```
    /// # ksched::sched::spawn(async {
    /// use ksched::mutex::Mutex;
    ///
    /// let mut mutex: Mutex<usize> = Mutex::new(0);
    /// *mutex.get_mut() = 10;
    /// assert_eq!(*mutex.lock().await.expect("oom"), 10);
    /// # });
    /// # ksched::sched::run_all();
    /// ```
    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.data.get() }
    }
}

impl<T: fmt::Debug + ?Sized> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Locked;
        impl fmt::Debug for Locked {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("<locked>")
            }
        }

        match self.try_lock() {
            None => f.debug_struct("Mutex").field("data", &Locked).finish(),
            Some(guard) => f.debug_struct("Mutex").field("data", &&*guard).finish(),
        }
    }
}

impl<T> From<T> for Mutex<T> {
    fn from(val: T) -> Mutex<T> {
        Mutex::new(val)
    }
}

impl<T: Default + ?Sized> Default for Mutex<T> {
    fn default() -> Mutex<T> {
        Mutex::new(Default::default())
    }
}

/// A guard that releases the mutex when dropped.
pub struct MutexGuard<'a, T: ?Sized>(&'a Mutex<T>);

unsafe impl<T: Send + ?Sized> Send for MutexGuard<'_, T> {}
unsafe impl<T: Sync + ?Sized> Sync for MutexGuard<'_, T> {}

impl<'a, T: ?Sized> MutexGuard<'a, T> {
    /// Returns a reference to the mutex a guard came from.
    ///
    /// # Examples
    ///
    /// ```
    /// # ksched::sched::spawn(async {
    /// use ksched::sync::{Mutex, MutexGuard};
    ///
    /// let mutex = Mutex::new(10i32);
    /// let guard = mutex.lock().await.expect("oom");
    /// dbg!(MutexGuard::source(&guard));
    /// # }).expect("oom");
    /// # ksched::sched::run_all();
    /// ```
    pub fn source(guard: &MutexGuard<'a, T>) -> &'a Mutex<T> {
        guard.0
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        // Notify waiters.
        let mut g = self.0.inner.lock();
        g.slpque.wakeup_one();
        g.locked = false;

        #[cfg(test)]
        println!("mutex guard drop");
    }
}

impl<T: fmt::Debug + ?Sized> fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: fmt::Display + ?Sized> fmt::Display for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.0.data.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.0.data.get() }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;
    use crate::sched::{run_all, spawn};
    use crate::yield_now::yield_now;

    #[test]
    fn test_mutex() {
        const N: usize = 100;
        const NCPU: usize = 4;
        let data: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        for i in 0..N {
            let data = data.clone();
            spawn(async move {
                println!("task {}: start", i);
                let mut lk = data.lock().await.unwrap();
                yield_now().await;
                *lk += 1;
                yield_now().await;
                println!("task {}: end", i);
            })
            .unwrap();
        }

        let mut handles = vec![];
        for _ in 0..NCPU {
            let data = data.clone();
            handles.push(thread::spawn(|| {
                run_all();
                spawn(async move {
                    let g = data.lock().await.unwrap();
                    assert_eq!(*g, N);
                })
                .unwrap();
                run_all();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }
}
