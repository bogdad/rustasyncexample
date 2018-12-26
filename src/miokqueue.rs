use libc;
use mioio::MioPollOpt;


use mioio::MioReady;

use std::ptr;

use std::os::raw::c_int;
use std::os::raw::c_short;

use std::cell::UnsafeCell;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use std::os::unix::io::RawFd;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MioToken(pub usize);

impl From<usize> for MioToken {
    fn from(val: usize) -> MioToken {
        MioToken(val)
    }
}

impl From<MioToken> for usize {
    fn from(val: MioToken) -> usize {
        val.0
    }
}

#[derive(Debug)]
pub struct MioSelectorId {
    id: AtomicUsize,
}

impl MioSelectorId {
    pub fn new() -> MioSelectorId {
        MioSelectorId {
            id: AtomicUsize::new(0),
        }
    }

    pub fn associate_selector(&self, poll: &MioPoll) -> io::Result<()> {
        let selector_id = self.id.load(Ordering::SeqCst);

        if selector_id != 0 && selector_id != poll.selector.id {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "socket already registered",
            ))
        } else {
            self.id.store(poll.selector.id, Ordering::SeqCst);
            Ok(())
        }
    }
}

struct MioAtomicState {
    inner: AtomicUsize,
}

#[cfg(not(target_os = "netbsd"))]
type Filter = c_short;
#[cfg(not(target_os = "netbsd"))]
type UData = *mut ::libc::c_void;
#[cfg(not(target_os = "netbsd"))]
type Count = c_int;

#[cfg(target_os = "netbsd")]
type Filter = u32;
#[cfg(target_os = "netbsd")]
type UData = ::libc::intptr_t;
#[cfg(target_os = "netbsd")]
type Count = usize;

macro_rules! kevent {
    ($id: expr, $filter: expr, $flags: expr, $data: expr) => {
        libc::kevent {
            ident: $id as ::libc::uintptr_t,
            filter: $filter as Filter,
            flags: $flags,
            fflags: 0,
            data: 0,
            udata: $data as UData,
        }
    };
}

pub struct MioKQueueSelector {
    id: usize,
    kq: RawFd,
}

trait IsMinusOne {
    fn is_minus_one(&self) -> bool;
}

impl IsMinusOne for i32 {
    fn is_minus_one(&self) -> bool {
        *self == -1
    }
}
impl IsMinusOne for isize {
    fn is_minus_one(&self) -> bool {
        *self == -1
    }
}

fn cvt<T: IsMinusOne>(t: T) -> ::io::Result<T> {
    use std::io;

    if t.is_minus_one() {
        Err(io::Error::last_os_error())
    } else {
        Ok(t)
    }
}

impl MioKQueueSelector {
    pub fn register(
        &self,
        fd: RawFd,
        token: MioToken,
        interests: MioReady,
        opts: MioPollOpt,
    ) -> io::Result<()> {
        //trace!("registering; token={:?}; interests={:?}", token, interests);

        let flags = if opts.contains(MioPollOpt::edge()) {
            libc::EV_CLEAR
        } else {
            0
        } | if opts.contains(MioPollOpt::oneshot()) {
            libc::EV_ONESHOT
        } else {
            0
        } | libc::EV_RECEIPT;

        unsafe {
            let r = if interests.contains(MioReady::readable()) {
                libc::EV_ADD
            } else {
                libc::EV_DELETE
            };
            let w = if interests.contains(MioReady::writable()) {
                libc::EV_ADD
            } else {
                libc::EV_DELETE
            };
            let mut changes = [
                kevent!(fd, libc::EVFILT_READ, flags | r, usize::from(token)),
                kevent!(fd, libc::EVFILT_WRITE, flags | w, usize::from(token)),
            ];

            cvt(libc::kevent(
                self.kq,
                changes.as_ptr(),
                changes.len() as Count,
                changes.as_mut_ptr(),
                changes.len() as Count,
                ::std::ptr::null(),
            ))?;

            for change in changes.iter() {
                debug_assert_eq!(change.flags & libc::EV_ERROR, libc::EV_ERROR);

                // Test to see if an error happened
                if change.data == 0 {
                    continue;
                }

                // Older versions of OSX (10.11 and 10.10 have been witnessed)
                // can return EPIPE when registering a pipe file descriptor
                // where the other end has already disappeared. For example code
                // that creates a pipe, closes a file descriptor, and then
                // registers the other end will see an EPIPE returned from
                // `register`.
                //
                // It also turns out that kevent will still report events on the
                // file descriptor, telling us that it's readable/hup at least
                // after we've done this registration. As a result we just
                // ignore `EPIPE` here instead of propagating it.
                //
                // More info can be found at carllerche/mio#582
                if change.data as i32 == libc::EPIPE
                    && change.filter == libc::EVFILT_WRITE as Filter
                {
                    continue;
                }

                // ignore ENOENT error for EV_DELETE
                let orig_flags = if change.filter == libc::EVFILT_READ as Filter {
                    r
                } else {
                    w
                };
                if change.data as i32 == libc::ENOENT && orig_flags & libc::EV_DELETE != 0 {
                    continue;
                }

                return Err(::std::io::Error::from_raw_os_error(change.data as i32));
            }
            Ok(())
        }
    }

