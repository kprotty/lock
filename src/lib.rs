#[cfg(unix)]
extern crate libc;

use std::fmt;
use std::cell::UnsafeCell;
use std::sync::atomic::{
    Ordering,
    AtomicUsize,
    spin_loop_hint,
};

pub struct Mutex<T: ?Sized> {
    state: AtomicUsize,
    value: UnsafeCell<T>,
}

unsafe impl<T> Send for Mutex<T> {}
unsafe impl<T> Sync for Mutex<T> {}

impl<T> From<T> for Mutex<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", &* self.lock())
    }
}

struct QueueNode {
    next: *mut QueueNode,
    parker: Parker,
}

const SPIN_CPU: usize = 4;
const SPIN_THREAD: usize = 30 - SPIN_CPU;
const SPIN_CPU_COUNT: usize = 30;

const MUTEX_LOCK: usize = 1 << 0;
const QUEUE_LOCK: usize = 1 << 1;
const QUEUE_MASK: usize = !(MUTEX_LOCK | QUEUE_LOCK);

impl<T: Sized> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self { 
            state: AtomicUsize::new(0),
            value: UnsafeCell::new(value),
        }
    }

    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }

    pub fn get_mut(&mut self) -> &mut T {
        return unsafe{ &mut* self.value.get() }
    }

    #[inline]
    pub fn try_lock(&self) -> Option<MutexGuard<T>> {
        self.state
            .compare_exchange_weak(0, MUTEX_LOCK, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| MutexGuard { mutex: &self })
    }

    #[inline]
    pub fn lock(&self) -> MutexGuard<T> {
        self.try_lock().unwrap_or_else(|| {
            self.lock_slow();
            MutexGuard { mutex: &self }
        })
    }

    #[cold]
    fn lock_slow(&self) {
        let mut state = self.state.load(Ordering::Relaxed);
        let mut spin_count = 0;

        loop {
            // try and lock the mutex if its unlocked
            if (state & MUTEX_LOCK) == 0 {
                match self.state.compare_exchange_weak(
                    state,
                    state | MUTEX_LOCK,
                    Ordering::Acquire,
                    Ordering::Relaxed
                ) {
                    Err(new_state) => state = new_state,
                    Ok(_) => return,
                }

            // keep spinning if the queue is empty and haven't spun too much
            } else if (state & QUEUE_MASK) == 0 && spin_count < SPIN_CPU + SPIN_THREAD {
                if spin_count < SPIN_CPU {
                    (0..SPIN_CPU_COUNT).for_each(|_| spin_loop_hint());
                } else {
                    std::thread::yield_now();
                }
                spin_count += 1;
                state = self.state.load(Ordering::Relaxed);

            // time to park, try and add ourselves to the queue, parking right after.
            } else {
                let node = QueueNode {
                    parker: Parker::new(),
                    next: (state & QUEUE_MASK) as *mut _,
                };
                match self.state.compare_exchange_weak(
                    state,
                    (&node as *const _ as usize) | (state & !QUEUE_MASK),
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Err(new_state) => state = new_state,
                    Ok(_) => {
                        node.parker.park();
                        spin_count = 0;
                        state = self.state.load(Ordering::Relaxed);
                    }
                }
            }
        }
    }

    #[inline]
    pub unsafe fn force_unlock(&self) {
        // unlock the queue in a wait-free fashion
        let state = self.state.fetch_sub(MUTEX_LOCK, Ordering::Release);
        
        // if the queue isnt locked and theres a parked node, unpark it
        if (state & QUEUE_LOCK) == 0 && (state & QUEUE_MASK) != 0 {
            self.unlock_slow();
        }
    }

    #[cold]
    fn unlock_slow(&self) {
        // try and acquire the queue lock to unpark a node
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            // if the queue is locked or the queue is empty, stop trying
            if (state & QUEUE_LOCK) != 0 || (state & QUEUE_MASK) == 0 {
                return;
            }
            match self.state.compare_exchange_weak(
                state,
                state | QUEUE_LOCK,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Err(new_state) => state = new_state,
                Ok(_) => break,
            }
        }
        
        // the queue is locked, try and pop a node off the queue to unpark it
        state = self.state.load(Ordering::Relaxed);
        loop {
            // if the mutex is locked, let the unlocker unpark the node
            if (state & MUTEX_LOCK) != 0 {
                match self.state.compare_exchange_weak(
                    state,
                    state & !QUEUE_LOCK,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Err(new_state) => state = new_state,
                    Ok(_) => break,
                }
            
            // the mutex isnt locked, we have the queue lock and theres a parked node in the queue
            } else {
                let node = unsafe { &* ((state & QUEUE_MASK) as *const QueueNode) };
                let next_state = node.next as usize;
                
                // set the state to the node's link which both consumes 
                // the node from the queue and removes QUEUE_LOCK.
                match self.state.compare_exchange_weak(
                    state,
                    next_state,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Err(new_state) => state = new_state,
                    Ok(_) => {
                        node.parker.unpark();
                        break;
                    }
                }
            }
        }
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>
}

