use std::sync::{Arc, Barrier};
use criterion::{
    Criterion,
    criterion_main,
    criterion_group,
};

trait Lock<T> {
    fn name() -> &'static str;
    fn new(value: T) -> Self;
    fn lock<F, R>(&self, f: F) -> R where F: FnOnce(&mut T) -> R;
}

impl<T> Lock<T> for custom_lock::Mutex<T> {
    fn name() -> &'static str {
        "custom_lock::Mutex"
    }
    fn new(value: T) -> Self {
        Self::new(value)
    }
    fn lock<F, R>(&self, f: F) -> R where F: FnOnce(&mut T) -> R {
        f(&mut *self.lock())
    }
}

impl<T> Lock<T> for std::sync::Mutex<T> {
    fn name() -> &'static str {
        "std::sync::Mutex"
    }
    fn new(value: T) -> Self {
        Self::new(value)
    }
    fn lock<F, R>(&self, f: F) -> R where F: FnOnce(&mut T) -> R {
        f(&mut *self.lock().unwrap())
    }
}

impl<T> Lock<T> for parking_lot::Mutex<T> {
    fn name() -> &'static str {
        "parking_lot::Mutex"
    }
    fn new(value: T) -> Self {
        Self::new(value)
    }
    fn lock<F, R>(&self, f: F) -> R where F: FnOnce(&mut T) -> R {
        f(&mut *self.lock())
    }
}

fn bench_mutex<L: Lock<u128> + Send + Sync + 'static>(iters: usize, c: &mut Criterion) {
    let num_threads = num_cpus::get();
    c.bench_function(&format!("{} - {}", L::name(), iters), |b| b.iter(|| {
        let mutex = Arc::new(L::new(0));
        let barrier = Arc::new(Barrier::new(num_threads));
        (0..num_threads).map(|_| {
            let barrier = barrier.clone();
            let mutex = mutex.clone();
            std::thread::spawn(move || {
                barrier.wait();
                while mutex.lock(|count| {
                    if *count == iters as u128 {
                        false
                    } else {
                        *count += 1;
                        true
                    }
                }) {}
            })
        })
        .collect::<Vec<std::thread::JoinHandle<_>>>()
        .into_iter()
        .for_each(|t| t.join().unwrap());
        assert_eq!((*mutex).lock(|count| *count), iters as u128);
    }));
}

fn criterion_benchmark(c: &mut Criterion) {
    for iter in [
        1000,
        10 * 1000,
    ].iter() {
        bench_mutex::<std::sync::Mutex<u128>>(*iter, c);
        bench_mutex::<custom_lock::Mutex<u128>>(*iter, c);
        bench_mutex::<parking_lot::Mutex<u128>>(*iter, c);
    }
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
