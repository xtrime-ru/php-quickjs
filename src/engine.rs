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
    zend::{ExecutorGlobals, Function as PhpFunction},
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
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
    host_calls: Cell<usize>,
    depth: Cell<usize>,
    results: RefCell<HashMap<u64, Result<MiddleValue, WireError>>>,
    waiters: RefCell<HashMap<u64, Rc<Waiter>>>,
    events: RefCell<VecDeque<Event>>,
    watcher: RefCell<Option<(Zval, Zval)>>,
    watcher_referenced: Cell<bool>,
    manifest_revision: Cell<Option<u64>>,
    message_waiter: Cell<Option<u64>>,
    message_error: RefCell<Option<WireError>>,
    loop_functions: RefCell<HashMap<&'static str, PhpFunction>>,
    weak: Weak<Engine>,
}
struct Waiter {
    suspension: Zval,
    sleeping: Cell<bool>,
}
unsafe extern "C" {
    fn php_quickjs_notification_stream(fd: i32, result: *mut Zval) -> bool;
}
impl Waiter {
    fn wake(&self) -> PhpResult<()> {
        if self.sleeping.replace(false) {
            checked(self.suspension.try_call_method("resume", vec![]))?;
        }
        Ok(())
    }
}
fn checked<T>(result: ext_php_rs::error::Result<T>) -> PhpResult<T> {
    let value = result?;
    // Leave Zend's exception in place, including uncatchable Fiber shutdown.
    // PhpException::throw preserves it when control returns to PHP.
    if ExecutorGlobals::has_exception() {
        return Err(PhpException::from_message("PHP callback failed".into()));
    }
    Ok(value)
}
pub(crate) fn call_php(class: &str, method: &str, args: Vec<&dyn IntoZvalDyn>) -> PhpResult<Zval> {
    checked(
        PhpFunction::try_from_method(class, method)
            .ok_or_else(|| {
                PhpException::from_message(format!(
                    "{class}::{method} unavailable; autoload revolt/event-loop"
                ))
            })?
            .try_call(args),
    )
}
fn function(name: &str, args: Vec<&dyn IntoZvalDyn>) -> PhpResult<Zval> {
    checked(
        PhpFunction::try_from_function(name)
            .ok_or_else(|| PhpException::from_message(format!("PHP function {name} unavailable")))?
            .try_call(args),
    )
}
impl Engine {
    pub fn new(config: Config) -> PhpResult<Rc<Self>> {
        let state = BridgeState::new();
        let isolated = config.isolated;
        let port = worker::Port::new(config).map_err(WireError::into_php)?;
        let engine = Rc::new_cyclic(|weak| Self {
            state: state.clone(),
            port,
            isolated,
            realm: Cell::new(0),
            sequence: Cell::new(0),
            active_eval: Cell::new(None),
            root_calls: Cell::new(0),
            host_calls: Cell::new(0),
            depth: Cell::new(0),
            results: RefCell::new(HashMap::new()),
            waiters: RefCell::new(HashMap::new()),
            events: RefCell::new(VecDeque::new()),
            watcher: RefCell::new(None),
            watcher_referenced: Cell::new(false),
            manifest_revision: Cell::new(None),
            message_waiter: Cell::new(None),
            message_error: RefCell::new(None),
            loop_functions: RefCell::new(HashMap::new()),
            weak: weak.clone(),
        });
        state.set_engine(Rc::downgrade(&engine));
        Ok(engine)
    }
    pub fn close(&self) -> PhpResult<()> {
        self.port.check_pid().map_err(WireError::into_php)?;
        self.port.close();
        let result = self.wake_all();
        // No user destructor runs until every sleeping caller has been resumed.
        self.remove_watcher();
        self.events.borrow_mut().clear();
        self.results.borrow_mut().clear();
        let _ = self.port.drain_messages();
        self.state.clear();
        result
    }
    fn loop_function(&self, method: &'static str) -> Option<PhpFunction> {
        if let Some(function) = self.loop_functions.borrow().get(method).copied() {
            return Some(function);
        }
        let function = PhpFunction::try_from_method("Revolt\\EventLoop", method)?;
        self.loop_functions.borrow_mut().insert(method, function);
        Some(function)
    }
    fn loop_call(&self, method: &'static str, args: Vec<&dyn IntoZvalDyn>) -> PhpResult<Zval> {
        checked(
            self.loop_function(method)
                .ok_or_else(|| PhpException::from_message("Autoload revolt/event-loop".into()))?
                .try_call(args),
        )
    }
    pub fn ensure_open(&self) -> PhpResult<()> {
        self.port.check().map_err(WireError::into_php)
    }
    pub fn ensure_post_open(&self) -> PhpResult<()> {
        self.port
            .check()
            .map_err(|error| self.error_after_stop(error))
    }
    pub fn isolated(&self) -> bool {
        self.isolated
    }
    pub fn realm_id(&self) -> u64 {
        self.realm.get()
    }
    pub fn is_active(&self) -> bool {
        self.active_eval.get().is_some() || self.root_calls.get() > 0 || self.host_calls.get() > 0
    }
    pub fn enter(&self) -> Result<CounterGuard<'_>, String> {
        let d = self.depth.get();
        if d >= MAX_DEPTH {
            return Err(format!(
                "maximum bridge re-entrancy depth ({MAX_DEPTH}) exceeded"
            ));
        }
        Ok(CounterGuard::enter(&self.depth))
    }
    pub fn eval(&self, code: String, typescript: bool) -> PhpResult<Zval> {
        if self.is_active() {
            return Err(PhpException::from_message(
                "Cannot eval while JavaScript is executing; use a JS callback".into(),
            ));
        }
        if self.isolated {
            self.realm.set(self.realm.get() + 1)
        }
        self.request(Operation::Eval { code, typescript })
    }
    pub fn callback(&self, id: u64, args: Vec<MiddleValue>) -> PhpResult<Zval> {
        self.request(Operation::Call {
            id,
            args,
            posted: false,
        })
    }
    pub fn post(&self, function: u64, args: Vec<MiddleValue>) -> PhpResult<()> {
        if self.isolated {
            return Err(PhpException::from_message(
                "post requires a non-isolated runtime".into(),
            ));
        }
        self.port
            .start_pump()
            .map_err(|error| self.error_after_stop(error))?;
        while !self.port.reserve_post() {
            if self.host_calls.get() > 0 {
                return Err(PhpException::from_message(
                    "Post admission is full during a synchronous PHP host callback".into(),
                ));
            }
            if let Err(error) = self.request(Operation::Capacity) {
                if let Err(stopped) = self.port.check() {
                    return Err(self.error_after_stop(stopped));
                }
                return Err(error);
            }
            self.port.check().map_err(WireError::into_php)?;
        }
        let id = self.next_id();
        let result = self.submit(
            id,
            Operation::Call {
                id: function,
                args,
                posted: true,
            },
            self.active_eval.get(),
        );
        if result.is_err() {
            self.port.release_post();
        }
        result
    }
    pub fn jobs(&self, max: i64) -> PhpResult<Zval> {
        if self.is_active() {
            return Err(PhpException::from_message(
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
    pub fn await_messages(&self) -> PhpResult<Vec<MiddleValue>> {
        if self.isolated || self.message_waiter.get().is_some() {
            return Err(PhpException::from_message(
                "awaitMessages requires one consumer on a non-isolated runtime".into(),
            ));
        }
        self.port
            .start_pump()
            .map_err(|error| self.error_after_stop(error))?;
        if let Some(error) = self.message_error.borrow_mut().take() {
            return Err(error.into_php());
        }
        let ready = self.drain_messages()?;
        if !ready.is_empty() {
            return Ok(ready);
        }
        let id = self.next_id();
        self.message_waiter.set(Some(id));
        let result = self.wait(
            id,
            || Ok(()),
            || {
                if let Some(error) = self.message_error.borrow_mut().take() {
                    return Err(error.into_php());
                }
                let messages = self.drain_messages()?;
                Ok((!messages.is_empty()).then_some(messages))
            },
        );
        self.message_waiter.set(None);
        result
    }
    fn next_id(&self) -> u64 {
        let id = self.sequence.get() + 1;
        self.sequence.set(id);
        id
    }
    fn wait<T>(
        &self,
        id: u64,
        start: impl FnOnce() -> PhpResult<()>,
        mut ready: impl FnMut() -> PhpResult<Option<T>>,
    ) -> PhpResult<T> {
        if self.waiters.borrow().len() >= 256 {
            return Err(PhpException::from_message(
                "Too many pending QuickJS operations (256)".into(),
            ));
        }
        let suspension = self
            .loop_function("getSuspension")
            .map(|function| checked(function.try_call(vec![])))
            .transpose()?;
        let waiter = suspension.map(|suspension| {
            Rc::new(Waiter {
                suspension,
                sleeping: Cell::new(false),
            })
        });
        if let Some(waiter) = &waiter {
            self.waiters.borrow_mut().insert(id, waiter.clone());
        }
        let result = (|| {
            start()?;
            loop {
                loop {
                    let event = self
                        .events
                        .borrow_mut()
                        .pop_front()
                        .or_else(|| self.port.receive().ok());
                    let Some(event) = event else { break };
                    self.event(event, waiter.is_some())?;
                }
                if let Some(value) = ready()? {
                    return Ok(value);
                }
                self.ensure_open()?;
                if let Some(waiter) = &waiter {
                    self.ensure_watcher()?;
                    waiter.sleeping.set(true);
                    let result = waiter.suspension.try_call_method("suspend", vec![]);
                    waiter.sleeping.set(false);
                    checked(result)?;
                } else {
                    self.event(
                        self.port.receive_blocking().map_err(WireError::into_php)?,
                        false,
                    )?;
                }
            }
        })();
        self.waiters.borrow_mut().remove(&id);
        if self.waiters.borrow().is_empty() {
            self.pause_watcher();
        }
        result
    }
    fn wake(&self, id: Option<u64>) -> PhpResult<()> {
        let waiter = id.and_then(|id| self.waiters.borrow().get(&id).cloned());
        waiter.map_or(Ok(()), |waiter| waiter.wake())
    }
    fn wake_all(&self) -> PhpResult<()> {
        let waiters = self.waiters.borrow().values().cloned().collect::<Vec<_>>();
        let mut error = None;
        for waiter in waiters {
            if let Err(e) = waiter.wake() {
                error.get_or_insert(e);
            }
        }
        error.map_or(Ok(()), Err)
    }
    fn send(&self, command: Command) -> PhpResult<()> {
        self.port.send(command).map_err(WireError::into_php)
    }
    fn error_after_stop(&self, fallback: WireError) -> PhpException {
        if self.port.check_pid().is_err() {
            return fallback.into_php();
        }
        while let Ok(event) = self.port.receive() {
            match event {
                Event::Host { .. } | Event::Stopped(_) => {}
                event => {
                    let _ = self.event(event, true);
                }
            }
        }
        self.message_error
            .borrow_mut()
            .take()
            .unwrap_or(fallback)
            .into_php()
    }
    fn submit(&self, id: u64, op: Operation, parent: Option<u64>) -> PhpResult<()> {
        let revision = self.state.manifest_revision();
        let names = (self.isolated || self.manifest_revision.get() != Some(revision)).then(|| {
            self.state
                .manifest_snapshot()
                .into_iter()
                .map(|entry| entry.name)
                .collect()
        });
        self.send(Command::Request(Request {
            id,
            op,
            names,
            deletions: self.state.take_pending_deletions(),
            parent,
        }))?;
        self.manifest_revision.set(Some(revision));
        Ok(())
    }
    fn request(&self, op: Operation) -> PhpResult<Zval> {
        self.ensure_open()?;
        let id = self.next_id();
        let eval = matches!(&op, Operation::Eval { .. });
        let parent = self.active_eval.get();
        let _root = (!eval && parent.is_none() && !matches!(&op, Operation::HasJobs))
            .then(|| CounterGuard::enter(&self.root_calls));
        if eval {
            self.active_eval.set(Some(id));
        }
        let result = self.wait(
            id,
            || self.submit(id, op, parent),
            || {
                let result = self.results.borrow_mut().remove(&id);
                result
                    .map(|result| {
                        let data = result.map_err(WireError::into_php)?;
                        middle_to_zval(&data, &self.state).map_err(PhpException::from_message)
                    })
                    .transpose()
            },
        );
        if eval {
            self.active_eval.set(None);
        }
        result
    }
    fn event(&self, event: Event, has_loop: bool) -> PhpResult<()> {
        match event {
            Event::Messages => self.wake(self.message_waiter.get())?,
            Event::AsyncError(error) | Event::Fatal(error) => {
                *self.message_error.borrow_mut() = Some(error);
                self.wake(self.message_waiter.get())?;
            }
            Event::Done { id, result } => {
                self.results.borrow_mut().insert(id, result);
                self.wake(Some(id))?;
            }
            Event::Host {
                token,
                target,
                args,
            } => {
                let _host = CounterGuard::enter(&self.host_calls);
                let result = match target {
                    worker::HostTarget::Named(name) => {
                        crate::bridge::host_call(&self.state, &name, args)
                    }
                    worker::HostTarget::Function(id) => {
                        crate::bridge::php_fn_call(&self.state, id, args)
                    }
                };
                if let Err(error) = checked(Ok(())) {
                    self.port.close();
                    return Err(error);
                }
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
            Event::Stopped(message) => return Err(PhpException::from_message(message)),
        }
        Ok(())
    }
    fn ensure_watcher(&self) -> PhpResult<()> {
        let watcher = self
            .watcher
            .borrow()
            .as_ref()
            .map(|(id, _)| id.shallow_clone());
        if let Some(watcher) = watcher {
            if !self.watcher_referenced.replace(true) {
                self.loop_call("reference", vec![&watcher])?;
            }
            return Ok(());
        }
        use std::os::fd::AsRawFd;
        let mut stream = Zval::new();
        // The C shim uses the PHP streams API and owns a duplicate descriptor.
        if !unsafe { php_quickjs_notification_stream(self.port.wake.as_raw_fd(), &mut stream) } {
            return Err(PhpException::from_message(
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
                        Ok(count) if count < buf.len() => break,
                        Ok(_) => {}
                    }
                }
                engine.port.acknowledge_notification();
                while let Ok(event) = engine.port.receive() {
                    match event {
                        Event::Done { .. }
                        | Event::Messages
                        | Event::AsyncError(_)
                        | Event::Fatal(_) => engine.event(event, true)?,
                        Event::NeedsLoop { .. } => {}
                        event => engine.events.borrow_mut().push_back(event),
                    }
                }
                if !engine.events.borrow().is_empty() {
                    let waiter = engine
                        .waiters
                        .borrow()
                        .values()
                        .find(|w| w.sleeping.get())
                        .cloned();
                    if let Some(waiter) = waiter {
                        waiter.wake()?;
                    }
                }
                if engine.port.check().is_err() {
                    engine.wake_all()?;
                }
            }
            Ok(())
        }) as Box<dyn Fn() -> PhpResult<()>>)
        .into_zval(false)?;
        let callback = call_php("Closure", "fromCallable", vec![&native])?;
        let watcher = self.loop_call("onReadable", vec![&stream, &callback])?;
        *self.watcher.borrow_mut() = Some((watcher, stream));
        self.watcher_referenced.set(true);
        Ok(())
    }
    fn pause_watcher(&self) {
        if self.watcher_referenced.replace(false) {
            let watcher = self
                .watcher
                .borrow()
                .as_ref()
                .map(|(id, _)| id.shallow_clone());
            if let Some(watcher) = watcher {
                let _ = self.loop_call("unreference", vec![&watcher]);
            }
        }
    }
    fn remove_watcher(&self) {
        self.watcher_referenced.set(false);
        let watcher = self.watcher.borrow_mut().take();
        if let Some((watcher, stream)) = watcher {
            let _ = self.loop_call("cancel", vec![&watcher]);
            let _ = function("fclose", vec![&stream]);
        }
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        // PHP forbids calling userland after executor shutdown. Its driver and
        // stream resources are already being destroyed at that point.
        if ExecutorGlobals::get().active {
            self.remove_watcher();
        }
        self.port.close();
    }
}
pub struct CounterGuard<'a>(&'a Cell<usize>);
impl<'a> CounterGuard<'a> {
    fn enter(depth: &'a Cell<usize>) -> Self {
        depth.set(depth.get() + 1);
        Self(depth)
    }
}
impl Drop for CounterGuard<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}
