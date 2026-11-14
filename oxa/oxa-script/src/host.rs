//! A15: host-function binding, captured script functions, the runtime thread. Owner: task A15. Signatures below are frozen; bodies are yours.
use crate::runtime::{call_ref, state_ref};
use crate::{InterruptHandle, Limits, Runtime};
use oxa_proto::{codes, OxaError, Value};
use std::cell::Cell;
use std::sync::mpsc;
use std::sync::Mutex;
use std::thread::JoinHandle;

/// The callback of an upcall: serves a request from the script side.
type UpcallFn<'a> = dyn FnMut(&mut CallCx, &FnRef, &[Value]) -> Result<Value, OxaError> + 'a;

/// A host function. Runs on the runtime's thread, synchronously, while the script waits.
pub type HostFn = Box<dyn Fn(&mut CallCx, &[Value]) -> Result<Value, OxaError> + Send>;

/// A handle to a script function that was passed to a host function (bridged as
/// `{"$fn": <id>}`). Valid until released or until its runtime is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FnRef(pub u64);

impl FnRef {
    /// Key of the bridge object.
    pub const KEY: &'static str = "$fn";

    /// Read a bridged function value (`{"$fn": 7}`).
    pub fn from_value(v: &Value) -> Option<FnRef> {
        let o = v.as_object()?;
        if o.len() != 1 {
            return None;
        }
        o.get(Self::KEY)?.as_u64().map(FnRef)
    }

    /// The bridge object for this function.
    pub fn to_value(self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert(Self::KEY.to_string(), Value::from(self.0));
        Value::Object(m)
    }
}

/// What a host function can do while it runs.
pub struct CallCx<'a> {
    pub(crate) rt: &'a mut Runtime,
    pub(crate) up: Option<&'a UpcallLink>,
}

impl CallCx<'_> {
    /// Call back into the script (re-entrant, same deadline) and await the result.
    pub fn call(&mut self, f: &FnRef, args: &[Value]) -> Result<Value, OxaError> {
        let inner = state_ref(self.rt)
            .cloned()
            .ok_or_else(|| OxaError::new(codes::INTERNAL, "runtime state is missing"))?;
        call_ref(&inner, f, args)
    }

    /// Forget a captured function (see [`Runtime::release`]): a host function that received a
    /// function only for the duration of its own call releases it before it returns, so the
    /// runtime does not keep it alive. Unknown ids are ignored.
    pub fn release(&mut self, f: &FnRef) {
        self.rt.release(f);
    }

    /// Ask the thread that submitted the current job (see [`RuntimeThread::run`]) to serve
    /// `request`, and block until it answers. While blocked, [`CallbackPort::call`] requests
    /// from the serving side are executed here, in order. Errors: `script` when the runtime is
    /// not driven through a [`RuntimeThread`], `aborted` when the submitter went away.
    pub fn upcall(&mut self, request: Value) -> Result<Value, OxaError> {
        self.upcall_timed(request, false)
    }

    /// [`CallCx::upcall`] for a request that waits for the user (a dialog): the deadline clock
    /// of the outermost call stops while the submitter serves it, so the handler's budget is
    /// spent on script time only. Script callbacks served during the wait (a component's
    /// `render`) run against the remaining budget.
    pub fn upcall_paused(&mut self, request: Value) -> Result<Value, OxaError> {
        self.upcall_timed(request, true)
    }

    fn upcall_timed(&mut self, request: Value, paused: bool) -> Result<Value, OxaError> {
        // While paused, the deadline is lifted and the remaining budget kept aside; it is
        // re-armed around each callback and after the reply.
        let inner = if paused { state_ref(self.rt).cloned() } else { None };
        let mut remaining = inner
            .as_ref()
            .and_then(|i| i.deadline.get())
            .map(|t| t.saturating_duration_since(std::time::Instant::now()));
        let lift = |inner: &Option<std::rc::Rc<crate::runtime::Inner>>| {
            if let Some(i) = inner {
                i.deadline.set(None);
            }
        };
        let rearm = |inner: &Option<std::rc::Rc<crate::runtime::Inner>>,
                     left: Option<std::time::Duration>| {
            if let (Some(i), Some(left)) = (inner, left) {
                i.deadline.set(Some(std::time::Instant::now() + left));
            }
        };
        if remaining.is_some() {
            lift(&inner);
        }
        let out = self.upcall_inner(request, &mut |cx, f, args| {
            rearm(&inner, remaining);
            let started = std::time::Instant::now();
            let r = cx.call(f, args);
            remaining = remaining.map(|left| left.saturating_sub(started.elapsed()));
            if remaining.is_some() {
                lift(&inner);
            }
            r
        });
        rearm(&inner, remaining);
        out
    }

    fn upcall_inner(&mut self, request: Value, callback: &mut UpcallFn<'_>) -> Result<Value, OxaError> {
        let Some(link) = self.up else {
            return Err(OxaError::new(
                codes::SCRIPT,
                "this runtime is not driven through a RuntimeThread, so it cannot make upcalls",
            ));
        };
        link.to_submitter
            .send(UpMsg::Request(request))
            .map_err(|_| OxaError::new(codes::ABORTED, "the job submitter went away"))?;
        loop {
            match link.from_submitter.recv() {
                Ok(DownMsg::Reply(v)) => return Ok(v),
                Ok(DownMsg::Callback(f, args)) => {
                    let r = callback(self, &f, &args);
                    let _ = link.to_submitter.send(UpMsg::CallbackResult(r));
                }
                Err(_) => return Err(OxaError::new(codes::ABORTED, "the job submitter went away")),
            }
        }
    }

    /// Name of the module whose code made this host call, when known.
    pub fn module(&self) -> Option<&str> {
        let inner = state_ref(self.rt)?;
        // SAFETY: the cell is only written by the host-call machinery, which is not running
        // while this shared borrow of `self` (and thus of the runtime) is alive.
        let slot: &Option<String> = unsafe { &*inner.cur_module.as_ptr() };
        slot.as_deref()
    }
}

