//! Runtime ownership and RPC. Driver and Session retain only owned wire values;
//! PHP exception conversion is called by the PHP-thread facade.
use crate::{
    bridge::{build_facade, MessageQueue, RUNTIME_JS},
    error::{self, HostError, JsErrorParts},
    marshal::{js_to_data, js_to_middle, middle_into_js, MiddleValue as Data},
    transpile::{TranspileCache, Transpiled},
};
use ext_php_rs::prelude::PhpException;
use rquickjs::{function::This, Context, Ctx, Exception, Function, Promise, Runtime, Value};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    fs::File,
    io::Write,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
fn notification_pair() -> std::io::Result<(File, File)> {
    use std::os::fd::FromRawFd;
    unsafe extern "C" {
        fn php_quickjs_notification_fd() -> i32;
    }
    let fd = unsafe { php_quickjs_notification_fd() };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // The PHP thread owns the reader until after joining the writer's thread.
    let reader = unsafe { File::from_raw_fd(fd) };
    let writer = reader.try_clone()?;
    Ok((reader, writer))
}
#[cfg(not(target_os = "linux"))]
fn notification_pair() -> std::io::Result<(File, File)> {
    use std::os::{fd::OwnedFd, unix::net::UnixStream};
    let (reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    Ok((
        File::from(OwnedFd::from(reader)),
        File::from(OwnedFd::from(writer)),
    ))
}

pub struct Config {
    pub memory_limit: usize,
    pub timeout_ms: u64,
    pub max_stack: usize,
    pub isolated: bool,
    pub max_queued_message_bytes: usize,
    pub cache_entries: usize,
    pub cache_bytes: usize,
}
// Each direction writes different counters. Keep them off one cache line.
#[derive(Default)]
#[repr(align(128))]
struct Signal<T>(T);
impl<T> std::ops::Deref for Signal<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}
#[derive(Default)]
struct Signals {
    stop: Signal<AtomicBool>,
    notified: Signal<AtomicBool>,
    pump: Signal<AtomicBool>,
    posts: Signal<AtomicUsize>,
}
pub enum Operation {
    Eval {
        code: String,
        typescript: bool,
    },
    Call {
        id: u64,
        args: Vec<Data>,
        posted: bool,
    },
    Capacity,
    Jobs(i64),
    HasJobs,
    Roundtrip(Data),
}
pub struct Request {
    pub id: u64,
    pub op: Operation,
    pub names: Option<Vec<String>>,
    pub deletions: Vec<u64>,
    pub parent: Option<u64>,
}
pub enum HostTarget {
    Named(String),
    Function(u64),
}
pub enum Command {
    Pump,
    Request(Request),
    HostReply {
        token: u64,
        result: Result<Data, HostError>,
    },
    Abort {
        id: u64,
        message: String,
    },
    Stop,
}
pub enum Event {
    Messages,
    AsyncError(WireError),
    Fatal(WireError),
    Done {
        id: u64,
        result: Result<Data, WireError>,
    },
    Host {
        owner: Option<u64>,
        token: u64,
        target: HostTarget,
        args: Vec<Data>,
    },
    NeedsLoop {
        id: u64,
    },
    Stopped(String),
}
pub enum WireError {
    Internal(String),
    Timeout,
    Guest {
        parts: JsErrorParts,
        module: Option<String>,
        map: Option<String>,
        eval: bool,
    },
    Syntax {
        message: String,
        line: u32,
        col: u32,
    },
}
impl WireError {
    pub fn into_php(self) -> PhpException {
        match self {
            Self::Internal(s) => PhpException::from_message(s),
            Self::Timeout => {
                PhpException::from_class::<crate::exceptions::QuickJSTimeoutException>(
                    "JavaScript execution timed out".into(),
                )
            }
            Self::Syntax { message, line, col } => crate::exceptions::eval_exception(
                "SyntaxError".into(),
                message,
                "guest.ts",
                line,
                format!("    at guest.ts:{line}:{col}"),
            ),
            Self::Guest {
                parts,
                module,
                map,
                eval,
            } => {
                if !eval {
                    return error::js_error_to_php(parts);
                }
                let message = parts.display_message();
                if matches!(
                    error::resource_error(&parts, false),
                    Some(error::ResourceError::Memory)
                ) {
                    return PhpException::from_class::<crate::exceptions::QuickJSMemoryException>(
                        message,
                    );
                }
                let module = module.unwrap_or_else(|| "guest.js".into());
                let stack = match (parts.stack.as_deref(), map.as_deref()) {
                    (Some(s), Some(m)) => error::remap_stack(s, m, &module),
                    (Some(s), None) => Some(s.into()),
                    _ => None,
                };
                let line = stack
                    .as_deref()
                    .and_then(|s| error::top_frame_location(s, &module))
                    .map_or(0, |x| x.0);
                crate::exceptions::eval_exception(
                    parts.name,
                    message,
                    &module,
                    line,
                    stack.unwrap_or_default(),
                )
            }
        }
    }
}
pub struct Port {
    inner: std::mem::ManuallyDrop<PortInner>,
    pid: u32,
}
pub struct PortInner {
    tx: mpsc::SyncSender<Command>,
    rx: mpsc::Receiver<Event>,
    pub wake: File,
    worker: RefCell<Option<JoinHandle<()>>>,
    signals: Arc<Signals>,
    messages: Arc<Mutex<MessageQueue>>,
}
impl std::ops::Deref for Port {
    type Target = PortInner;
    fn deref(&self) -> &PortInner {
        &self.inner
    }
}
impl Port {
    pub fn new(config: Config) -> Result<Self, WireError> {
        let (wake, notify) = notification_pair().map_err(|e| WireError::Internal(e.to_string()))?;
        let (tx, commands) = mpsc::sync_channel(256);
        let (events, rx) = mpsc::channel();
        let signals = Arc::new(Signals::default());
        let worker_signals = signals.clone();
        let messages = Arc::new(Mutex::new(MessageQueue::new(
            config.max_queued_message_bytes,
        )));
        let queue = messages.clone();
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("php-quickjs".into())
            .stack_size(
                config
                    .max_stack
                    .saturating_add(4 * 1024 * 1024)
                    .max(8 * 1024 * 1024),
            )
            .spawn(move || {
                let driver = Rc::new(Driver {
                    commands,
                    events,
                    notify,
                    signals: worker_signals,
                    messages: queue,
                    cache: TranspileCache::new(config.cache_entries, config.cache_bytes),
                    config,
                    token: Cell::new(0),
                    notification_pending: Cell::new(false),
                });
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.run(ready_tx)));
                driver.signals.stop.store(true, Ordering::SeqCst);
                if !matches!(outcome, Ok(Ok(()))) {
                    driver.emit(Event::Stopped(
                        "QuickJS worker terminated unexpectedly".into(),
                    ));
                }
                driver.notify();
            })
            .map_err(|e| WireError::Internal(e.to_string()))?;
        let initialized = ready.recv().unwrap_or_else(|_| {
            Err(WireError::Internal(
                "QuickJS worker initialization failed".into(),
            ))
        });
        if let Err(error) = initialized {
            signals.stop.store(true, Ordering::SeqCst);
            let _ = tx.try_send(Command::Stop);
            let _ = worker.join();
            return Err(error);
        }
        Ok(Self {
            inner: std::mem::ManuallyDrop::new(PortInner {
                tx,
                rx,
                wake,
                worker: RefCell::new(Some(worker)),
                signals,
                messages,
            }),
            pid: std::process::id(),
        })
    }
    pub fn check_pid(&self) -> Result<(), WireError> {
        if self.pid != std::process::id() {
            return Err(WireError::Internal(
                "QuickJS cannot be used after fork; create runtimes in the child".into(),
            ));
        }
        Ok(())
    }
    pub fn check(&self) -> Result<(), WireError> {
        self.check_pid()?;
        if self.signals.stop.load(Ordering::SeqCst) {
            return Err(WireError::Internal("QuickJS worker is closed".into()));
        }
        Ok(())
    }
    pub fn send(&self, command: Command) -> Result<(), WireError> {
        self.check()?;
        self.tx
            .try_send(command)
            .map_err(|_| WireError::Internal("QuickJS command queue is full or closed".into()))?;
        Ok(())
    }
    pub fn receive(&self) -> Result<Event, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
    pub fn acknowledge_notification(&self) {
        // Clear before draining events: a concurrent enqueue either appears in
        // this drain or writes another byte. Neither ordering loses a wakeup.
        self.signals.notified.store(false, Ordering::SeqCst);
    }
    pub fn start_pump(&self) -> Result<(), WireError> {
        self.check()?;
        if !self.signals.pump.load(Ordering::SeqCst)
            && !self.signals.pump.swap(true, Ordering::SeqCst)
        {
            self.send(Command::Pump)?;
        }
        Ok(())
    }
    pub fn reserve_post(&self) -> bool {
        self.signals
            .posts
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < 128).then_some(n + 1)
            })
            .is_ok()
    }
    pub fn release_post(&self) {
        self.signals.posts.fetch_sub(1, Ordering::Relaxed);
    }
    pub fn receive_blocking(&self) -> Result<Event, WireError> {
        self.rx
            .recv()
            .map_err(|_| WireError::Internal("QuickJS worker disconnected".into()))
    }
    pub fn drain_messages(&self) -> Result<Vec<Data>, WireError> {
        self.check_pid()?;
        Ok(self
            .messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain())
    }
    pub fn close(&self) {
        if self.pid != std::process::id() {
            return;
        }
        self.signals.stop.store(true, Ordering::SeqCst);
        let _ = self.tx.try_send(Command::Stop);
        if let Some(worker) = self.worker.borrow_mut().take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Port {
    fn drop(&mut self) {
        if self.pid == std::process::id() {
            self.close();
            // SAFETY: the owner has joined its worker; inner is dropped exactly once.
            unsafe {
                std::mem::ManuallyDrop::drop(&mut self.inner);
            }
        }
        // After fork, do not touch inherited channel/mutex state: its lock owner
        // may no longer exist. Child process exit reclaims these inherited resources.
    }
}
struct Driver {
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    notify: File,
    signals: Arc<Signals>,
    messages: Arc<Mutex<MessageQueue>>,
    cache: TranspileCache,
    config: Config,
    token: Cell<u64>,
    notification_pending: Cell<bool>,
}
impl Driver {
    fn notify(&self) {
        if !self.signals.notified.swap(true, Ordering::SeqCst) {
            let _ = (&self.notify).write(&1u64.to_ne_bytes());
        }
    }
    fn emit(&self, event: Event) {
        if matches!(&event, Event::Fatal(_)) && self.signals.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        let urgent = !self.signals.pump.load(Ordering::SeqCst)
            || matches!(
                &event,
                Event::Host { .. } | Event::Stopped(_) | Event::AsyncError(_) | Event::Fatal(_)
            );
        let _ = self.events.send(event);
        self.notification_pending.set(true);
        if urgent {
            self.flush_notifications();
        }
    }
    fn flush_notifications(&self) {
        if self.notification_pending.replace(false) {
            self.notify();
        }
    }
    fn run(self: &Rc<Self>, ready: mpsc::SyncSender<Result<(), WireError>>) -> Result<(), String> {
        if self.config.isolated {
            let _ = ready.send(Ok(()));
            loop {
                let command = self.commands.recv().map_err(|e| e.to_string())?;
                if self.signals.stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
                match command {
                    Command::Stop => return Ok(()),
                    Command::Request(request) => {
                        let id = request.id;
                        if !matches!(
                            &request.op,
                            Operation::Eval { .. } | Operation::Roundtrip(_)
                        ) {
                            self.emit(Event::Done {
                                id,
                                result: Err(WireError::Internal(
                                    "JS callback belongs to an ended isolated eval".into(),
                                )),
                            });
                            continue;
                        }
                        self.context(Some(request), None)?;
                    }
                    _ => {}
                }
            }
        } else {
            self.context(None, Some(ready))
        }
    }
    fn context(
        self: &Rc<Self>,
        first: Option<Request>,
        ready: Option<mpsc::SyncSender<Result<(), WireError>>>,
    ) -> Result<(), String> {
        let rt = Runtime::new().map_err(|e| e.to_string())?;
        if self.config.memory_limit > 0 {
            rt.set_memory_limit(self.config.memory_limit);
        }
        if self.config.max_stack > 0 {
            rt.set_max_stack_size(self.config.max_stack);
        }
        let deadline = Rc::new(Cell::new(None::<Instant>));
        let timed_out = Rc::new(Cell::new(false));
        let interrupted = self.signals.clone();
        let dl = deadline.clone();
        let expired = timed_out.clone();
        rt.set_interrupt_handler(Some(Box::new(move || {
            if interrupted.stop.load(Ordering::SeqCst) {
                return true;
            }
            if dl.get().is_some_and(|d| Instant::now() >= d) {
                expired.set(true);
                return true;
            }
            false
        })));
        let context = Context::full(&rt).map_err(|e| e.to_string())?;
        let root = first.as_ref().map(|r| r.id);
        context.with(|ctx| {
            let promise: rquickjs::Object =
                ctx.globals().get("Promise").map_err(|e| e.to_string())?;
            let prototype: rquickjs::Object =
                promise.get("prototype").map_err(|e| e.to_string())?;
            let then: Function = prototype.get("then").map_err(|e| e.to_string())?;
            let session = Rc::new(Session {
                driver: self.clone(),
                pending: RefCell::new(HashMap::new()),
                ready: Rc::new(RefCell::new(VecDeque::new())),
                then: RefCell::new(Some(then)),
                deferred_ready: RefCell::new(Vec::new()),
                turn_deadline: Cell::new(None),
                posted: RefCell::new(HashSet::new()),
                capacity: RefCell::new(VecDeque::new()),
                replies: RefCell::new(HashMap::new()),
                entries: RefCell::new(Vec::new()),
                deadline,
                timed_out,
                failed: RefCell::new(VecDeque::new()),
                root,
                root_finished: Cell::new(false),
                facade: RefCell::new(None),
                functions: RefCell::new(None),
                js_depth: Cell::new(0),
                reported_failure: Cell::new(false),
                active_request: Cell::new(None),
            });
            let _cleanup = SessionCleanup(session.clone());
            install(&ctx, session.clone()).map_err(|e| error::js_error_message(&ctx, e))?;
            if let Some(ready) = ready {
                let _ = ready.send(Ok(()));
            }
            if let Some(first) = first {
                session.request(&ctx, first);
            }
            while session.step(&ctx, None) {}
            let pending = std::mem::take(&mut *session.pending.borrow_mut());
            for (id, _) in pending {
                self.emit(Event::Done {
                    id,
                    result: Err(WireError::Internal(
                        if self.signals.stop.load(Ordering::SeqCst) {
                            "QuickJS worker is closed".into()
                        } else {
                            "JS callback canceled: owning isolated eval ended".into()
                        },
                    )),
                });
            }
            session.replies.borrow_mut().clear();
            Ok(())
        })
    }
}
#[derive(Clone, Default)]
struct ErrorContext {
    module: Option<Transpiled>,
    eval: bool,
}
struct Pending<'js> {
    id: u64,
    parent: Option<u64>,
    promise: Promise<'js>,
    context: ErrorContext,
    deadline: Option<Instant>,
    asked: Cell<bool>,
    settle_depth: usize,
}
struct Session<'js> {
    driver: Rc<Driver>,
    pending: RefCell<HashMap<u64, Rc<Pending<'js>>>>,
    ready: Rc<RefCell<VecDeque<u64>>>,
    then: RefCell<Option<Function<'js>>>,
    deferred_ready: RefCell<Vec<u64>>,
    turn_deadline: Cell<Option<Instant>>,
    posted: RefCell<HashSet<u64>>,
    capacity: RefCell<VecDeque<(u64, Option<u64>)>>,
    replies: RefCell<HashMap<u64, Option<Result<Data, HostError>>>>,
    entries: RefCell<Vec<Option<Instant>>>,
    deadline: Rc<Cell<Option<Instant>>>,
    timed_out: Rc<Cell<bool>>,
    failed: RefCell<VecDeque<u64>>,
    root: Option<u64>,
    root_finished: Cell<bool>,
    facade: RefCell<Option<rquickjs::Object<'js>>>,
    functions: RefCell<Option<rquickjs::Object<'js>>>,
    js_depth: Cell<usize>,
    reported_failure: Cell<bool>,
    active_request: Cell<Option<u64>>,
}
struct RequestScope<'a>(&'a Cell<Option<u64>>, Option<u64>);
impl Drop for RequestScope<'_> {
    fn drop(&mut self) {
        self.0.set(self.1);
    }
}
// Native closures retain Session. Break its JS roots before the context is freed,
// including panic unwinding; otherwise a cached facade could retain the runtime.
struct SessionCleanup<'js>(Rc<Session<'js>>);
struct JsEntry<'a, 'js> {
    session: &'a Session<'js>,
    depth: bool,
    budget: bool,
}
impl Drop for JsEntry<'_, '_> {
    fn drop(&mut self) {
        if self.depth {
            self.session.js_depth.set(self.session.js_depth.get() - 1);
            let deferred = std::mem::take(&mut *self.session.deferred_ready.borrow_mut());
            if !deferred.is_empty() {
                self.session.ready.borrow_mut().extend(deferred);
            }
        }
        if self.budget {
            self.session.entries.borrow_mut().pop();
            self.session.update_deadline();
        }
    }
}
impl Drop for SessionCleanup<'_> {
    fn drop(&mut self) {
        self.0.pending.borrow_mut().clear();
        self.0.facade.borrow_mut().take();
        self.0.functions.borrow_mut().take();
        self.0.then.borrow_mut().take();
        self.0.ready.borrow_mut().clear();
        self.0.deferred_ready.borrow_mut().clear();
    }
}
fn jobs_pending(ctx: &Ctx<'_>) -> bool {
    unsafe { rquickjs::qjs::JS_IsJobPending(rquickjs::qjs::JS_GetRuntime(ctx.as_raw().as_ptr())) }
}
impl<'js> Session<'js> {
    fn enter(&self, deadline: Option<Instant>, depth: bool) -> JsEntry<'_, 'js> {
        let budget = self.driver.config.timeout_ms > 0;
        if budget {
            self.entries.borrow_mut().push(deadline);
            self.update_deadline();
        }
        if depth {
            self.js_depth.set(self.js_depth.get() + 1);
        }
        JsEntry {
            session: self,
            depth,
            budget,
        }
    }
    fn finish_value(
        &self,
        id: u64,
        ctx: &Ctx<'js>,
        result: rquickjs::Result<Value<'js>>,
        context: &ErrorContext,
    ) {
        let result = result
            .and_then(|value| {
                if self.posted.borrow().contains(&id) {
                    Ok(Data::Null)
                } else {
                    js_to_middle(ctx, value)
                }
            })
            .map_err(|e| self.error(ctx, e, context));
        self.finish(id, result);
    }
    fn update_deadline(&self) {
        if self.driver.config.timeout_ms == 0 {
            return;
        }
        let entries = self.entries.borrow();
        // An independent pending Promise must not shorten a new synchronous entry.
        // Awaited jobs inherit their own active budget through the entry stack.
        self.deadline.set(if entries.is_empty() {
            self.pending
                .borrow()
                .values()
                .filter_map(|p| p.deadline)
                .min()
                .or(self.turn_deadline.get())
        } else {
            entries.iter().copied().flatten().min()
        });
    }
    fn error(&self, ctx: &Ctx<'js>, e: rquickjs::Error, context: &ErrorContext) -> WireError {
        let parts = error::js_error_parts(ctx, e);
        if self.timed_out.replace(false) {
            return WireError::Timeout;
        }
        WireError::Guest {
            parts,
            module: context.module.as_ref().map(|m| m.module_id.clone()),
            map: context
                .module
                .as_ref()
                .and_then(|m| m.map_json.as_ref().map(|s| s.to_string())),
            eval: context.eval,
        }
    }
    fn finish(&self, id: u64, result: Result<Data, WireError>) {
        self.pending.borrow_mut().remove(&id);
        if result.is_err() {
            let mut failed = self.failed.borrow_mut();
            failed.push_back(id);
            if failed.len() > 256 {
                failed.pop_front();
            }
            drop(failed);
            let mut canceled = Vec::new();
            self.pending.borrow_mut().retain(|_, p| {
                if p.parent == Some(id) {
                    canceled.push(p.id);
                    false
                } else {
                    true
                }
            });
            self.capacity.borrow_mut().retain(|(child, parent)| {
                if *parent == Some(id) {
                    canceled.push(*child);
                    false
                } else {
                    true
                }
            });
            for id in canceled {
                self.finish(
                    id,
                    Err(WireError::Internal(
                        "JS callback canceled: owning call ended".into(),
                    )),
                );
            }
        }
        let posted = self.posted.borrow_mut().remove(&id);
        if posted {
            self.driver.signals.posts.fetch_sub(1, Ordering::Relaxed);
            if let Err(error) = result {
                self.driver.emit(Event::Fatal(error));
            }
            let available =
                128usize.saturating_sub(self.driver.signals.posts.load(Ordering::Relaxed));
            let ready = {
                let mut capacity = self.capacity.borrow_mut();
                let n = available.min(capacity.len());
                capacity.drain(..n).map(|(id, _)| id).collect::<Vec<_>>()
            };
            for id in ready {
                self.finish(id, Ok(Data::Null));
            }
        } else {
            self.driver.emit(Event::Done { id, result });
        }
        if self.root == Some(id) {
            self.root_finished.set(true)
        }
    }
    fn command(&self, ctx: &Ctx<'js>, command: Command) -> bool {
        match command {
            Command::Pump => {}
            Command::Stop => return false,
            Command::Request(request) => self.request(ctx, request),
            Command::HostReply { token, result } => {
                if let Some(reply) = self.replies.borrow_mut().get_mut(&token) {
                    *reply = Some(result);
                }
            }
            Command::Abort { id, message } => {
                self.finish(id, Err(WireError::Internal(message)));
                self.update_deadline();
            }
        }
        true
    }
    fn wait(&self, ctx: &Ctx<'js>) -> bool {
        self.driver.flush_notifications();
        if !self.ready.borrow().is_empty() {
            return true;
        }
        let command = match self.deadline.get() {
            Some(deadline) => match self
                .driver
                .commands
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => return true,
                Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            },
            None => match self.driver.commands.recv() {
                Ok(command) => command,
                Err(_) => return false,
            },
        };
        self.command(ctx, command)
    }
    fn request(&self, ctx: &Ctx<'js>, request: Request) {
        let _owner = RequestScope(
            &self.active_request,
            self.active_request.replace(Some(request.id)),
        );
        let Request {
            id,
            op,
            names,
            deletions,
            parent,
        } = request;
        if matches!(&op, Operation::Call { posted: true, .. }) {
            self.posted.borrow_mut().insert(id);
        }
        if parent.is_some_and(|p| self.failed.borrow().contains(&p)) {
            self.finish(
                id,
                Err(WireError::Internal(
                    "JS callback canceled: owning call ended".into(),
                )),
            );
            return;
        }
        if matches!(&op, Operation::Capacity) {
            if self.driver.signals.posts.load(Ordering::Relaxed) < 128 {
                self.finish(id, Ok(Data::Null));
            } else {
                self.capacity.borrow_mut().push_back((id, parent));
            }
            return;
        }
        let inherited = if self.driver.config.timeout_ms == 0 {
            None
        } else {
            self.entries
                .borrow()
                .iter()
                .copied()
                .flatten()
                .chain(
                    self.pending
                        .borrow()
                        .values()
                        .filter(|p| Some(p.id) == parent)
                        .filter_map(|p| p.deadline),
                )
                .min()
        };
        let deadline = inherited.or_else(|| {
            (self.driver.config.timeout_ms > 0)
                .then(|| Instant::now() + Duration::from_millis(self.driver.config.timeout_ms))
        });
        if deadline.is_some_and(|d| Instant::now() >= d) {
            self.finish(id, Err(WireError::Timeout));
            return;
        }
        if self.js_depth.get() >= crate::engine::MAX_DEPTH {
            self.finish(
                id,
                Err(WireError::Internal(
                    "maximum bridge re-entrancy depth exceeded".into(),
                )),
            );
            return;
        }
        let _entry = self.enter(deadline, true);
        self.timed_out.set(false);
        let await_result = matches!(&op, Operation::Eval { .. } | Operation::Call { .. });
        let context = ErrorContext {
            eval: matches!(&op, Operation::Eval { .. }),
            module: None,
        };
        let (value, context) = match self.start(ctx, op, names, &deletions, context) {
            Ok(result) => result,
            Err(error) => return self.finish(id, Err(error)),
        };
        if self.timed_out.replace(false) || deadline.is_some_and(|d| Instant::now() >= d) {
            return self.finish(id, Err(WireError::Timeout));
        }
        let result = (|| {
            if await_result {
                let promise = match value.as_promise() {
                    Some(promise) => Some(promise.clone()),
                    None if value.is_object() => {
                        let normalize: Function = ctx.globals().get("__asPromise")?;
                        normalize.call((value.clone(),))?
                    }
                    None => None,
                };
                if let Some(promise) = promise {
                    if let Some(result) = promise.result::<Value>() {
                        return result.map(Some);
                    }
                    self.pending.borrow_mut().insert(
                        id,
                        Rc::new(Pending {
                            id,
                            parent,
                            promise: promise.clone(),
                            context: context.clone(),
                            deadline,
                            asked: Cell::new(false),
                            settle_depth: self.js_depth.get() - 1,
                        }),
                    );
                    let ready = self.ready.clone();
                    let notify = Function::new(ctx.clone(), move || {
                        ready.borrow_mut().push_back(id);
                    })?;
                    // Observe the native Promise with the pristine intrinsic,
                    // without a Rust executor or guest-overridable .then lookup.
                    let then = self.then.borrow().as_ref().unwrap().clone();
                    then.call::<_, ()>((This(promise), notify.clone(), notify))?;
                    return Ok(None);
                }
            }
            Ok(Some(value))
        })();
        if let Some(result) = result.transpose() {
            self.finish_value(id, ctx, result, &context);
        }
    }
    fn start(
        &self,
        ctx: &Ctx<'js>,
        op: Operation,
        names: Option<Vec<String>>,
        deletions: &[u64],
        mut context: ErrorContext,
    ) -> Result<(Value<'js>, ErrorContext), WireError> {
        let prep = (|| -> rquickjs::Result<()> {
            if !deletions.is_empty() {
                let functions = self.functions.borrow().as_ref().unwrap().clone();
                for id in deletions {
                    functions.remove(*id as f64)?;
                }
            }
            if names.is_none() && !matches!(&op, Operation::Eval { .. }) {
                return Ok(());
            }
            let cached = self
                .facade
                .borrow()
                .as_ref()
                .filter(|_| names.is_none())
                .cloned();
            if let Some(object) = cached {
                ctx.globals().set("php", object)?;
            } else {
                let names = names.unwrap_or_default();
                ctx.eval::<(), _>(build_facade(&names))?;
                let object = ctx.globals().get("php")?;
                *self.facade.borrow_mut() = Some(object);
            }
            Ok(())
        })();
        prep.map_err(|e| self.error(ctx, e, &context))?;
        let value = match op {
            Operation::Eval { code, typescript } => {
                let module = if typescript {
                    self.driver
                        .cache
                        .get_or_transpile(&code)
                        .map_err(|e| WireError::Syntax {
                            message: e.message,
                            line: e.line,
                            col: e.col,
                        })?
                } else {
                    Transpiled {
                        module_id: "guest.js".into(),
                        js: Rc::from(code),
                        map_json: None,
                    }
                };
                context.module = Some(module.clone());
                let mut options = rquickjs::context::EvalOptions::default();
                options.filename = Some(module.module_id.clone());
                ctx.eval_with_options(module.js.as_bytes(), options)
                    .map_err(|e| self.error(ctx, e, &context))?
            }
            Operation::Call { id, args, .. } => {
                let result = (|| {
                    let function: Function =
                        self.functions.borrow().as_ref().unwrap().get(id as f64)?;
                    let mut call_args = rquickjs::function::Args::new(ctx.clone(), args.len());
                    for arg in args {
                        call_args.push_arg(middle_into_js(ctx, arg)?)?;
                    }
                    function.call_arg(call_args)
                })();
                result.map_err(|e| self.error(ctx, e, &context))?
            }
            Operation::Jobs(max) => {
                let mut n = 0;
                while n < max {
                    if self.deadline.get().is_some_and(|d| Instant::now() >= d) {
                        return Err(WireError::Timeout);
                    }
                    if !self.job(ctx, &context)? {
                        break;
                    }
                    n += 1;
                    self.settle(ctx);
                }
                Value::new_float(ctx.clone(), n as f64)
            }
            Operation::HasJobs => Value::new_bool(ctx.clone(), jobs_pending(ctx)),
            Operation::Capacity => unreachable!("capacity requests do not enter JS"),
            Operation::Roundtrip(value) => {
                middle_into_js(ctx, value).map_err(|e| self.error(ctx, e, &context))?
            }
        };
        Ok((value, context))
    }
    fn job(&self, ctx: &Ctx<'js>, context: &ErrorContext) -> Result<bool, WireError> {
        let mut job_ctx = std::ptr::null_mut();
        let entry = self.enter(self.deadline.get(), true);
        let result = unsafe {
            rquickjs::qjs::JS_ExecutePendingJob(
                rquickjs::qjs::JS_GetRuntime(ctx.as_raw().as_ptr()),
                &mut job_ctx,
            )
        };
        drop(entry);
        if result < 0 {
            return Err(self.error(ctx, rquickjs::Error::Exception, context));
        }
        if self.timed_out.replace(false) {
            return Err(WireError::Timeout);
        }
        Ok(result > 0)
    }
    fn settle(&self, ctx: &Ctx<'js>) {
        if self.pending.borrow().is_empty() {
            self.ready.borrow_mut().clear();
            return;
        }
        let ready = std::mem::take(&mut *self.ready.borrow_mut());
        for id in ready {
            let item = self.pending.borrow().get(&id).cloned();
            let Some(item) = item else { continue };
            if item.settle_depth < self.js_depth.get() {
                self.deferred_ready.borrow_mut().push(id);
                continue;
            }
            if let Some(result) = item.promise.result::<Value>() {
                self.finish_value(id, ctx, result, &item.context);
            }
        }
        if self.driver.config.timeout_ms > 0 {
            let expired = self
                .pending
                .borrow()
                .values()
                .filter(|p| p.deadline.is_some_and(|d| Instant::now() >= d))
                .map(|p| p.id)
                .collect::<Vec<_>>();
            for id in expired {
                self.finish(id, Err(WireError::Timeout));
            }
        }
        self.update_deadline();
    }
    fn step(&self, ctx: &Ctx<'js>, host: Option<u64>) -> bool {
        if self.driver.signals.stop.load(Ordering::SeqCst) || self.root_finished.get() {
            return false;
        }
        let had_pending = !self.pending.borrow().is_empty();
        self.settle(ctx);
        if self.timed_out.replace(false) && !had_pending {
            self.driver.signals.pump.store(false, Ordering::SeqCst);
            self.driver.emit(Event::AsyncError(WireError::Timeout));
        }
        // Commands are JS turns. Checkpoint their microtasks before admitting
        // another command, but bound the batch so an infinite chain cannot
        // starve PHP replies, cancellation or close.
        for _ in 0..64 {
            if !self.has_awaitable()
                || !jobs_pending(ctx)
                || self.root_finished.get()
                || self.driver.signals.stop.load(Ordering::SeqCst)
            {
                break;
            }
            self.advance(ctx);
        }
        if self.root_finished.get() || self.driver.signals.stop.load(Ordering::SeqCst) {
            return false;
        }
        self.settle(ctx);
        // A nested job may consume an ancestor's reply. Do not sleep after
        // that reply has become ready on this same worker thread.
        if host.is_some_and(|id| self.replies.borrow().get(&id).is_some_and(Option::is_some)) {
            return true;
        }
        self.driver.flush_notifications();
        match self.driver.commands.try_recv() {
            Ok(command) => self.command(ctx, command),
            Err(mpsc::TryRecvError::Disconnected) => false,
            Err(mpsc::TryRecvError::Empty) => {
                if self.has_awaitable() && jobs_pending(ctx) {
                    return true;
                }
                self.ask_loop();
                self.wait(ctx)
            }
        }
    }
    fn has_awaitable(&self) -> bool {
        self.driver.signals.pump.load(Ordering::SeqCst)
            || self
                .pending
                .borrow()
                .values()
                .any(|p| p.settle_depth >= self.js_depth.get())
    }
    fn advance(&self, ctx: &Ctx<'js>) {
        self.settle(ctx);
        if !self.has_awaitable() {
            return;
        }
        if !jobs_pending(ctx) {
            self.turn_deadline.set(None);
            return;
        }
        if self.turn_deadline.get().is_none() && self.driver.config.timeout_ms > 0 {
            self.turn_deadline.set(Some(
                Instant::now() + Duration::from_millis(self.driver.config.timeout_ms),
            ));
            self.update_deadline();
        }
        let (id, context) = self
            .pending
            .borrow()
            .values()
            .find(|p| p.settle_depth >= self.js_depth.get())
            .map(|p| (Some(p.id), p.context.clone()))
            .unwrap_or_default();
        if let Err(error) = self.job(ctx, &context) {
            if let Some(id) = id {
                self.finish(id, Err(error));
                self.update_deadline();
            } else {
                self.driver.signals.pump.store(false, Ordering::SeqCst);
                self.driver.emit(Event::AsyncError(error));
            }
        }
        if !jobs_pending(ctx) {
            self.turn_deadline.set(None);
        }
        self.settle(ctx);
    }
    fn ask_loop(&self) {
        for pending in self
            .pending
            .borrow()
            .values()
            .filter(|p| p.settle_depth >= self.js_depth.get())
            .filter(|p| !p.asked.replace(true))
        {
            self.driver.emit(Event::NeedsLoop { id: pending.id });
        }
    }
    fn host(
        &self,
        ctx: &Ctx<'js>,
        target: HostTarget,
        args: Value<'js>,
    ) -> rquickjs::Result<Value<'js>> {
        let args = js_to_middle(ctx, args)?;
        let Data::Array(args) = args else {
            return Err(Exception::throw_type(
                ctx,
                "host arguments must be an array",
            ));
        };
        let token = self.driver.token.get() + 1;
        self.driver.token.set(token);
        self.replies.borrow_mut().insert(token, None);
        self.driver.emit(Event::Host {
            owner: self.active_request.get(),
            token,
            target,
            args,
        });
        let result = (|| loop {
            if self.driver.signals.stop.load(Ordering::SeqCst) {
                return Err(Exception::throw_internal(ctx, "QuickJS worker closed"));
            }
            if self.deadline.get().is_some_and(|d| Instant::now() >= d) {
                self.timed_out.set(true);
                return Err(Exception::throw_internal(
                    ctx,
                    "JavaScript execution timed out",
                ));
            }
            let reply = self
                .replies
                .borrow_mut()
                .get_mut(&token)
                .and_then(Option::take);
            if let Some(reply) = reply {
                return match reply {
                    Ok(value) => middle_into_js(ctx, value),
                    Err(error) => Err(error::throw_host_error(ctx, &error)),
                };
            }
            if !self.step(ctx, Some(token)) {
                return Err(Exception::throw_internal(ctx, "QuickJS worker closed"));
            }
        })();
        self.replies.borrow_mut().remove(&token);
        result
    }
}
fn install<'js>(ctx: &Ctx<'js>, session: Rc<Session<'js>>) -> rquickjs::Result<()> {
    let host_session = session.clone();
    let host = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, name: String, args: Value<'js>| {
            host_session.host(&ctx, HostTarget::Named(name), args)
        },
    )?;
    ctx.globals().set("__host", host)?;
    let fn_session = session.clone();
    let invoke = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, id: f64, args: Value<'js>| {
            fn_session.host(&ctx, HostTarget::Function(id as u64), args)
        },
    )?;
    ctx.globals().set("__php_invoke", invoke)?;
    let message_driver = session.driver.clone();
    let post = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, value: Value<'js>| -> rquickjs::Result<()> {
            let queue = &message_driver.messages;
            let capacity = queue.lock().unwrap_or_else(|e| e.into_inner()).remaining();
            let (data, bytes) = js_to_data(&ctx, value, capacity)?;
            let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
            let empty = queue.is_empty();
            queue
                .push(data, bytes)
                .map_err(|e| Exception::throw_type(&ctx, e))?;
            drop(queue);
            if empty && message_driver.signals.pump.load(Ordering::SeqCst) {
                message_driver.emit(Event::Messages);
            }
            Ok(())
        },
    )?;
    let quickjs = rquickjs::Object::new(ctx.clone())?;
    let failure_session = session.clone();
    quickjs.set(
        "reportError",
        Function::new(ctx.clone(), move |ctx: Ctx<'js>, value: Value<'js>| {
            if !failure_session.reported_failure.replace(true) {
                let thrown = ctx.throw(value);
                let parts = error::js_error_parts(&ctx, thrown);
                failure_session.driver.emit(Event::Fatal(WireError::Guest {
                    parts,
                    module: None,
                    map: None,
                    eval: false,
                }));
            }
        })?,
    )?;
    let clock_origin = Instant::now();
    quickjs.set(
        "now",
        Function::new(ctx.clone(), move || {
            clock_origin.elapsed().as_secs_f64() * 1000.0
        })?,
    )?;
    quickjs.set("postMessage", post)?;
    ctx.globals().set("quickjs", quickjs)?;
    ctx.eval::<(),_>("Object.freeze(globalThis.quickjs); Object.defineProperty(globalThis,'quickjs',{value:quickjs,writable:false,configurable:false})")?;
    ctx.eval::<(), _>(RUNTIME_JS)?;
    *session.functions.borrow_mut() = Some(ctx.globals().get("__jsFunctions")?);
    Ok(())
}
