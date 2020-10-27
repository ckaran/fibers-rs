// Copyright (c) 2016 DWANGO Co., Ltd. All Rights Reserved.
// See the LICENSE file at the top-level directory of this distribution.

use futures::Future;
use futures03::compat::Future01CompatExt;
use futures03::executor::ThreadPool as ThreadPool03;
use futures03::task::FutureObj as FutureObj03;
use futures03::FutureExt;
use num_cpus;
use std::io;

use super::Executor;
use fiber::Spawn;

/// An executor that executes spawned fibers on pooled threads.
///
/// # Examples
///
/// An example to calculate fibonacci numbers:
///
/// ```
/// # extern crate fibers;
/// # extern crate futures;
/// use fibers::{Spawn, Executor, ThreadPoolExecutor};
/// use futures::{Async, Future};
///
/// fn fib<H: Spawn + Clone>(n: usize, handle: H) -> Box<dyn Future<Item=usize, Error=()> + Send> {
///     if n < 2 {
///         Box::new(futures::finished(n))
///     } else {
///         let f0 = handle.spawn_monitor(fib(n - 1, handle.clone()));
///         let f1 = handle.spawn_monitor(fib(n - 2, handle.clone()));
///         Box::new(f0.join(f1).map(|(a0, a1)| a0 + a1).map_err(|_| ()))
///     }
/// }
///
/// let mut executor = ThreadPoolExecutor::new().unwrap();
/// let monitor = executor.spawn_monitor(fib(7, executor.handle()));
/// let answer = executor.run_fiber(monitor).unwrap();
/// assert_eq!(answer, Ok(13));
/// ```
#[derive(Debug)]
pub struct ThreadPoolExecutor {
    pool: ThreadPool03,
}
impl ThreadPoolExecutor {
    /// Creates a new instance of `ThreadPoolExecutor`.
    ///
    /// This is equivalent to `ThreadPoolExecutor::with_thread_count(num_cpus::get() * 2)`.
    pub fn new() -> io::Result<Self> {
        Self::with_thread_count(num_cpus::get() * 2)
    }

    /// Creates a new instance of `ThreadPoolExecutor` with the specified size of thread pool.
    ///
    /// # Implementation Details
    ///
    /// Note that current implementation is very naive and
    /// should be improved in future releases.
    ///
    /// Internally, `count` threads are assigned to each of
    /// the scheduler (i.e., `fibers::fiber::Scheduler`) and
    /// the I/O poller (i.e., `fibers::io::poll::Poller`).
    ///
    /// When `spawn` function is called, the executor will assign a scheduler (thread)
    /// for the fiber in simple round robin fashion.
    ///
    /// If any of those threads are aborted, the executor will return an error as
    /// a result of `run_once` method call after that.
    pub fn with_thread_count(count: usize) -> io::Result<Self> {
        assert!(count > 0);
        let pool = ThreadPool03::builder().pool_size(count).create()?;
        Ok(Self { pool })
    }
}
impl Executor for ThreadPoolExecutor {
    type Handle = ThreadPoolExecutorHandle;
    fn handle(&self) -> Self::Handle {
        ThreadPoolExecutorHandle {
            pool: self.pool.clone(),
        }
    }
    fn run_once(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Spawn for ThreadPoolExecutor {
    fn spawn_boxed(&self, fiber: Box<dyn Future<Item = (), Error = ()> + Send>) {
        self.handle().spawn_boxed(fiber)
    }
}

/// A handle of a `ThreadPoolExecutor` instance.
#[derive(Debug, Clone)]
pub struct ThreadPoolExecutorHandle {
    pool: ThreadPool03,
}
impl Spawn for ThreadPoolExecutorHandle {
    fn spawn_boxed(&self, fiber: Box<dyn Future<Item = (), Error = ()> + Send>) {
        let future03 = fiber.compat().map(|_result| ());
        let futureobj03: FutureObj03<()> = Box::new(future03).into();
        self.pool.spawn_obj_ok(futureobj03);
    }
}