/// Private wiring between a blocked host function and the job's submitter.
pub(crate) struct UpcallLink {
    pub(crate) to_submitter: mpsc::Sender<UpMsg>,
    pub(crate) from_submitter: mpsc::Receiver<DownMsg>,
}

pub(crate) enum UpMsg {
    Request(Value),
    CallbackResult(Result<Value, OxaError>),
}

pub(crate) enum DownMsg {
    Reply(Value),
    Callback(FnRef, Vec<Value>),
}

thread_local! {
    /// The upcall link of the job running on this thread, null when none runs.
    static CURRENT_LINK: Cell<*const UpcallLink> = const { Cell::new(std::ptr::null()) };
    /// The slot most recently created or fetched on this thread (for panic cleanup).
    static LAST_SLOT: Cell<Option<SlotId>> = const { Cell::new(None) };
}

/// The upcall link of the job running on this thread, if any.
pub(crate) fn current_link<'a>() -> Option<&'a UpcallLink> {
    let p = CURRENT_LINK.with(|c| c.get());
    // SAFETY: the pointer is set by the runtime thread loop for the duration of one job, and the
    // link it points to lives on that loop's stack for exactly that duration.
    unsafe { p.as_ref() }
}

/// Given to the `serve` closure of [`RuntimeThread::run`]: lets the serving side call script
/// functions while the script is blocked in [`CallCx::upcall`] (this is how a JS component's
/// `render(width)` is called from the UI thread). Cloneable and `Send`; it stops working
/// (`Err(script)`) as soon as the upcall it belongs to has been answered.
#[derive(Clone)]
pub struct CallbackPort {
    pub(crate) tx: mpsc::Sender<DownMsg>,
    pub(crate) rx: std::sync::Arc<Mutex<mpsc::Receiver<UpMsg>>>,
    /// True only while `serve` runs for an unanswered upcall. Read and cleared under the `rx`
    /// lock, so a call either completes before the upcall is answered or is refused.
    pub(crate) live: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl CallbackPort {
    pub fn call(&self, f: &FnRef, args: &[Value]) -> Result<Value, OxaError> {
        // The submitter holds `rx` while it waits for the next upcall, so a stale port must not
        // block on the lock: poll it, and give up as soon as the upcall has been answered.
        let stale = || OxaError::new(codes::SCRIPT, "callback port used after its upcall was answered");
        let rx = loop {
            if !self.live.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(stale());
            }
            match self.rx.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(std::time::Duration::from_millis(1))
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(OxaError::new(codes::SCRIPT, "callback port is poisoned"));
                }
            }
        };
        if !self.live.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(stale());
        }
        self.tx
            .send(DownMsg::Callback(*f, args.to_vec()))
            .map_err(|_| OxaError::new(codes::SCRIPT, "callback port is closed"))?;
        match rx.recv() {
            Ok(UpMsg::CallbackResult(r)) => r,
            Ok(UpMsg::Request(_)) => Err(OxaError::new(
                codes::SCRIPT,
                "a script upcall during a callback is not supported",
            )),
            Err(_) => Err(OxaError::new(codes::SCRIPT, "callback port is closed")),
        }
    }
}

