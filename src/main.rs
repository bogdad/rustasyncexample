#![feature(never_type)]

use mioio::MioPollOpt;
use mioio::MioReady;
use miokqueue::MioEvented;
use miokqueue::MioPoll;
use miokqueue::MioToken;
use miokqueue::MioUnixEventedFd;
use std::cell::UnsafeCell;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::RawFd;
use std::sync::atomic::AtomicUsize;
use std::sync::Weak;

use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::io;
use std::net as stdnet;
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use std::usize;

extern crate libc;

mod mioio;
mod miokqueue;

use miokqueue::MioSelectorId;

//TODO: split!

#[derive(Clone)]
pub struct Waker {
    wake: Arc<Wake>,
}

pub trait Wake: Send + Sync {
    /// Signals that the associated task is ready to be `poll`ed again.;
    fn wake(&self);
}

impl Waker {
    /// Signals that the associated task is ready to be `poll`ed again.
    pub fn wake(&self) {
        self.wake.wake();
    }
}

impl<T: Wake + 'static> From<Arc<T>> for Waker {
    fn from(wake: Arc<T>) -> Waker {
        Waker { wake: wake }
    }
}

pub enum Async<T> {
    /// Work completed with a result of type `T`.
    Ready(T),

    /// Work was blocked, and the task is set to be woken when ready
    /// to continue.
    Pending,
}

// the internal executor state
struct ExecState {
    // The next available task ID.
    next_id: usize,

    // The complete list of tasks, keyed by ID.
    tasks: HashMap<usize, TaskEntry>,

    // The set of IDs for ready-to-run tasks.
    ready: HashSet<usize>,

    // The actual OS thread running the executor.
    thread: thread::Thread,
}

impl ExecState {
    fn wake_task(&mut self, id: usize) {
        self.ready.insert(id);

        // *after* inserting in the ready set, ensure the executor OS
        // thread is woken up if it's not already running.
        self.thread.unpark();
    }
}

struct ToyWake {
    // A link back to the executor that owns the task we want to wake up.
    exec: ToyExec,

    // The ID for the task we want to wake up.
    id: usize,
}

impl Wake for ToyWake {
    fn wake(&self) {
        //println!("waiking task {:?} ", self.id);
        self.exec.state_mut().wake_task(self.id);
    }
}

#[derive(Clone)]
pub struct ToyExec {
    state: Arc<Mutex<ExecState>>,
}

struct TaskEntry {
    task: Box<ToyTask + Send>,
    wake: Waker,
}

pub trait ToyTask {
    /// Attempt to finish executing the task, returning `Async::Pending`
    /// if the task needs to wait for an event before it can complete.
    fn poll(&mut self, waker: &Waker) -> Async<()>;
}

impl ToyExec {
    pub fn new() -> Self {
        ToyExec {
            state: Arc::new(Mutex::new(ExecState {
                next_id: 0,
                tasks: HashMap::new(),
                ready: HashSet::new(),
                thread: std::thread::current(),
            })),
        }
    }

    // a convenience method for getting our hands on the executor state
    fn state_mut(&self) -> MutexGuard<ExecState> {
        self.state.lock().unwrap()
    }

    pub fn run(&self) {
        loop {
            // Each time around, we grab the *entire* set of ready-to-run task IDs:
            let mut ready = std::mem::replace(&mut self.state_mut().ready, HashSet::new());
            //println!("ready size {:?}", ready);
            // Now try to `complete` each initially-ready task:
            for id in ready.drain() {
                // We take *full ownership* of the task; if it completes, it will
                // be dropped.
                let entry = self.state_mut().tasks.remove(&id);
                if let Some(mut entry) = entry {
                    if let Async::Pending = entry.task.poll(&entry.wake) {
                        //println!("id {:?} has not completed", id);
                        // The task hasn't completed, so put it back in the table.
                        self.state_mut().tasks.insert(id, entry);
                        self.state_mut().ready.insert(id);
                    }
                }
            }

            // We've processed all work we acquired on entry; block until more work
            // is available. If new work became available after our `ready` snapshot,
            // this will be a no-op.
            std::thread::park();
        }
    }

    pub fn spawn<T>(&self, task: T)
    where
        T: ToyTask + Send + 'static,
    {
        let mut state = self.state_mut();

        let id = state.next_id;
        state.next_id += 1;

        let wake = ToyWake {
            id,
            exec: self.clone(),
        };
        let entry = TaskEntry {
            wake: Waker::from(Arc::new(wake)),
            task: Box::new(task),
        };
        state.tasks.insert(id, entry);

        // A newly-added task is considered immediately ready to run,
        // which will cause a subsequent call to `park` to immediately
        // return.
        //println!("waking {:?} ", id);
        state.wake_task(id);
    }
}

