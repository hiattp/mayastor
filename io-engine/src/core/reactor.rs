//! The first thing that needs to happen is to initialize DPDK, this among
//! others provides us with lockless queues which we use to send messages
//! between the cores.  Typically, these messages contain simple function
//! pointers and argument pointers.
//!
//! Per core data structure, there are so-called threads. These threads
//! represent, not to be confused with OS threads are created dynamically
//! depending on the subsystem(s). For example, an NVMF subsystem might create a
//! new thread instance on a given core. This separates different types of work
//! and allows us to divide work between cores evenly.
//!
//! To summarize, a reactor instance to CPU core is a one-to-one relation. A
//! reactor, in turn, may have one or more thread objects. The thread objects
//! MAY hold messages for a specific subsystem. During init, per reactor, we
//! create one thread which is always thread 0.
//!
//! During the poll loop, we traverse all threads and poll each queue of that
//! thread. The functions executed are all executed within the context of that
//! thread. This in effect means that we set a TLS pointer to the thread that we
//! are polling. This context is verified during run time, such that we can
//! ensure that, for example,  a bdev (say) that is open, is closed within the
//! same context as it was opened with.
//!
//! The queue of each thread is unique, but the messages in the queue are all
//! preallocated from a global pool. This prevents allocations at runtime.
//!
//! Alongside that, each reactor (currently) has two additional queues. One
//! queue is for receiving and sending messages between cores. The other queue
//! is used for holding on to the messages while it is being processed. Once
//! processed (or completed) it is dropped from the queue. Unlike the native
//! SPDK messages, these futures -- are allocated before they execute.
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    fmt::{self, Debug, Display, Formatter},
    future::Future,
    os::raw::c_void,
    pin::Pin,
    slice::Iter,
    time::Duration,
};

use once_cell::sync::OnceCell;

use crossbeam::channel::{unbounded, Receiver, Sender};
use futures::{
    channel::oneshot::{Receiver as OnceShotRecv, Sender as OneShotSend},
    task::{Context, Poll},
};

use spdk_rs::libspdk::{
    spdk_cpuset_get_cpu,
    spdk_env_thread_launch_pinned,
    spdk_env_thread_wait_all,
    spdk_thread,
    spdk_thread_get_cpumask,
    spdk_thread_lib_init_ext,
    spdk_thread_op,
    spdk_thread_send_msg,
    SPDK_DEFAULT_MSG_MEMPOOL_SIZE,
    SPDK_THREAD_OP_NEW,
};

use crate::core::{CoreError, Cores};
use gettid::gettid;
use nix::errno::Errno;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReactorState {
    Init,
    Running,
    Shutdown,
    Delayed,
}

impl Display for ReactorState {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let s = match self {
            ReactorState::Init => "Init",
            ReactorState::Running => "Running",
            ReactorState::Shutdown => "Shutdown",
            ReactorState::Delayed => "Delayed",
        };
        write!(f, "{}", s)
    }
}

#[derive(Debug)]
pub struct Reactors(Vec<Reactor>);

unsafe impl Sync for Reactors {}
unsafe impl Send for Reactors {}

unsafe impl Sync for Reactor {}
unsafe impl Send for Reactor {}

pub static REACTOR_LIST: OnceCell<Reactors> = OnceCell::new();

// TODO: we only have one "type" of core however, only the master core deals
// with futures we can TODO: should consider creating two variants of the
// Reactor: master and remote
#[derive(Debug)]
#[allow(unknown_lints)]
#[allow(clippy::non_send_fields_in_send_ty)]
pub struct Reactor {
    /// Vector of threads allocated by the various subsystems. The threads are
    /// protected by a RefCell to avoid, at runtime, mutating the vector.
    /// This, ideally, we don't want to do but considering the unsafety we
    /// keep it for now
    threads: RefCell<VecDeque<spdk_rs::Thread>>,
    /// incoming threads that have been scheduled to this core but are not
    /// polled yet
    incoming: crossbeam::queue::SegQueue<spdk_rs::Thread>,
    /// the logical core this reactor is created on
    lcore: u32,
    /// represents the state of the reactor
    flags: Cell<ReactorState>,
    /// Unique identifier of the thread on which reactor is running.
    tid: Cell<u64>,
    /// sender and Receiver for sending futures across cores without going
    /// through FFI
    sx: Sender<Pin<Box<dyn Future<Output = ()> + 'static>>>,
    rx: Receiver<Pin<Box<dyn Future<Output = ()> + 'static>>>,
}