/// Identifies one runtime on a [`RuntimeThread`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotId(pub u32);

/// The runtimes living on one [`RuntimeThread`] (one per extension, so a load error or a
/// crash isolates one extension).
pub struct Slots {
    pub(crate) next: u32,
    pub(crate) runtimes: Vec<(SlotId, Runtime)>,
}

impl Slots {
    pub fn create(&mut self, limits: Limits) -> Result<SlotId, OxaError> {
        let rt = Runtime::new(limits)?;
        let id = SlotId(self.next);
        self.next += 1;
        self.runtimes.push((id, rt));
        LAST_SLOT.with(|c| c.set(Some(id)));
        Ok(id)
    }
    pub fn get(&mut self, id: SlotId) -> Option<&mut Runtime> {
        LAST_SLOT.with(|c| c.set(Some(id)));
        self.runtimes.iter_mut().find(|(i, _)| *i == id).map(|(_, r)| r)
    }
    /// Drop the runtime and everything it holds (`/reload`). Unknown ids are ignored.
    pub fn remove(&mut self, id: SlotId) {
        self.runtimes.retain(|(i, _)| *i != id);
    }
    /// Drop every runtime.
    pub fn clear(&mut self) {
        self.runtimes.clear();
    }
    pub fn ids(&self) -> Vec<SlotId> {
        let mut ids: Vec<SlotId> = self.runtimes.iter().map(|(i, _)| *i).collect();
        ids.sort();
        ids
    }
}

pub(crate) type Job = Box<dyn FnOnce(&mut Slots, &UpcallLink) + Send>;

type JobMsg = (Job, mpsc::Sender<UpMsg>, mpsc::Receiver<DownMsg>);

/// A thread that owns script runtimes. `Send + Sync`; jobs from several threads are serialised
/// in arrival order. Dropping it drops all runtimes and joins the thread.
pub struct RuntimeThread {
    pub(crate) jobs: Mutex<Option<mpsc::Sender<JobMsg>>>,
    pub(crate) interrupts: Mutex<Vec<crate::InterruptHandle>>,
    pub(crate) join: Mutex<Option<JoinHandle<()>>>,
}

impl RuntimeThread {
    /// Start the thread (named `oxa-script:<name>`).
    pub fn spawn(name: &str) -> Result<RuntimeThread, OxaError> {
        let (jobs_tx, jobs_rx) = mpsc::channel::<JobMsg>();
        let join = std::thread::Builder::new()
            .name(format!("oxa-script:{name}"))
            .spawn(move || {
                let mut slots = Slots {
                    next: 0,
                    runtimes: Vec::new(),
                };
                while let Ok((job, up_tx, down_rx)) = jobs_rx.recv() {
                    let link = UpcallLink {
                        to_submitter: up_tx,
                        from_submitter: down_rx,
                    };
                    let prev = CURRENT_LINK.with(|c| c.replace(&link as *const UpcallLink));
                    job(&mut slots, &link);
                    CURRENT_LINK.with(|c| c.set(prev));
                    drop(link);
                }
                slots.clear();
            })
            .map_err(|e| OxaError::io("start the script thread", &e))?;
        Ok(RuntimeThread {
            jobs: Mutex::new(Some(jobs_tx)),
            interrupts: Mutex::new(Vec::new()),
            join: Mutex::new(Some(join)),
        })
    }