/// A wakeup request
struct Registration {
    at: Instant,
    wake: Waker,
}

#[derive(Clone)]
struct ToyTimer {
    tx: mpsc::Sender<Registration>,
}

/// State for the worker thread that processes timer events
struct Worker {
    rx: mpsc::Receiver<Registration>,
    active: BTreeMap<Instant, Waker>,
}

impl ToyTimer {
    fn new() -> ToyTimer {
        let (tx, rx) = mpsc::channel();
        let worker = Worker {
            rx,
            active: BTreeMap::new(),
        };
        std::thread::spawn(|| worker.work());
        ToyTimer { tx }
    }

    // Register a new wakeup with this timer
    fn register(&self, at: Instant, wake: Waker) {
        self.tx.send(Registration { at, wake }).unwrap();
    }
}

impl Worker {
    fn enroll(&mut self, item: Registration) {
        if let Some(prev) = self.active.insert(item.at, item.wake.clone()) {
            self.enroll(Registration {
                at: item.at + Duration::new(0, 5),
                wake: prev,
            });
        }
    }

    fn fire(&mut self, key: Instant) {
        self.active.remove(&key).unwrap().wake();
    }

    fn work(mut self) {
        loop {
            if let Some(first) = self.active.keys().next().cloned() {
                let now = Instant::now();
                if first <= now {
                    self.fire(first);
                } else {
                    // we're not ready to fire off `first` yet, so wait until we are
                    // (or until we get a new registration, which might be for an
                    // earlier time).
                    if let Ok(new_registration) = self.rx.recv_timeout(first - now) {
                        self.enroll(new_registration);
                    }
                }
            } else {
                // no existing registrations, so unconditionally block until
                // we receive one.
                let new_registration = self.rx.recv().unwrap();
                self.enroll(new_registration)
            }
        }
    }
}

struct Periodic {
    // a name for this task
    id: u64,

    // how often to "ding"s
    period: Duration,

    // when the next "ding" is scheduled
    next: Instant,

    // a handle back to the timer event loop
    timer: ToyTimer,
}

impl Periodic {
    fn new(id: u64, period: Duration, timer: ToyTimer) -> Periodic {
        Periodic {
            id,
            period,
            timer,
            next: Instant::now() + period,
        }
    }
}

impl ToyTask for Periodic {
    fn poll(&mut self, wake: &Waker) -> Async<()> {
        // are we ready to ding yet?
        let now = Instant::now();
        //println!("Task {} {:?} {:?}", self.id, now, self.next);
        if now >= self.next {
            self.next = now + self.period;
            println!("Task {} - ding", self.id);
        }

        // make sure we're registered to wake up at the next expected `ding`
        self.timer.register(self.next, wake.clone());
        Async::Pending
    }
}

type AsyncResult<T, E> = TokioPoll<Result<T, E>>;
type AsyncIoResult<T> = AsyncResult<T, io::Error>;

/// An asynchronous computation that completes with a value or an error.
trait Future {
    type Item;
    type Error;

    /// Attempt to complete the future, yielding `Ok(Async::Pending)`
    /// if the future is blocked waiting for some other event to occur.
    fn poll(&mut self, waker: &Waker) -> TokioPoll<Result<Self::Item, Self::Error>>;
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum TokioPoll<T> {
    /// Represents that a value is immediately ready.
    Ready(T),

    /// Represents that a value is not ready yet.
    ///
    /// When a function returns `Pending`, the function *must* also
    /// ensure that the current task is scheduled to be awoken when
    /// progress can be made.
    Pending,
}

/// When called within a task being executed, returns the wakeup handle for
/// that task. Panics if called outside of task execution.

struct ToyTaskToFuture<T>(T);

impl<T: ToyTask> Future for ToyTaskToFuture<T> {
    type Item = ();
    type Error = !;

    fn poll(&mut self, waker: &Waker) -> TokioPoll<Result<(), !>> {
        match self.0.poll(waker) {
            Async::Ready(x) => TokioPoll::Ready(Ok(x)),
            Async::Pending => TokioPoll::Pending,
        }
    }
}

pub struct MioTcpStream {
    sys: stdnet::TcpStream,
    selector_id: MioSelectorId,
}

impl MioTcpStream {
    pub fn from_stream(stream: stdnet::TcpStream) -> io::Result<MioTcpStream> {
        MioTcpStream::set_nonblocking(&stream)?;

        Ok(MioTcpStream {
            sys: stream,
            selector_id: MioSelectorId::new(),
        })
    }