    pub fn reregister(
        &self,
        fd: RawFd,
        token: MioToken,
        interests: MioReady,
        opts: MioPollOpt,
    ) -> io::Result<()> {
        self.register(fd, token, interests, opts)
    }

    pub fn deregister(&self, fd: RawFd) -> io::Result<()> {
        unsafe {
            // EV_RECEIPT is a nice way to apply changes and get back per-event results while not
            // draining the actual changes.
            let filter = libc::EV_DELETE | libc::EV_RECEIPT;

            let mut changes = [
                kevent!(fd, libc::EVFILT_READ, filter, ptr::null_mut()),
                kevent!(fd, libc::EVFILT_WRITE, filter, ptr::null_mut()),
            ];

            cvt(libc::kevent(
                self.kq,
                changes.as_ptr(),
                changes.len() as Count,
                changes.as_mut_ptr(),
                changes.len() as Count,
                ::std::ptr::null(),
            ))
            .map(|_| ())?;

            if changes[0].data as i32 == libc::ENOENT && changes[1].data as i32 == libc::ENOENT {
                return Err(::std::io::Error::from_raw_os_error(changes[0].data as i32));
            }
            for change in changes.iter() {
                debug_assert_eq!(libc::EV_ERROR & change.flags, libc::EV_ERROR);
                if change.data != 0 && change.data as i32 != libc::ENOENT {
                    return Err(::std::io::Error::from_raw_os_error(changes[0].data as i32));
                }
            }
            Ok(())
        }
    }
}

struct MioReadinessNode {
    // Node state, see struct docs for `ReadinessState`
    //
    // This variable is the primary point of coordination between all the
    // various threads concurrently accessing the node.
    state: MioAtomicState,

    // The registration token cannot fit into the `state` variable, so it is
    // broken out here. In order to atomically update both the state and token
    // we have to jump through a few hoops.
    //
    // First, `state` includes `token_read_pos` and `token_write_pos`. These can
    // either be 0, 1, or 2 which represent a token slot. `token_write_pos` is
    // the token slot that contains the most up to date registration token.
    // `token_read_pos` is the token slot that `poll` is currently reading from.
    //
    // When a call to `update` includes a different token than the one currently
    // associated with the registration (token_write_pos), first an unused token
    // slot is found. The unused slot is the one not represented by
    // `token_read_pos` OR `token_write_pos`. The new token is written to this
    // slot, then `state` is updated with the new `token_write_pos` value. This
    // requires that there is only a *single* concurrent call to `update`.
    //
    // When `poll` reads a node state, it checks that `token_read_pos` matches
    // `token_write_pos`. If they do not match, then it atomically updates
    // `state` such that `token_read_pos` is set to `token_write_pos`. It will
    // then read the token at the newly updated `token_read_pos`.
    token_0: UnsafeCell<MioToken>,
    token_1: UnsafeCell<MioToken>,
    token_2: UnsafeCell<MioToken>,