thread_local! {
    /// This queue holds any in coming futures from other cores
    static QUEUE: (Sender<async_task::Runnable>, Receiver<async_task::Runnable>) = unbounded();
}

impl Reactors {
    /// initialize the reactor subsystem for each core assigned to us
    pub fn init() {
        REACTOR_LIST.get_or_init(|| {
            let rc = unsafe {
                spdk_thread_lib_init_ext(
                    Some(Self::do_op),
                    Some(Self::can_op),
                    0,
                    SPDK_DEFAULT_MSG_MEMPOOL_SIZE as u64,
                )
            };
            assert_eq!(rc, 0);

            Reactors(
                Cores::count()
                    .into_iter()
                    .map(Reactor::new)
                    .collect::<Vec<_>>(),
            )
        });

        // construct one main init thread, this thread is used to bootstrap
        // and should be used to teardown as well.
        if let Some(t) =
            spdk_rs::Thread::new("init_thread".into(), Cores::first())
        {
            info!("Init thread ID {}", t.id());
        }
    }

    /// advertise what scheduling options we support
    extern "C" fn can_op(op: spdk_thread_op) -> bool {
        matches!(op, SPDK_THREAD_OP_NEW)
    }

    /// do the advertised scheduling option
    extern "C" fn do_op(thread: *mut spdk_thread, op: spdk_thread_op) -> i32 {
        match op {
            SPDK_THREAD_OP_NEW => Self::schedule(thread),
            _ => -1,
        }
    }

    /// schedule a thread in here, we should make smart choices based
    /// on load etc, right now we schedule to the current core.
    fn schedule(thread: *mut spdk_thread) -> i32 {
        let mask = unsafe { spdk_thread_get_cpumask(thread) };
        let scheduled = Reactors::iter().any(|r| {
            if unsafe { spdk_cpuset_get_cpu(mask, r.lcore) } {
                let mt = spdk_rs::Thread::from_ptr(thread);
                info!(
                    "Scheduled SPDK thread '{}' ({:p}) on core #{}",
                    mt.name(),
                    thread,
                    r.lcore,
                );
                r.incoming.push(mt);
                return true;
            }
            false
        });

        if !scheduled {
            error!("Failed to find core for thread {:p}!", thread);
            1
        } else {
            0
        }
    }

    /// launch the poll loop on the master core, this is implemented somewhat
    /// different from the remote cores.
    pub fn launch_master() {
        assert_eq!(Cores::current(), Cores::first());
        Reactor::poll(Cores::current() as *const u32 as *mut c_void);
        // wait for all other cores to exit before we unblock.
        unsafe { spdk_env_thread_wait_all() };
    }

    /// start polling the reactors on the given core, when multiple cores are
    /// involved they must be running during init as they must process in coming
    /// messages that are send as part of the init process.
    #[allow(clippy::needless_return)]
    pub fn launch_remote(core: u32) -> Result<(), CoreError> {
        // the master core -- who is the only core that can call this function
        // should not be launched this way. For that use ['launch_master`].
        // Nothing prevents anyone from call this function twice now.
        if core == Cores::current() {
            return Ok(());
        }

        if Cores::count().into_iter().any(|c| c == core) {
            let rc = unsafe {
                spdk_env_thread_launch_pinned(
                    core,
                    Some(Reactor::poll),
                    core as *const u32 as *mut c_void,
                )
            };
            return if rc == 0 {
                Ok(())
            } else {
                error!("Failed to launch core #{}", core);
                Err(CoreError::ReactorConfigureFailed {
                    source: Errno::from_i32(rc),
                })
            };
        } else {
            Err(CoreError::ReactorConfigureFailed {
                source: Errno::ENOSYS,
            })
        }
    }

    /// get a reference to a ['Reactor'] associated with the given core.
    pub fn get_by_core(core: u32) -> Option<&'static Reactor> {
        Reactors::iter().find(|c| c.lcore == core)
    }

    /// get a reference to a reactor on the current core
    pub fn current() -> &'static Reactor {
        Self::get_by_core(Cores::current()).expect("no reactor allocated")
    }

    pub fn master() -> &'static Reactor {
        Self::get_by_core(Cores::first()).expect("no reactor allocated")
    }

    pub fn is_master() -> bool {
        Cores::first() == Cores::current()
    }

    /// returns an iterator over all reactors
    pub fn iter() -> Iter<'static, Reactor> {
        REACTOR_LIST.get().unwrap().into_iter()
    }
}