    fn set_nonblocking(stream: &stdnet::TcpStream) -> io::Result<()> {
        stream.set_nonblocking(true)
    }
}

struct MioTcpListener {
    sys: stdnet::TcpListener,
    selector_id: MioSelectorId,
}

impl MioTcpListener {
    fn bind(addr: &stdnet::SocketAddr) -> io::Result<MioTcpListener> {
        // mio has platform dependent code here
        // seems we dont need it
        stdnet::TcpListener::bind(addr).map(|stdlistener| MioTcpListener {
            sys: stdlistener,
            selector_id: MioSelectorId::new(),
        })
    }

    fn accept(&mut self) -> io::Result<(MioTcpStream, stdnet::SocketAddr)> {
        // TODO: continue from here
        let (s, a) = try!(self.sys.accept());
        Ok((MioTcpStream::from_stream(s)?, a))
    }
}

impl AsRawFd for MioTcpListener {
    fn as_raw_fd(&self) -> RawFd {
        self.sys.as_raw_fd()
    }
}

impl MioEvented for MioTcpListener {
    fn register(
        &self,
        poll: &MioPoll,
        token: MioToken,
        interest: MioReady,
        opts: MioPollOpt,
    ) -> io::Result<()> {
        self.selector_id.associate_selector(poll)?;
        MioUnixEventedFd(&self.as_raw_fd()).register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &MioPoll,
        token: MioToken,
        interest: MioReady,
        opts: MioPollOpt,
    ) -> io::Result<()> {
        MioUnixEventedFd(&self.as_raw_fd()).reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &MioPoll) -> io::Result<()> {
        MioUnixEventedFd(&self.as_raw_fd()).deregister(poll)
    }
}

pub struct TokioRegistration {
    inner: UnsafeCell<Option<TokioRegistrationInner>>,
    state: AtomicUsize,
}

/// Initial state. The handle is not set and the registration is idle.
const INIT: usize = 0;

/// A thread locked the state and will associate a handle.
const LOCKED: usize = 1;

/// A handle has been associated with the registration.
const READY: usize = 2;

/// Masks the lifecycle state
const LIFECYCLE_MASK: usize = 0b11;

/// A fake token used to identify error situations
const ERROR: usize = usize::MAX;

impl TokioRegistration {
    pub fn new() -> TokioRegistration {
        TokioRegistration {
            inner: UnsafeCell::new(None),
            state: AtomicUsize::new(INIT),
        }
    }
}

struct TokioRegistrationInner {
    handle: TokioHandlePriv,
    token: usize,
}

#[derive(Clone)]
struct TokioHandlePriv {
    inner: Weak<TokioPollEventedInner>,
}

pub struct TokioPollEvented<E: MioEvented> {
    io: Option<E>,
    inner: TokioPollEventedInner,
}

struct TokioPollEventedInner {
    registration: TokioRegistration,

    /// Currently visible read readiness
    read_readiness: AtomicUsize,

    /// Currently visible write readiness
    write_readiness: AtomicUsize,
}

impl<E> TokioPollEvented<E>
where
    E: MioEvented,
{
    pub fn new(io: E) -> TokioPollEvented<E> {
        TokioPollEvented {
            io: Some(io),
            inner: TokioPollEventedInner {
                registration: TokioRegistration::new(),
                read_readiness: AtomicUsize::new(0),
                write_readiness: AtomicUsize::new(0),
            },
        }
    }
}

struct TokioTcpListener {
    io: TokioPollEvented<MioTcpListener>,
}

impl TokioTcpListener {
    pub fn bind(addr: &stdnet::SocketAddr) -> io::Result<TokioTcpListener> {
        let l = MioTcpListener::bind(addr)?;
        Ok(TokioTcpListener::new(l))
    }

    fn new(listener: MioTcpListener) -> TokioTcpListener {
        let io = TokioPollEvented::new(listener);
        TokioTcpListener { io }
    }
}

struct ReadExactData<R> {
    reader: R,
    buf: Vec<u8>,
}

struct ReadExact<R> {
    data: Option<ReadExactData<R>>,
    from: usize,
    to: usize,
}

fn main() {
    let timer = ToyTimer::new();
    let exec = ToyExec::new();

    for i in 1..11 {
        exec.spawn(Periodic::new(
            i,
            Duration::from_millis(i * 500),
            timer.clone(),
        ));
    }

    exec.run()
}

#[test]
fn testMain() {
    /*
    let timer = ToyTimer::new();
    let exec = ToyExec::new();

    for i in 1..11 {
        exec.spawn(Periodic::new(
            i,
            Duration::from_millis(i * 500),
            timer.clone(),
        ));

        exec.run()
    }*/


}