unsafe impl<'a, T> Sync for MutexGuard<'a, T> {}

impl<'a, T> Drop for MutexGuard<'a, T> {
    fn drop(&mut self) {
        unsafe { self.mutex.force_unlock() }
    }
}

impl<'a, T: fmt::Debug> fmt::Debug for MutexGuard<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", &*self)
    }
}

impl<'a, T> std::ops::DerefMut for MutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut* self.mutex.value.get() }
    }
}

impl<'a, T> std::ops::Deref for MutexGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &* self.mutex.value.get() }
    }
}

use parker::*;

#[cfg(all(unix, not(target_os = "linux")))]
mod parker {
    use std::cell::{Cell, UnsafeCell};
    use libc::{
        PTHREAD_COND_INITIALIZER,
        pthread_cond_t,
        pthread_cond_destroy,
        pthread_cond_signal,
        pthread_cond_wait,
        PTHREAD_MUTEX_INITIALIZER,
        pthread_mutex_t,
        pthread_mutex_destroy,
        pthread_mutex_lock,
        pthread_mutex_unlock,
        EAGAIN,
    };

    pub struct Parker {
        is_unparked: Cell<bool>,
        cond: UnsafeCell<pthread_cond_t>,
        mutex: UnsafeCell<pthread_mutex_t>,
    }

    impl Drop for Parker {
        fn drop(&mut self) {
            unsafe {
                // looks like dragonfly returns EAGAIN on destroy() for statically initialized pthread structs
                let valid_err = if cfg!(target_os = "dragonfly") { EAGAIN } else { 0 };
                let rc = pthread_mutex_destroy(self.mutex.get());
                debug_assert!(rc == 0 || rc == valid_err);
                let rc = pthread_cond_destroy(self.cond.get());
                debug_assert!(rc == 0 || rc == valid_err);
            }
        }
    }

    impl Parker {
        pub const fn new() -> Self {
            Self {
                is_unparked: Cell::new(false),
                cond: UnsafeCell::new(PTHREAD_COND_INITIALIZER),
                mutex: UnsafeCell::new(PTHREAD_MUTEX_INITIALIZER),
            }
        }

        pub fn unpark(&self) {
            unsafe {
                let ret = pthread_mutex_lock(self.mutex.get());
                debug_assert_eq!(ret, 0);
                if !self.is_unparked.get() {
                    self.is_unparked.set(true);
                    let ret = pthread_cond_signal(self.cond.get());
                    debug_assert_eq!(ret, 0);
                }
                let ret = pthread_mutex_unlock(self.mutex.get());
                debug_assert_eq!(ret, 0);
            }
        }

        pub fn park(&self) {
            unsafe {
                let ret = pthread_mutex_lock(self.mutex.get());
                debug_assert_eq!(ret, 0);
                while !self.is_unparked.get() {
                    let ret = pthread_cond_wait(self.cond.get(), self.mutex.get());
                    debug_assert_eq!(ret, 0);
                }
                let ret = pthread_mutex_unlock(self.mutex.get());
                debug_assert_eq!(ret, 0);
            }
        }
    }
}

#[cfg(any(not(unix), target_os = "linux"))]
mod parker {
    use std::sync::atomic::{AtomicI32, Ordering};

    const EMPTY: i32 = 0;
    const PARKED: i32 = 1;
    const UNPARKED: i32 = 2;

    pub struct Parker {
        state: AtomicI32,
    }

    impl Parker {
        pub const fn new() -> Self {
            Self { state: AtomicI32::new(EMPTY) }
        }

        pub fn unpark(&self) {
            if self.state.swap(UNPARKED, Ordering::Release) == PARKED {
                self.wake();
            }
        }

        pub fn park(&self) {
            let mut state = self.state.load(Ordering::Relaxed);
            while state == EMPTY {
                match self.state.compare_exchange_weak(
                    EMPTY,
                    PARKED,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Err(new_state) => state = new_state,
                    Ok(_) => return self.wait(),
                }
            }
        }

        #[cfg(not(target_os = "linux"))]
        fn spin_wake(&self) {}

