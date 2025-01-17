use log::debug;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use futures_task::{ArcWake, FutureObj};
use libc::{fd_set, select, timeval, FD_ISSET, FD_SET, FD_ZERO};

use std::os::unix::io::RawFd;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

mod async_tcp_listener;
mod async_tcp_stream;

pub use crate::async_tcp_listener::AsyncTcpListener;
pub use crate::async_tcp_stream::AsyncTcpStream;

// reactor lives in a thread local variable. Here's where all magic happens!
thread_local! {
    static REACTOR: Rc<EventLoop> = Rc::new(EventLoop::new());
}

type TaskId = usize;

pub fn run<F: Future<Output = ()> + Send + 'static>(f: F) {
    REACTOR.with(|reactor| reactor.run(f))
}

pub fn spawn<F: Future<Output = ()> + Send + 'static>(f: F) {
    REACTOR.with(|reactor| reactor.do_spawn(f))
}

// Our waker Token. It stores the index of the future in the wait queue
// (see below)
#[derive(Debug)]
struct Token(usize);

impl ArcWake for Token {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        debug!("waking {:?}", arc_self);

        let Token(idx) = **arc_self;

        // get access to the reactor by way of TLS and call wake
        REACTOR.with(|reactor| {
            let wakeup = Wakeup {
                index: idx,
                waker: futures_task::waker(arc_self.clone()),
            };
            reactor.wake(wakeup);
        });
    }
}

// Wakeup notification struct stores the index of the future in the wait queue
// and waker
struct Wakeup {
    index: usize,
    waker: Waker,
}

// Task is a boxed future with Output = ()
struct Task {
    future: FutureObj<'static, ()>,
}

impl Task {
    // returning Ready will lead to task being removed from wait queues and dropped
    fn poll(&mut self, waker: Waker) -> Poll<()> {
        let future = Pin::new(&mut self.future);
        let mut ctx = Context::from_waker(&waker);

        match future.poll(&mut ctx) {
            Poll::Ready(_) => {
                debug!("future done");
                Poll::Ready(())
            }
            Poll::Pending => {
                debug!("future not yet ready");
                Poll::Pending
            }
        }
    }
}

// The "real" event loop.
struct EventLoop {
    read: RefCell<BTreeMap<RawFd, Waker>>,
    write: RefCell<BTreeMap<RawFd, Waker>>,
    counter: Cell<usize>,
    wait_queue: RefCell<BTreeMap<TaskId, Task>>,
    run_queue: RefCell<VecDeque<Wakeup>>,
}

impl EventLoop {
    fn new() -> Self {
        EventLoop {
            read: RefCell::new(BTreeMap::new()),
            write: RefCell::new(BTreeMap::new()),
            counter: Cell::new(0),
            wait_queue: RefCell::new(BTreeMap::new()),
            run_queue: RefCell::new(VecDeque::new()),
        }
    }

    // a future calls this to register its interest
    // in socket's "ready to be read" events
    fn add_read_interest(&self, fd: RawFd, waker: Waker) {
        debug!("adding read interest for {}", fd);

        if !self.read.borrow().contains_key(&fd) {
            self.read.borrow_mut().insert(fd, waker);
        }
    }

    fn remove_read_interest(&self, fd: RawFd) {
        debug!("removing read interest for {}", fd);

        self.read.borrow_mut().remove(&fd);
    }

    // see above
    fn remove_write_interest(&self, fd: RawFd) {
        debug!("removing write interest for {}", fd);

        self.write.borrow_mut().remove(&fd);
    }

    fn add_write_interest(&self, fd: RawFd, waker: Waker) {
        debug!("adding write interest for {}", fd);

        if !self.write.borrow().contains_key(&fd) { //fd应该是可比较的，所以直接添加就行，btreemap不会重复添加元素，这里的contains检查多此一举
            self.write.borrow_mut().insert(fd, waker);
        }
    }

    // waker calls this to put the future on the run queue
    fn wake(&self, wakeup: Wakeup) {
        self.run_queue.borrow_mut().push_back(wakeup);
    }

    fn next_task(&self) -> (TaskId, Waker) {
        let counter = self.counter.get();
        let w = Arc::new(Token(counter));
        self.counter.set(counter + 1);
        (counter, futures_task::waker(w))
    }