impl<'a> IntoIterator for &'a Reactors {
    type Item = &'a Reactor;
    type IntoIter = ::std::slice::Iter<'a, Reactor>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Reactor {
    /// create a new ['Reactor'] instance
    fn new(core: u32) -> Self {
        // create a channel to receive futures on
        let (sx, rx) =
            unbounded::<Pin<Box<dyn Future<Output = ()> + 'static>>>();

        Self {
            threads: RefCell::new(VecDeque::new()),
            incoming: crossbeam::queue::SegQueue::new(),
            lcore: core,
            flags: Cell::new(ReactorState::Init),
            tid: Cell::new(0),
            sx,
            rx,
        }
    }

    /// this function gets called by DPDK
    extern "C" fn poll(arg: *mut c_void) -> i32 {
        let core = arg as u32;

        info!(core, tid = gettid(), "Starting reactor polling loop",);
        let reactor = Reactors::get_by_core(core).unwrap();
        if reactor.get_state() != ReactorState::Init {
            warn!("calling poll on a reactor who is not in the INIT state");
        }

        if std::env::var("MAYASTOR_DELAY").is_ok() {
            reactor.developer_delayed();
        } else {
            reactor.running();
        }
        // loops
        reactor.poll_reactor();
        0
    }

    /// run the futures received on the channel
    fn run_futures(&self) {
        QUEUE.with(|(_, r)| {
            r.try_iter().for_each(|f| {
                f.run();
            })
        });
    }

    /// receive futures if any
    fn receive_futures(&self) {
        self.rx.try_iter().for_each(|m| {
            self.spawn_local(m).detach();
        });
    }

    /// send messages to the core/thread -- similar as spdk_thread_send_msg()
    pub fn send_future<F>(&self, future: F)
    where
        F: Future<Output = ()> + 'static,
    {
        self.sx.send(Box::pin(future)).unwrap();
    }

    /// spawn a future locally on this core; note that you can *not* use the
    /// handle to complete the future with a different runtime.
    pub fn spawn_local<F, R>(&self, future: F) -> async_task::Task<R>
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        // our scheduling right now is basically non-existent but -- in the
        // future we want to schedule work to cores that are not very
        // busy etc.
        let schedule = |t| QUEUE.with(|(s, _)| s.send(t).unwrap());