    /// Run `job` on the runtime thread and block until it returns.
    ///
    /// While it runs, every [`CallCx::upcall`] made by a host function is delivered to `serve`
    /// on the calling thread (this is how scripts reach the caller's `&mut dyn Ui`); `serve`'s
    /// return value is the upcall's result. Pass `&mut |_, _| Value::Null` when no upcalls are
    /// expected.
    ///
    /// Errors: `script` when the thread is gone or the job panicked (the panic is caught; the
    /// slot that was running is removed).
    pub fn run<R, F>(
        &self,
        job: F,
        serve: &mut dyn FnMut(Value, &CallbackPort) -> Value,
    ) -> Result<R, OxaError>
    where
        R: Send + 'static,
        F: FnOnce(&mut Slots) -> R + Send + 'static,
    {
        let (res_tx, res_rx) = mpsc::channel::<(Result<R, OxaError>, Vec<InterruptHandle>)>();
        let (up_tx, up_rx) = mpsc::channel::<UpMsg>();
        let (down_tx, down_rx) = mpsc::channel::<DownMsg>();
        let wrapped: Job = Box::new(move |slots: &mut Slots, _link: &UpcallLink| {
            let out = oxa_proto::panic::contain(|| job(slots));
            let result = match out {
                Ok(v) => Ok(v),
                Err(error) => {
                    if let Some(id) = LAST_SLOT.with(|c| c.get()) {
                        slots.remove(id);
                    }
                    Err(OxaError::new(
                        codes::SCRIPT,
                        format!("the script job panicked: {error}"),
                    ))
                }
            };
            let handles = slots.runtimes.iter().map(|(_, r)| r.interrupt_handle()).collect();
            let _ = res_tx.send((result, handles));
        });
        {
            let jobs = self
                .jobs
                .lock()
                .map_err(|_| OxaError::new(codes::SCRIPT, "script thread state is poisoned"))?;
            let tx = jobs
                .as_ref()
                .ok_or_else(|| OxaError::new(codes::SCRIPT, "script thread was shut down"))?;
            tx.send((wrapped, up_tx, down_rx))
                .map_err(|_| OxaError::new(codes::SCRIPT, "script thread is gone"))?;
        }
        let port = CallbackPort {
            tx: down_tx.clone(),
            rx: std::sync::Arc::new(Mutex::new(up_rx)),
            live: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        loop {
            let msg = match port.rx.lock() {
                Ok(guard) => guard.recv(),
                Err(_) => break,
            };
            match msg {
                Ok(UpMsg::Request(req)) => {
                    port.live.store(true, std::sync::atomic::Ordering::SeqCst);
                    let answer = serve(req, &port);
                    {
                        // Wait out a call in flight on another thread, then close the port.
                        let _guard = port.rx.lock();
                        port.live.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                    if down_tx.send(DownMsg::Reply(answer)).is_err() {
                        break;
                    }
                }
                Ok(UpMsg::CallbackResult(_)) => {}
                // The job has returned and dropped its link: nothing more to serve.
                Err(_) => break,
            }
        }
        let (result, handles) = res_rx
            .recv()
            .map_err(|_| OxaError::new(codes::SCRIPT, "script thread stopped during the job"))?;
        if let Ok(mut g) = self.interrupts.lock() {
            *g = handles;
        }
        result
    }

    /// Interrupt whatever is running on the thread (abort, shutdown). Safe from any thread.
    pub fn interrupt(&self) {
        if let Ok(list) = self.interrupts.lock() {
            for h in list.iter() {
                h.interrupt();
            }
        }
    }

    /// Drop all runtimes and stop the thread. Idempotent.
    pub fn shutdown(&self) {
        self.interrupt();
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.take();
        }
        let handle = self.join.lock().ok().and_then(|mut j| j.take());
        if let Some(h) = handle {
            if h.thread().id() != std::thread::current().id() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for RuntimeThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}