    // create a task, poll it once and push it on wait queue
    fn do_spawn<F: Future<Output = ()> + Send + 'static>(&self, f: F) {
        let (id, waker) = self.next_task();
        let f = Box::new(f);
        let mut task = Task {
            future: FutureObj::new(f),
        };

        // if the task is ready immediately, don't add it to wait_queue
        if let Poll::Ready(_) = task.poll(waker) {
            return;
        }

        self.wait_queue.borrow_mut().insert(id, task);
    }

    // the meat of the event loop
    // we're using select(2) because it's simple and it's portable
    pub fn run<F: Future<Output = ()> + Send + 'static>(&self, f: F) {
        self.do_spawn(f);

        loop {
            //检测哪些fd就绪 - 开始
            debug!("select loop start");

            // event loop iteration timeout. if no descriptor
            // is ready we continue iterating
            let mut tv: timeval = timeval {
                tv_sec: 1,
                tv_usec: 0,
            };

            // initialize fd_sets (file descriptor sets)
            let mut read_fds: fd_set = unsafe { std::mem::zeroed() };
            let mut write_fds: fd_set = unsafe { std::mem::zeroed() };

            unsafe { FD_ZERO(&mut read_fds) };
            unsafe { FD_ZERO(&mut write_fds) };

            let mut nfds = 0;

            // add read interests to read fd_sets
            for fd in self.read.borrow().keys() {
                debug!("added fd {} for read", fd);
                unsafe { FD_SET(*fd, &mut read_fds as *mut fd_set) };
                nfds = std::cmp::max(nfds, fd + 1);
            }

            // add write interests to write fd_sets
            for fd in self.write.borrow().keys() {
                debug!("added fd {} for write", fd);
                unsafe { FD_SET(*fd, &mut write_fds as *mut fd_set) };
                nfds = std::cmp::max(nfds, fd + 1);
            }

            // select will block until some event happens
            // on the fds or timeout triggers
            let rv = unsafe {
                select(
                    nfds,
                    &mut read_fds,
                    &mut write_fds,
                    std::ptr::null_mut(),
                    &mut tv,
                )  //可将select换成mio
            };

            // don't care for errors
            if rv == -1 {
                panic!("select()");
            } else if rv == 0 {
                debug!("timeout");
            } else {
                debug!("data available on {} fds", rv);
            }

            //检测哪些fd就绪 - 结束

            //唤醒就绪的fd的context - 开始
            // check which fd it was and put appropriate future on run queue
            for (fd, waker) in self.read.borrow().iter() {
                let is_set = unsafe { FD_ISSET(*fd, &mut read_fds as *mut fd_set) };
                debug!("fd#{} set (read)", fd);
                if is_set {
                    waker.wake_by_ref();
                }
            }

            // same for write
            for (fd, waker) in self.write.borrow().iter() {
                let is_set = unsafe { FD_ISSET(*fd, &mut write_fds as *mut fd_set) };
                debug!("fd#{} set (write)", fd);
                if is_set {
                    waker.wake_by_ref();
                }
            }

            //唤醒就绪的fd的context - 结束

            //移除就绪的fd对应的task
            // now pop wakeup notifications from the run queue and poll associated futures
            loop {
                let w = self.run_queue.borrow_mut().pop_front();
                match w {
                    Some(w) => {
                        debug!("polling task#{}", w.index);

                        //先移除task，然后检测是否就绪，如果未就绪就重新添加回去，如果就绪就保持移除状态(在上面已经将就绪的context唤醒了，这里不用管了，那些就绪的future会从之前await的地方继续执行，然后结束)。
                        let task = self.wait_queue.borrow_mut().remove(&w.index);
                        if let Some(mut task) = task {
                            // if a task is not ready put it back
                            if let Poll::Pending = task.poll(w.waker) {
                                self.wait_queue.borrow_mut().insert(w.index, task);
                            }
                            // otherwise just drop it
                        }
                    }
                    None => break,
                }
            }

            //没任务的时候返回
            // stop the loop if no more tasks
            if self.wait_queue.borrow().is_empty() {
                return;
            }
        }
    }
}