        let (runnable, task) = async_task::spawn_local(future, schedule);
        runnable.schedule();
        // the handler typically has no meaning to us unless we want to wait for
        // the spawned future to complete before we continue which is
        // done, in example with ['block_on']
        task
    }

    /// spawn a future locally on the current core block until the future is
    /// completed. The master core is used.
    pub fn block_on<F, R>(future: F) -> Option<R>
    where
        F: Future<Output = R> + 'static,
        R: 'static,
    {
        // hold on to the any potential thread we might be running on right now
        let thread = spdk_rs::Thread::current();
        spdk_rs::Thread::primary().set_current();
        let schedule = |t| QUEUE.with(|(s, _)| s.send(t).unwrap());
        let (runnable, task) = async_task::spawn_local(future, schedule);

        let waker = runnable.waker();
        let cx = &mut Context::from_waker(&waker);

        pin_utils::pin_mut!(task);
        runnable.schedule();
        let reactor = Reactors::master();

        loop {
            match task.as_mut().poll(cx) {
                Poll::Ready(output) => {
                    spdk_rs::Thread::primary().unset_current();
                    if let Some(t) = thread {
                        t.set_current()
                    }
                    return Some(output);
                }
                Poll::Pending => {
                    reactor.poll_once();
                }
            };
        }
    }

    /// set the state of this reactor
    fn set_state(&self, state: ReactorState) {
        match state {
            ReactorState::Init
            | ReactorState::Delayed
            | ReactorState::Shutdown
            | ReactorState::Running => {
                self.flags.set(state);
            }
        }
    }

    /// set the state of the reactor to running. In this state the reactor will
    /// poll for work on the thread message pools as well as its own queue
    /// to launch futures.
    pub fn running(&self) {
        self.set_state(ReactorState::Running)
    }

    /// set the reactor to sleep each iteration
    pub fn developer_delayed(&self) {
        info!("core {} set to developer delayed poll mode", self.lcore);
        self.set_state(ReactorState::Delayed);
    }

    /// initiate shutdown of the reactor and stop polling
    pub fn shutdown(&self) {
        info!("shutdown requested for core {}", self.lcore);
        self.set_state(ReactorState::Shutdown);
    }

    /// returns the current state of the reactor
    pub fn get_state(&self) -> ReactorState {
        self.flags.get()
    }

    /// returns core number of this reactor
    pub fn core(&self) -> u32 {
        self.lcore
    }

    /// Returns system identifier of the thread this reactor is running on.
    pub fn tid(&self) -> u64 {
        self.tid.get()
    }

    /// poll this reactor to complete any work that is pending
    pub fn poll_reactor(&self) {
        // Initialize TID for this reactor.
        self.tid.set(gettid());

        loop {
            match self.get_state() {
                // running is the default mode for all cores. All cores, except
                // the master core spin within this specific loop
                ReactorState::Running => {
                    self.poll_once();
                }
                ReactorState::Shutdown => {
                    info!("reactor {} shutdown requested", self.lcore);
                    break;
                }
                ReactorState::Delayed => {
                    std::thread::sleep(Duration::from_millis(1));
                    self.poll_once();
                }
                _ => panic!("invalid reactor state {:?}", self.get_state()),
            }

            self.destroy_exited();
        }

        debug!("initiating shutdown for core {}", Cores::current());

        if self.lcore == Cores::first() {
            debug!("master core stopped polling");
        }
    }

    /// polls the reactor only once for any work regardless of its state. For
    /// now
    #[inline]
    pub fn poll_once(&self) {
        self.receive_futures();
        self.run_futures();
        let threads = self.threads.borrow();
        threads.iter().for_each(|t| {
            t.poll();
        });

        drop(threads);

        self.add_incoming();
    }

    /// poll the threads n times but only poll the futures queue once and look
    /// for incoming only once.
    ///
    /// We might want to set a flag that we need to run futures and or incoming
    /// queues
    pub fn poll_times(&self, times: u32) {
        let threads = self.threads.borrow();
        for _ in 0 .. times {
            threads.iter().for_each(|t| {
                t.poll();
            });
        }

        self.receive_futures();
        self.run_futures();

        drop(threads);

        self.add_incoming();
    }

    fn add_incoming(&self) {
        while let Some(i) = self.incoming.pop() {
            self.threads.borrow_mut().push_back(i);
        }
    }

    /// Removes from the reactor and destroys all existed SPDK threads.
    fn destroy_exited(&self) {
        let mut removed = Vec::new();

        {
            self.threads.borrow_mut().retain(|t| {
                if t.is_exited() {
                    removed.push(*t);
                    false
                } else {
                    true
                }
            });
        }

        removed.into_iter().for_each(|t| {
            info!(
                "Core #{}: destroying exited thread '{}'",
                Cores::current(),
                t.name()
            );
            t.destroy();
        });
    }

    /// TODO
    ///
    /// # Note
    ///
    /// Spawns a future on a core the current thread is running on returning a
    /// channel which can be awaited. This decouples the SPDK runtime from the
    /// future runtimes within Rust.
    pub fn spawn_at<F>(
        thread: &spdk_rs::Thread,
        f: F,
    ) -> Result<OnceShotRecv<F::Output>, CoreError>
    where
        F: Future + 'static,
        F::Output: Send + Debug,
    {
        // context structure which is passed to the callback as argument
        struct Ctx<F>
        where
            F: Future,
            F::Output: Send + Debug,
        {
            future: F,
            sender: Option<OneShotSend<F::Output>>,
        }

        // helper routine to unpack the closure and its arguments
        extern "C" fn trampoline<F>(arg: *mut c_void)
        where
            F: Future + 'static,
            F::Output: Send + Debug,
        {
            let mut ctx = unsafe { Box::from_raw(arg as *mut Ctx<F>) };
            Reactors::current()
                .spawn_local(async move {
                    let result = ctx.future.await;
                    if let Err(e) = ctx
                        .sender
                        .take()
                        .expect("sender already taken")
                        .send(result)
                    {
                        error!("Failed to send response future result {:?}", e);
                    }
                })
                .detach();
        }

        let (s, r) = futures::channel::oneshot::channel::<F::Output>();

        let ctx = Box::new(Ctx {
            future: f,
            sender: Some(s),
        });

        let rc = unsafe {
            spdk_thread_send_msg(
                thread.as_ptr(),
                Some(trampoline::<F>),
                Box::into_raw(ctx).cast(),
            )
        };
        if rc != 0 {
            Err(CoreError::NotSupported {
                source: Errno::UnknownErrno,
            })
        } else {
            Ok(r)
        }
    }

    /// TODO
    pub fn spawn_at_primary<F>(
        f: F,
    ) -> Result<OnceShotRecv<F::Output>, CoreError>
    where
        F: Future + 'static,
        F::Output: Send + Debug,
    {
        Self::spawn_at(&spdk_rs::Thread::primary(), f)
    }
}

