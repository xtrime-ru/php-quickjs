//! PHP-thread facade. Only owned wire values cross into the JavaScript thread.
use crate::{
    bridge::BridgeState,
    marshal::{middle_to_zval, MiddleValue},
    worker::{self, Command, Config, Event, Operation, Request, WireError},
};
use ext_php_rs::{
    closure::Closure,
    convert::{IntoZval, IntoZvalDyn},
    prelude::*,
    types::Zval,
    zend::Function as PhpFunction,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::Read,
    rc::{Rc, Weak},
};
pub const MAX_DEPTH: usize = 200;

pub struct Engine {
    pub state: Rc<BridgeState>,
    port: worker::Port,
    isolated: bool,
    realm: Cell<u64>,
    sequence: Cell<u64>,
    active_eval: Cell<Option<u64>>,
    root_calls: Cell<usize>,
    depth: Cell<usize>,
    results: RefCell<HashMap<u64, Result<MiddleValue, WireError>>>,
    waiters: RefCell<HashMap<u64, Rc<Waiter>>>,
    watcher: RefCell<Option<(Zval, Zval)>>,
    weak: Weak<Engine>,
}
struct Waiter {
    suspension: Zval,
    sleeping: Cell<bool>,
}
impl Waiter {
    fn wake(&self) -> PhpResult<()> {
        if self.sleeping.replace(false) {
            self.suspension.try_call_method("resume", vec![])?;
        }
        Ok(())
    }
}
pub(crate) fn call_php(class: &str, method: &str, args: Vec<&dyn IntoZvalDyn>) -> PhpResult<Zval> {
    PhpFunction::try_from_method(class, method)
        .ok_or_else(|| {
            PhpException::default(format!(
                "{class}::{method} unavailable; autoload revolt/event-loop"
            ))
        })?
        .try_call(args)
        .map_err(Into::into)
}
fn function(name: &str, args: Vec<&dyn IntoZvalDyn>) -> PhpResult<Zval> {
    PhpFunction::try_from_function(name)
        .ok_or_else(|| PhpException::default(format!("PHP function {name} unavailable")))?
        .try_call(args)
        .map_err(Into::into)
}
impl Engine {
    pub fn new(
        memory_limit: usize,
        timeout_ms: u64,
        max_stack: usize,
        isolated: bool,
        max_queued_message_bytes: usize,
        cache_entries: usize,
        cache_bytes: usize,
    ) -> PhpResult<Rc<Self>> {
        let state = BridgeState::new();
        let port = worker::Port::new(Config {
            memory_limit,
            timeout_ms,
            max_stack,
            isolated,
            max_queued_message_bytes,
            cache_entries,
            cache_bytes,
        })
        .map_err(WireError::into_php)?;
        let engine = Rc::new_cyclic(|weak| Self {
            state: state.clone(),
            port,
            isolated,
            realm: Cell::new(0),
            sequence: Cell::new(0),
            active_eval: Cell::new(None),
            root_calls: Cell::new(0),
            depth: Cell::new(0),
            results: RefCell::new(HashMap::new()),
            waiters: RefCell::new(HashMap::new()),
            watcher: RefCell::new(None),
            weak: weak.clone(),
        });
        state.set_engine(Rc::downgrade(&engine));
        Ok(engine)
    }
    pub fn close(&self) -> PhpResult<()> {
        self.port.check_pid().map_err(WireError::into_php)?;
        self.port.close();
        self.remove_watcher();
        self.state.clear();
        let _ = self.port.drain_messages();
        let waiters = self.waiters.borrow().values().cloned().collect::<Vec<_>>();
        for waiter in waiters {
            waiter.wake()?;
        }
        Ok(())
    }
    pub fn ensure_open(&self) -> PhpResult<()> {
        self.port.check().map_err(WireError::into_php)
    }
    pub fn isolated(&self) -> bool {
        self.isolated
    }
    pub fn realm_id(&self) -> u64 {
        self.realm.get()
    }
    pub fn is_active(&self) -> bool {
        self.active_eval.get().is_some() || self.root_calls.get() > 0
    }
    pub fn enter(&self) -> Result<DepthGuard<'_>, String> {
        let d = self.depth.get();
        if d >= MAX_DEPTH {
            return Err(format!(
                "maximum bridge re-entrancy depth ({MAX_DEPTH}) exceeded"
            ));
        }
        self.depth.set(d + 1);
        Ok(DepthGuard { depth: &self.depth })
    }
    pub fn eval(&self, code: String, typescript: bool) -> PhpResult<Zval> {
        if self.is_active() {
            return Err(PhpException::default(
                "Cannot eval while JavaScript is executing; use a JS callback".into(),
            ));
        }
        if self.isolated {
            self.realm.set(self.realm.get() + 1)
        }
        self.request(Operation::Eval { code, typescript })
    }
    pub fn callback(&self, id: u64, args: Vec<MiddleValue>) -> PhpResult<Zval> {
        self.request(Operation::Callback { id, args })
    }
    pub fn jobs(&self, max: i64) -> PhpResult<Zval> {
        if self.is_active() {
            return Err(PhpException::default(
                "Cannot run jobs while JavaScript is executing".into(),
            ));
        }
        self.request(Operation::Jobs(max))
    }
    pub fn has_jobs(&self) -> PhpResult<Zval> {
        self.request(Operation::HasJobs)
    }
    pub fn roundtrip(&self, value: MiddleValue) -> PhpResult<Zval> {
        self.request(Operation::Roundtrip(value))
    }
    pub fn drain_messages(&self) -> PhpResult<Vec<MiddleValue>> {
        self.port.drain_messages().map_err(WireError::into_php)
    }
    fn send(&self, command: Command) -> PhpResult<()> {
        self.port.send(command).map_err(WireError::into_php)
    }
    fn request(&self, op: Operation) -> PhpResult<Zval> {
        self.port.check().map_err(WireError::into_php)?;
        if self.waiters.borrow().len() >= 256 {
            return Err(PhpException::default(
                "Too many pending QuickJS operations (256)".into(),
            ));
        }
        let id = self.sequence.get() + 1;
        self.sequence.set(id);
        let eval = matches!(&op, Operation::Eval { .. });
        let parent = self.active_eval.get();
        let root_call = !eval && parent.is_none() && !matches!(&op, Operation::HasJobs);
        if root_call {
            self.root_calls.set(self.root_calls.get() + 1)
        }
        if eval {
            self.active_eval.set(Some(id))
        }
        let result = (|| {
            let suspension = PhpFunction::try_from_method("Revolt\\EventLoop", "getSuspension")
                .map(|f| f.try_call(vec![]).map_err(PhpException::from))
                .transpose()?;
            let waiter = suspension.map(|s| {
                Rc::new(Waiter {
                    suspension: s,
                    sleeping: Cell::new(false),
                })
            });
            if let Some(w) = &waiter {
                self.waiters.borrow_mut().insert(id, w.clone());
                self.ensure_watcher()?;
            }
            self.send(Command::Request(Request {
                id,
                op,
                names: self
                    .state
                    .manifest_snapshot()
                    .into_iter()
                    .map(|e| e.name)
                    .collect(),
                deletions: self.state.take_pending_deletions(),
                parent,
            }))?;
            loop {
                while let Ok(event) = self.port.receive() {
                    self.event(event, waiter.is_some())?;
                }
                let result = self.results.borrow_mut().remove(&id);
                if let Some(result) = result {
                    let data = result.map_err(WireError::into_php)?;
                    return middle_to_zval(&data, &self.state).map_err(PhpException::default);
                }
                self.port.check().map_err(WireError::into_php)?;
                if let Some(w) = &waiter {
                    w.sleeping.set(true);
                    let result = w.suspension.try_call_method("suspend", vec![]);
                    w.sleeping.set(false);
                    result?;
                } else {
                    let event = self.port.receive_blocking().map_err(WireError::into_php)?;
                    self.event(event, false)?;
                }
            }
        })();
        self.waiters.borrow_mut().remove(&id);
        if self.waiters.borrow().is_empty() {
            self.remove_watcher()
        }
        if eval {
            self.active_eval.set(None)
        }
        if root_call {
            self.root_calls.set(self.root_calls.get() - 1)
        }
        result
    }
    fn event(&self, event: Event, has_loop: bool) -> PhpResult<()> {
        match event {
            Event::Done { id, result } => {
                self.results.borrow_mut().insert(id, result);
                let waiter = self.waiters.borrow().get(&id).cloned();
                if let Some(w) = waiter {
                    w.wake()?
                }
            }
            Event::Host {
                token,
                target,
                args,
            } => {
                let result = match target {
                    worker::HostTarget::Named(name) => {
                        crate::bridge::host_call(&self.state, &name, args)
                    }
                    worker::HostTarget::Function(id) => {
                        crate::bridge::php_fn_call(&self.state, id, args)
                    }
                };
                self.send(Command::HostReply { token, result })?;
            }
            Event::NeedsLoop { id } => {
                if !has_loop {
                    self.send(Command::Abort {
                        id,
                        message: "Autoload revolt/event-loop to await external I/O".into(),
                    })?;
                }
            }
            Event::Stopped(message) => return Err(PhpException::default(message)),
        }
        Ok(())
    }
    fn ensure_watcher(&self) -> PhpResult<()> {
        if self.watcher.borrow().is_some() {
            return Ok(());
        }
        use std::os::fd::AsRawFd;
        let filename = format!("php://fd/{}", self.port.wake.as_raw_fd());
        let stream = function("fopen", vec![&filename, &"r"])?;
        if stream.is_false() {
            return Err(PhpException::default(
                "Cannot open QuickJS notification descriptor".into(),
            ));
        }
        function("stream_set_blocking", vec![&stream, &false])?;
        let weak = self.weak.clone();
        let native = Closure::wrap(Box::new(move || -> PhpResult<()> {
            if let Some(engine) = weak.upgrade() {
                let mut buf = [0u8; 512];
                loop {
                    match (&engine.port.wake).read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                let waiters = engine
                    .waiters
                    .borrow()
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                for w in waiters {
                    w.wake()?
                }
            }
            Ok(())
        }) as Box<dyn Fn() -> PhpResult<()>>)
        .into_zval(false)?;
        let callback = call_php("Closure", "fromCallable", vec![&native])?;
        let watcher = call_php("Revolt\\EventLoop", "onReadable", vec![&stream, &callback])?;
        *self.watcher.borrow_mut() = Some((watcher, stream));
        Ok(())
    }
    fn remove_watcher(&self) {
        let watcher = self.watcher.borrow_mut().take();
        if let Some((watcher, stream)) = watcher {
            let _ = call_php("Revolt\\EventLoop", "cancel", vec![&watcher]);
            let _ = function("fclose", vec![&stream]);
        }
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.remove_watcher();
        self.port.close();
    }
}
pub struct DepthGuard<'a> {
    depth: &'a Cell<usize>,
}
impl Drop for DepthGuard<'_> {
    fn drop(&mut self) {
        self.depth.set(self.depth.get() - 1)
    }
}