    // Used when the node is queued in the readiness linked list. Accessing
    // this field requires winning the "queue" lock
    next_readiness: AtomicPtr<MioReadinessNode>,

    // Ensures that there is only one concurrent call to `update`.
    //
    // Each call to `update` will attempt to swap `update_lock` from `false` to
    // `true`. If the CAS succeeds, the thread has obtained the update lock. If
    // the CAS fails, then the `update` call returns immediately and the update
    // is discarded.
    update_lock: AtomicBool,

    // Pointer to Arc<ReadinessQueueInner>
    readiness_queue: AtomicPtr<()>,

    // Tracks the number of `ReadyRef` pointers
    ref_count: AtomicUsize,
}

struct MioReadinessQueueInner {
    // Used to wake up `Poll` when readiness is set in another thread.
    //awakener: sys::Awakener,

    // Head of the MPSC queue used to signal readiness to `Poll::poll`.
    head_readiness: AtomicPtr<MioReadinessNode>,

    // Tail of the readiness queue.
    //
    // Only accessed by MioPoll::poll. Coordination will be handled by the poll fn
    tail_readiness: UnsafeCell<*mut MioReadinessNode>,

    // Fake readiness node used to punctuate the end of the readiness queue.
    // Before attempting to read from the queue, this node is inserted in order
    // to partition the queue between nodes that are "owned" by the dequeue end
    // and nodes that will be pushed on by producers.
    end_marker: Box<MioReadinessNode>,

    // Similar to `end_marker`, but this node signals to producers that `Poll`
    // has gone to sleep and must be woken up.
    sleep_marker: Box<MioReadinessNode>,

    // Similar to `end_marker`, but the node signals that the queue is closed.
    // This happens when `ReadyQueue` is dropped and signals to producers that
    // the nodes should no longer be pushed into the queue.
    closed_marker: Box<MioReadinessNode>,
}

#[derive(Clone)]
struct MioReadinessQueue {
    inner: Arc<MioReadinessQueueInner>,
}

pub struct MioPoll {
    selector: MioKQueueSelector,

    // Custom readiness queue
    readiness_queue: MioReadinessQueue,

    // Use an atomic to first check if a full lock will be required. This is a
    // fast-path check for single threaded cases avoiding the extra syscall
    lock_state: AtomicUsize,

    // Sequences concurrent calls to `Poll::poll`
    lock: Mutex<()>,

    // Wakeup the next waiter
    condvar: Condvar,
}

pub trait MioEvented {
    fn register(&self, &MioPoll, MioToken, MioReady, MioPollOpt) -> io::Result<()>;
    fn reregister(&self, &MioPoll, MioToken, MioReady, MioPollOpt) -> io::Result<()>;
    fn deregister(&self, &MioPoll) -> io::Result<()>;
}

pub struct MioUnixEventedFd<'a>(pub &'a RawFd);

impl<'a> MioEvented for MioUnixEventedFd<'a> {
    fn register(
        &self,
        poll: &MioPoll,
        token: MioToken,
        interest: MioReady,
        opts: MioPollOpt,
    ) -> io::Result<()> {
        poll.selector.register(*self.0, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &MioPoll,
        token: MioToken,
        interest: MioReady,
        opts: MioPollOpt,
    ) -> io::Result<()> {
        poll.selector.reregister(*self.0, token, interest, opts)
    }

    fn deregister(&self, poll: &MioPoll) -> io::Result<()> {
        poll.selector.deregister(*self.0)
    }
}

/*

impl Evented for TcpListener {
    fn register(&self, poll: &Poll, token: Token,
                interest: Ready, opts: PollOpt) -> io::Result<()> {
        EventedFd(&self.as_raw_fd()).register(poll, token, interest, opts)
    }

    fn reregister(&self, poll: &Poll, token: Token,
                  interest: Ready, opts: PollOpt) -> io::Result<()> {
        EventedFd(&self.as_raw_fd()).reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        EventedFd(&self.as_raw_fd()).deregister(poll)
    }
}


*/