/// This implements the poll() method of the for the reactor future. Only the
/// master core is polled by the Future abstraction. There are two reasons for
/// this
///
///  1. The master core is the management core, it is the only core that handles
/// gRPC calls  2. The master core handles the setup and tear down of the slave
/// cores
impl Future for &'static Reactor {
    type Output = Result<(), ()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match self.get_state() {
            ReactorState::Running => {
                self.poll_times(3);
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            ReactorState::Shutdown => {
                info!(
                    "Reactor #{} shutdown requested: {} SPDK thread(s) remain",
                    self.lcore,
                    self.threads.borrow().len()
                );

                {
                    while let Some(t) = self.threads.borrow_mut().pop_front() {
                        t.wait_exit();
                        t.destroy();
                    }
                }

                unsafe { spdk_env_thread_wait_all() };
                Poll::Ready(Err(()))
            }
            ReactorState::Delayed => {
                std::thread::sleep(Duration::from_millis(1));
                self.poll_once();
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            ReactorState::Init => {
                if std::env::var("MAYASTOR_DELAY").is_ok() {
                    self.developer_delayed();
                } else {
                    self.running();
                }
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

/// Heartbeat timeout (in seconds) to classify a reactor as frozen.
const REACTOR_HEARTBEAT_TIMEOUT: u64 = 3;

/// Monitor health for all reactors: all available reactors are constantly
/// monitored for liveness.
pub async fn reactor_monitor_loop(freeze_timeout: Option<u64>) {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Metadata for every reactor being monitored by the reactor monitor.
    struct ReactorRecord {
        frozen: bool,
        reactor: &'static Reactor,
        reactor_tick: &'static AtomicU64,
        core: u32,
    }

    let timeout = freeze_timeout.unwrap_or(REACTOR_HEARTBEAT_TIMEOUT);
    let num_cores = Cores::count().id() as usize;
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let mut tick: u64 = 0;
    let mut reactor_state: Vec<ReactorRecord> = Vec::with_capacity(num_cores);
    static REACTOR_TICKS: OnceCell<Vec<AtomicU64>> = OnceCell::new();

    info!(num_cores, timeout, "Starting reactor health monitor loop");

    // Intialize shared counters for heartbeat futures sent to reactors.
    let heartbeat_ticks = REACTOR_TICKS.get_or_init(|| {
        std::iter::repeat_with(|| AtomicU64::new(0))
            .take(num_cores)
            .collect::<Vec<AtomicU64>>()
    });

    // Initialize reactor records.
    for (id, core) in Cores::count().into_iter().enumerate() {
        let reactor = Reactors::get_by_core(core)
            .unwrap_or_else(|| panic!("Can't get reactor for core {}", core));
        let reactor_tick =
            heartbeat_ticks.get(id).expect("Failed to get tick item");

        reactor_state.push(ReactorRecord {
            frozen: false,
            reactor,
            reactor_tick,
            core,
        });
    }

    loop {
        // Schedule heartbeat futures on every reactor, ignoring reactors
        // which are already frozen.
        for (id, r) in reactor_state.iter().enumerate() {
            // For frozen reactors there are already N scheduled heartbeat
            // futures that haven't resolved yet, so maintain exactly this delta
            // by just adjusting the tick counter.
            if r.frozen {
                heartbeat_ticks[id].fetch_add(1, Ordering::Relaxed);
            } else {
                // Send heartbeat future to the reactor.
                r.reactor.send_future(async move {
                    heartbeat_ticks[id].fetch_add(1, Ordering::Relaxed);
                });
            }
        }

        // Wait till heartbeat check interval elapses and check ticks
        // reported by every reactor.
        interval.tick().await;
        tick += 1;

        for r in &mut reactor_state {
            if r.frozen {
                // Check if all pending heartbeat futures have resolved:
                // in such a case heartbeat counter adds to the correct
                // value and mark the reactor as alive.
                if tick - r.reactor_tick.load(Ordering::Relaxed) == 0 {
                    info!(core = r.core, "Reactor is healthy again");
                    r.frozen = false;
                }
            } else {
                // Reactor didn't respond within allowed number of intervals,
                // assume it is frozen.
                if tick - r.reactor_tick.load(Ordering::Relaxed) >= timeout {
                    r.frozen = true;
                    crate::core::diagnostics::diagnose_reactor(r.reactor);
                }
            }
        }
    }
}
