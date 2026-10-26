//! Runtime ownership and RPC. This module never calls Zend or retains PHP values.
use crate::{
    bridge::{build_facade, MessageQueue, RUNTIME_JS},
    error::{self, HostError, JsErrorParts},
    marshal::{js_to_data, js_to_middle, middle_to_js, MiddleValue as Data},
    transpile::{TranspileCache, Transpiled},
};
use ext_php_rs::prelude::PhpException;
use rquickjs::{Context, Ctx, Exception, Function, Promise, Runtime, Value};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    io::Write,
    os::unix::net::UnixStream,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub struct Config {
    pub memory_limit: usize,
    pub timeout_ms: u64,
    pub max_stack: usize,
    pub isolated: bool,
    pub max_queued_message_bytes: usize,
    pub cache_entries: usize,
    pub cache_bytes: usize,
}
pub enum Operation {
    Eval { code: String, typescript: bool },
    Callback { id: u64, args: Vec<Data> },
    Jobs(i64),
    HasJobs,
    Roundtrip(Data),
}
pub struct Request {
    pub id: u64,
    pub op: Operation,
    pub names: Vec<String>,
    pub deletions: Vec<u64>,
    pub parent: Option<u64>,
}
pub enum HostTarget {
    Named(String),
    Function(u64),
}
pub enum Command {
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
    Done {
        id: u64,
        result: Result<Data, WireError>,
    },
    Host {
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
            Self::Internal(s) => PhpException::default(s),
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
    pub wake: UnixStream,
    worker: RefCell<Option<JoinHandle<()>>>,
    stop: Arc<AtomicBool>,
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
        let (wake, notify) = UnixStream::pair().map_err(|e| WireError::Internal(e.to_string()))?;
        wake.set_nonblocking(true)
            .map_err(|e| WireError::Internal(e.to_string()))?;
        notify
            .set_nonblocking(true)
            .map_err(|e| WireError::Internal(e.to_string()))?;
        let (tx, commands) = mpsc::sync_channel(256);
        let (events, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let messages = Arc::new(Mutex::new(MessageQueue::new(
            config.max_queued_message_bytes,
        )));
        let closed = stop.clone();
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
                    commands: RefCell::new(commands),
                    events,
                    notify,
                    stop: closed.clone(),
                    messages: queue,
                    cache: TranspileCache::new(config.cache_entries, config.cache_bytes),
                    config,
                    token: Cell::new(0),
                });
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.run(ready_tx)));
                closed.store(true, Ordering::SeqCst);
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
            stop.store(true, Ordering::SeqCst);
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
                stop,
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
        if self.stop.load(Ordering::SeqCst) {
            return Err(WireError::Internal("QuickJS worker is closed".into()));
        }
        Ok(())
    }
    pub fn send(&self, command: Command) -> Result<(), WireError> {
        self.check()?;
        self.tx
            .try_send(command)
            .map_err(|_| WireError::Internal("QuickJS command queue is full or closed".into()))
    }
    pub fn receive(&self) -> Result<Event, mpsc::TryRecvError> {
        self.rx.try_recv()
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
        self.stop.store(true, Ordering::SeqCst);
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
    commands: RefCell<mpsc::Receiver<Command>>,
    events: mpsc::Sender<Event>,
    notify: UnixStream,
    stop: Arc<AtomicBool>,
    messages: Arc<Mutex<MessageQueue>>,
    cache: TranspileCache,
    config: Config,
    token: Cell<u64>,
}
impl Driver {
    fn notify(&self) {
        let _ = (&self.notify).write(&[1]);
    }
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
        self.notify();
    }
    fn run(self: &Rc<Self>, ready: mpsc::SyncSender<Result<(), WireError>>) -> Result<(), String> {
        if self.config.isolated {
            let _ = ready.send(Ok(()));
            loop {
                let command = self.commands.borrow().recv().map_err(|e| e.to_string())?;
                if self.stop.load(Ordering::SeqCst) {
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
        crate::sandbox::apply_limits(&rt, self.config.memory_limit, self.config.max_stack);
        let deadline = Rc::new(Cell::new(None::<Instant>));
        let timed_out = Rc::new(Cell::new(false));
        let interrupted = self.stop.clone();
        let dl = deadline.clone();
        let expired = timed_out.clone();
        rt.set_interrupt_handler(Some(Box::new(move || {
            if interrupted.load(Ordering::SeqCst) {
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
            let session = Rc::new(Session {
                driver: self.clone(),
                pending: RefCell::new(Vec::new()),
                replies: RefCell::new(HashMap::new()),
                host_waits: RefCell::new(HashSet::new()),
                entries: RefCell::new(Vec::new()),
                deadline,
                timed_out,
                failed: RefCell::new(VecDeque::new()),
                root,
                root_finished: Cell::new(false),
                facade: RefCell::new(None),
                js_depth: Cell::new(0),
            });
            let _cleanup = SessionCleanup(session.clone());
            install(&ctx, session.clone()).map_err(|e| error::js_error_message(&ctx, e))?;
            if let Some(ready) = ready {
                let _ = ready.send(Ok(()));
            }
            if let Some(first) = first {
                session.request(&ctx, first);
            }
            while !self.stop.load(Ordering::SeqCst) && !session.root_finished.get() {
                session.advance(&ctx);
                if session.root_finished.get() {
                    break;
                }
                let command = if session.pending.borrow().is_empty() {
                    self.commands.borrow().recv().ok()
                } else if jobs_pending(&ctx) {
                    self.commands.borrow().try_recv().ok()
                } else {
                    session.ask_loop();
                    match self
                        .commands
                        .borrow()
                        .recv_timeout(Duration::from_millis(5))
                    {
                        Ok(c) => Some(c),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(_) => break,
                    }
                };
                if let Some(command) = command {
                    if !session.command(&ctx, command) {
                        break;
                    }
                }
            }
            let pending = std::mem::take(&mut *session.pending.borrow_mut());
            for item in pending {
                self.emit(Event::Done {
                    id: item.id,
                    result: Err(WireError::Internal(
                        "JS callback canceled: owning isolated eval ended".into(),
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
    asked: bool,
    settle_depth: usize,
}
struct Session<'js> {
    driver: Rc<Driver>,
    pending: RefCell<Vec<Pending<'js>>>,
    replies: RefCell<HashMap<u64, Result<Data, HostError>>>,
    host_waits: RefCell<HashSet<u64>>,
    entries: RefCell<Vec<Option<Instant>>>,
    deadline: Rc<Cell<Option<Instant>>>,
    timed_out: Rc<Cell<bool>>,
    failed: RefCell<VecDeque<u64>>,
    root: Option<u64>,
    root_finished: Cell<bool>,
    facade: RefCell<Option<(Vec<String>, rquickjs::Object<'js>)>>,
    js_depth: Cell<usize>,
}
// Native closures retain Session. Break its JS roots before the context is freed,
// including panic unwinding; otherwise a cached facade could retain the runtime.
struct SessionCleanup<'js>(Rc<Session<'js>>);
impl Drop for SessionCleanup<'_> {
    fn drop(&mut self) {
        self.0.pending.borrow_mut().clear();
        self.0.facade.borrow_mut().take();
    }
}
fn jobs_pending(ctx: &Ctx<'_>) -> bool {
    unsafe { rquickjs::qjs::JS_IsJobPending(rquickjs::qjs::JS_GetRuntime(ctx.as_raw().as_ptr())) }
}
impl<'js> Session<'js> {
    fn update_deadline(&self) {
        let entries = self.entries.borrow();
        // An independent pending Promise must not shorten a new synchronous entry.
        // Awaited jobs inherit their own active budget through the entry stack.
        self.deadline.set(if entries.is_empty() {
            self.pending
                .borrow()
                .iter()
                .filter_map(|p| p.deadline)
                .min()
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
        if result.is_err() {
            let mut failed = self.failed.borrow_mut();
            failed.push_back(id);
            if failed.len() > 256 {
                failed.pop_front();
            }
            drop(failed);
            let mut canceled = Vec::new();
            self.pending.borrow_mut().retain(|p| {
                if p.parent == Some(id) {
                    canceled.push(p.id);
                    false
                } else {
                    true
                }
            });
            for id in canceled {
                self.driver.emit(Event::Done {
                    id,
                    result: Err(WireError::Internal(
                        "JS callback canceled: owning call ended".into(),
                    )),
                });
            }
        }
        self.driver.emit(Event::Done { id, result });
        if self.root == Some(id) {
            self.root_finished.set(true)
        }
    }
    fn command(&self, ctx: &Ctx<'js>, command: Command) -> bool {
        match command {
            Command::Stop => return false,
            Command::Request(request) => self.request(ctx, request),
            Command::HostReply { token, result } => {
                if self.host_waits.borrow().contains(&token) {
                    self.replies.borrow_mut().insert(token, result);
                }
            }
            Command::Abort { id, message } => {
                self.pending.borrow_mut().retain(|p| p.id != id);
                self.finish(id, Err(WireError::Internal(message)));
                self.update_deadline();
            }
        }
        true
    }
    fn request(&self, ctx: &Ctx<'js>, request: Request) {
        let Request {
            id,
            op,
            names,
            deletions,
            parent,
        } = request;
        if parent.is_some_and(|p| self.failed.borrow().contains(&p)) {
            self.finish(
                id,
                Err(WireError::Internal(
                    "JS callback canceled: owning call ended".into(),
                )),
            );
            return;
        }
        let inherited = self
            .entries
            .borrow()
            .iter()
            .copied()
            .flatten()
            .chain(
                self.pending
                    .borrow()
                    .iter()
                    .filter(|p| Some(p.id) == parent)
                    .filter_map(|p| p.deadline),
            )
            .min();
        let deadline = inherited.or_else(|| {
            (self.driver.config.timeout_ms > 0)
                .then(|| Instant::now() + Duration::from_millis(self.driver.config.timeout_ms))
        });
        if deadline.is_some_and(|d| Instant::now() >= d) {
            self.finish(id, Err(WireError::Timeout));
            return;
        }
        if self.entries.borrow().len() >= crate::engine::MAX_DEPTH {
            self.finish(
                id,
                Err(WireError::Internal(
                    "maximum bridge re-entrancy depth exceeded".into(),
                )),
            );
            return;
        }
        self.entries.borrow_mut().push(deadline);
        self.update_deadline();
        self.timed_out.set(false);
        let await_result = matches!(&op, Operation::Eval { .. } | Operation::Callback { .. });
        let context = ErrorContext {
            eval: matches!(&op, Operation::Eval { .. }),
            module: None,
        };
        self.js_depth.set(self.js_depth.get() + 1);
        let result = self.start(ctx, op, &names, &deletions, context);
        match result {
            Ok((value, context)) => {
                if self.timed_out.get() || deadline.is_some_and(|d| Instant::now() >= d) {
                    self.timed_out.set(false);
                    self.finish(id, Err(WireError::Timeout));
                } else if !await_result {
                    self.finish(
                        id,
                        js_to_middle(ctx, value).map_err(|e| self.error(ctx, e, &context)),
                    );
                } else {
                    let promise_result = (|| {
                        if let Some(promise) = value.as_promise() {
                            return Ok(Some(promise.clone()));
                        }
                        let normalize: Function = ctx.globals().get("__asPromise")?;
                        normalize.call((value.clone(),))
                    })();
                    match promise_result {
                        Err(e) => self.finish(id, Err(self.error(ctx, e, &context))),
                        Ok(Some(promise)) => {
                            if let Some(result) = promise.result::<Value>() {
                                let result = result
                                    .and_then(|v| js_to_middle(ctx, v))
                                    .map_err(|e| self.error(ctx, e, &context));
                                self.finish(id, result)
                            } else {
                                self.pending.borrow_mut().push(Pending {
                                    id,
                                    parent,
                                    promise,
                                    context,
                                    deadline,
                                    asked: false,
                                    settle_depth: self.js_depth.get() - 1,
                                })
                            }
                        }
                        Ok(None) => self.finish(
                            id,
                            js_to_middle(ctx, value).map_err(|e| self.error(ctx, e, &context)),
                        ),
                    }
                }
            }
            Err(e) => self.finish(id, Err(e)),
        }
        self.js_depth.set(self.js_depth.get() - 1);
        self.entries.borrow_mut().pop();
        self.update_deadline();
    }
    fn start(
        &self,
        ctx: &Ctx<'js>,
        op: Operation,
        names: &[String],
        deletions: &[u64],
        mut context: ErrorContext,
    ) -> Result<(Value<'js>, ErrorContext), WireError> {
        let prep = (|| -> rquickjs::Result<()> {
            if !deletions.is_empty() {
                let delete: Function = ctx.globals().get("__deleteJsFn")?;
                for id in deletions {
                    delete.call::<_, ()>((*id as f64,))?;
                }
            }
            let cached = self
                .facade
                .borrow()
                .as_ref()
                .filter(|(current, _)| current == names)
                .map(|(_, object)| object.clone());
            if let Some(object) = cached {
                ctx.globals().set("php", object)?;
            } else {
                ctx.eval::<(), _>(build_facade(names))?;
                let object = ctx.globals().get("php")?;
                *self.facade.borrow_mut() = Some((names.to_vec(), object));
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
            Operation::Callback { id, args } => {
                let get: Function = ctx
                    .globals()
                    .get("__getJsFn")
                    .map_err(|e| self.error(ctx, e, &context))?;
                let function: Function = get
                    .call((id as f64,))
                    .map_err(|e| self.error(ctx, e, &context))?;
                let mut call_args = rquickjs::function::Args::new(ctx.clone(), args.len());
                for arg in args {
                    call_args
                        .push_arg(
                            middle_to_js(ctx, &arg).map_err(|e| self.error(ctx, e, &context))?,
                        )
                        .map_err(|e| self.error(ctx, e, &context))?;
                }
                function
                    .call_arg(call_args)
                    .map_err(|e| self.error(ctx, e, &context))?
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
            Operation::Roundtrip(value) => {
                middle_to_js(ctx, &value).map_err(|e| self.error(ctx, e, &context))?
            }
        };
        Ok((value, context))
    }
    fn job(&self, ctx: &Ctx<'js>, context: &ErrorContext) -> Result<bool, WireError> {
        let mut job_ctx = std::ptr::null_mut();
        self.entries.borrow_mut().push(self.deadline.get());
        self.update_deadline();
        self.js_depth.set(self.js_depth.get() + 1);
        let result = unsafe {
            rquickjs::qjs::JS_ExecutePendingJob(
                rquickjs::qjs::JS_GetRuntime(ctx.as_raw().as_ptr()),
                &mut job_ctx,
            )
        };
        self.js_depth.set(self.js_depth.get() - 1);
        self.entries.borrow_mut().pop();
        self.update_deadline();
        if result < 0 {
            return Err(self.error(ctx, rquickjs::Error::Exception, context));
        }
        if self.timed_out.replace(false) {
            return Err(WireError::Timeout);
        }
        Ok(result > 0)
    }
    fn settle(&self, ctx: &Ctx<'js>) {
        loop {
            let item = {
                let pending = self.pending.borrow();
                pending.iter().position(|p| {
                    p.settle_depth >= self.js_depth.get()
                        && (p.deadline.is_some_and(|d| Instant::now() >= d)
                            || p.promise.result::<Value>().is_some())
                })
            };
            let Some(index) = item else { break };
            let item = self.pending.borrow_mut().remove(index);
            let result = if item.deadline.is_some_and(|d| Instant::now() >= d) {
                Err(WireError::Timeout)
            } else {
                item.promise
                    .result::<Value>()
                    .unwrap()
                    .and_then(|v| js_to_middle(ctx, v))
                    .map_err(|e| self.error(ctx, e, &item.context))
            };
            self.finish(item.id, result);
            self.update_deadline();
        }
    }
    fn has_awaitable(&self) -> bool {
        self.pending
            .borrow()
            .iter()
            .any(|p| p.settle_depth >= self.js_depth.get())
    }
    fn advance(&self, ctx: &Ctx<'js>) {
        self.settle(ctx);
        let target = self
            .pending
            .borrow()
            .iter()
            .find(|p| p.settle_depth >= self.js_depth.get())
            .map(|p| (p.id, p.context.clone()));
        let Some((id, context)) = target else {
            return;
        };
        if let Err(e) = self.job(ctx, &context) {
            let index = self.pending.borrow().iter().position(|p| p.id == id);
            if let Some(index) = index {
                self.pending.borrow_mut().remove(index);
                self.finish(id, Err(e));
                self.update_deadline();
            }
        }
        self.settle(ctx);
    }
    fn ask_loop(&self) {
        let ids = {
            let mut pending = self.pending.borrow_mut();
            pending
                .iter_mut()
                .filter_map(|p| {
                    if !p.asked {
                        p.asked = true;
                        Some(p.id)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        for id in ids {
            self.driver.emit(Event::NeedsLoop { id })
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
        self.host_waits.borrow_mut().insert(token);
        self.driver.emit(Event::Host {
            token,
            target,
            args,
        });
        let result = (|| loop {
            if self.driver.stop.load(Ordering::SeqCst) {
                return Err(Exception::throw_internal(ctx, "QuickJS worker closed"));
            }
            if self.deadline.get().is_some_and(|d| Instant::now() >= d) {
                self.timed_out.set(true);
                return Err(Exception::throw_internal(
                    ctx,
                    "JavaScript execution timed out",
                ));
            }
            let reply = self.replies.borrow_mut().remove(&token);
            if let Some(reply) = reply {
                return match reply {
                    Ok(value) => middle_to_js(ctx, &value),
                    Err(error) => Err(error::throw_host_error(ctx, &error)),
                };
            }
            self.advance(ctx);
            let command = if jobs_pending(ctx) && self.has_awaitable() {
                self.driver.commands.borrow().try_recv().ok()
            } else {
                match self
                    .driver
                    .commands
                    .borrow()
                    .recv_timeout(Duration::from_millis(5))
                {
                    Ok(c) => Some(c),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(_) => return Err(Exception::throw_internal(ctx, "PHP receiver closed")),
                }
            };
            if let Some(command) = command {
                if !self.command(ctx, command) {
                    return Err(Exception::throw_internal(ctx, "QuickJS worker closed"));
                }
            }
        })();
        self.host_waits.borrow_mut().remove(&token);
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
    let queue = session.driver.messages.clone();
    let post = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, value: Value<'js>| -> rquickjs::Result<()> {
            let capacity = queue.lock().unwrap_or_else(|e| e.into_inner()).remaining();
            let (data, bytes) = js_to_data(&ctx, value, capacity)?;
            queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(data, bytes)
                .map_err(|e| Exception::throw_type(&ctx, e))
        },
    )?;
    let quickjs = rquickjs::Object::new(ctx.clone())?;
    quickjs.set("postMessage", post)?;
    ctx.globals().set("quickjs", quickjs)?;
    ctx.eval::<(),_>("Object.freeze(globalThis.quickjs); Object.defineProperty(globalThis,'quickjs',{value:quickjs,writable:false,configurable:false})")?;
    ctx.eval::<(), _>(RUNTIME_JS)?;
    Ok(())
}