        #[cfg(not(target_os = "linux"))]
        fn spin_wait(&self) {
            let mut spin_count: usize = 0;
            while self.state.load(Ordering::Relaxed) == PARKED {
                spin_count = spin_count.wrapping_add(1);
                match spin_count {
                    0..=4   => (0..30).for_each(|_| std::sync::atomic::spin_loop_hint()),
                    4..=10  => std::thread::yield_now(),
                    10..=12 => std::time::sleep(std::time::Duration::from_millis(1)),
                    _       => std::time::sleep(std::time::Duration::from_millis(10)),
                }
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        fn wake(&self) {
            self.spin_wake()
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        fn wait(&self) {
            self.spin_wait()
        }

        #[cfg(target_os = "linux")]
        fn wake(&self) {
            let ret = unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    &self.state as *const _ as *const i32,
                    libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                    1,
                )
            };
            debug_assert!(ret == 0 || ret == 1);
        }

        #[cfg(target_os = "linux")]
        fn wait(&self) {
            while self.state.load(Ordering::Relaxed) == PARKED {
                let ret = unsafe {
                    libc::syscall(
                        libc::SYS_futex,
                        &self.state as *const _ as *const i32,
                        libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                        PARKED,
                        0,
                    )
                };
                debug_assert!(ret == 0 || ret == -1);
            }
        }

        #[cfg(target_os = "windows")]
        fn wake(&self) {
            ntdll::get_event_handle()
                .map(|handle| unsafe {
                    let ret = ntdll::NtReleaseKeyedEvent(handle, &self.state as *const _ as usize, 0, 0);
                    debug_assert_eq!(ret, 0);
                })
                .unwrap_or_else(|| self.spin_wake())
        }

        #[cfg(target_os = "windows")]
        fn wait(&self) {
            ntdll::get_event_handle()
                .map(|handle| unsafe {
                    let ret = ntdll::NtWaitForKeyedEvent(handle, &self.state as *const _ as usize, 0, 0);
                    debug_assert_eq!(ret, 0);
                })
                .unwrap_or_else(|| self.spin_wait())
        }
    }

    #[cfg(target_os = "windows")]
    mod ntdll {
        use std::cell::Cell;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[link(name = "ntdll")]
        extern "stdcall" {
            pub fn NtCreateKeyedEvent(handle: &mut usize, access: u32, attr: u32, flags: u32) -> u32;
            pub fn NtWaitForKeyedEvent(handle: usize, key: usize, alertable: i32, timeout: i64) -> u32;
            pub fn NtReleaseKeyedEvent(handle: usize, key: usize, alertable: i32, timeout: i64) -> u32;
        }

        const UNINITIALIZED: usize = 0;
        const INITIALIZING: usize = 1;
        const INITIALIZED: usize = 2;

        static event_handle: Cell<Option<usize>> = Cell::new(None);
        static event_state: AtomicUsize = AtomicUsize::new(UNINITIALIZED);

        pub fn get_event_handle() -> Option<usize> {
            let mut state = event_state.load(Ordering::Relaxed);
            loop {
                match state {
                    UNINITIALIZED => match event_state.compare_exchange_weak(
                        UNINITIALIZED,
                        INITIALIZING,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    ) {
                        Err(new_state) => state = new_state,
                        Ok(_) => unsafe {
                            let mut handle: usize = 0;
                            const access: u32 = 0x80000000 | 0x40000000;
                            event_handle.set(match NtCreateKeyedEvent(&mut handle, access, 0, 0) {
                                0 => Some(handle),
                                _ => None,
                            });
                            state = INITIALIZED;
                            event_state.store(state, Ordering::Release);
                        }
                    },
                    INITIALIZING => {
                        std::thread::yield_now();
                        state = event_state.load(Ordering::Relaxed);
                    },
                    INITIALIZED => {
                        return event_handle.get();
                    }
                }
            }
        }
    }
}

#[test]
fn test_mutex() {
    use std::sync::{Arc, Barrier};
    use std::thread::{spawn, JoinHandle};

    const NUM_THREADS: usize = 3;
    const NUM_ITERATIONS: usize = 100 * 1000;

    let mutex = Arc::new(Mutex::<u128>::new(0));
    let barrier = Arc::new(Barrier::new(NUM_THREADS));
    (0..NUM_THREADS).map(|_| {
        let mutex = mutex.clone();
        let barrier = barrier.clone();
        spawn(move || {
            barrier.wait();
            for _ in 0..NUM_ITERATIONS {
                let mut count = mutex.lock();
                *count += 1;
            }
        })
    })
    .collect::<Vec<JoinHandle<_>>>()
    .into_iter()
    .for_each(|t| t.join().unwrap());

    assert_eq!(
        *((*mutex).lock()),
        (NUM_THREADS * NUM_ITERATIONS) as u128,
    );
}