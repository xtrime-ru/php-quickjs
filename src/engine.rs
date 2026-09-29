//! Owns the QuickJS runtime/context and the re-entrancy machinery.

use crate::bridge::BridgeState;
use crate::marshal::MiddleValue;
use crate::sandbox;
use crate::transpile::TranspileCache;
use ext_php_rs::{
    closure::Closure,
    convert::{IntoZval, IntoZvalDyn},
    prelude::*,
    types::Zval,
    zend::Function as PhpFunction,
};
use rquickjs::{Context, Ctx, Function, Promise, Runtime, Value};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Maximum nesting depth across the JS<->PHP boundary, guarding runaway mutual
/// recursion (which would otherwise overflow the native stack).
pub const MAX_DEPTH: usize = 200;

/// The QuickJS engine: runtime + context + the shared bridge state.
pub struct Engine {
    pub rt: Runtime,
    pub state: Rc<BridgeState>,
    /// Content-addressed TS->JS transpile cache (source maps kept host-side).
    pub transpile: TranspileCache,
    /// The persistent realm in shared mode; `None` in isolated mode (a fresh
    /// realm is created per eval and discarded afterwards).
    shared_ctx: Option<Context>,
    depth: Cell<usize>,
    active_ctx: Cell<Option<NonNull<rquickjs::qjs::JSContext>>>,
    /// Identity of the PHP Fiber which owns `active_ctx` (zero is {main}).
    /// A suspended native stack may only be resumed by this same Fiber.
    active_fiber: Cell<Option<usize>>,
    /// Calls arriving from Revolt while the owner waits on a Promise are queued;
    /// the owner executes them after its Suspension resumes.
    queued_callbacks: RefCell<VecDeque<QueuedCallback>>,
    waiter: RefCell<Option<Rc<Waiter>>>,
    /// Per-entry wall-clock deadline; `None` when no eval is in flight.
    deadline: Rc<Cell<Option<Instant>>>,
    /// Set by the interrupt handler when it aborts on the deadline.
    timed_out: Rc<Cell<bool>>,
    /// Per-entry timeout; `None` disables the wall-clock guard.
    timeout: Option<Duration>,
}

struct QueuedCallback {
    id: u64,
    args: Vec<MiddleValue>,
    suspension: Zval,
    result: Rc<RefCell<Option<PhpResult<Zval>>>>,
}

struct Waiter {
    suspension: Zval,
    woken: Cell<bool>,
}

impl Waiter {
    fn wake(&self) -> PhpResult<()> {
        if !self.woken.replace(true) {
            self.suspension.try_call_method("resume", vec![])?;
        }
        Ok(())
    }
}

fn call_php(class: &str, method: &str, args: Vec<&dyn IntoZvalDyn>) -> PhpResult<Zval> {
    PhpFunction::try_from_method(class, method)
        .ok_or_else(|| {
            PhpException::default(format!(
                "{class}::{method} is unavailable; autoload revolt/event-loop for external I/O"
            ))
        })?
        .try_call(args)
        .map_err(Into::into)
}

impl Engine {
    fn current_fiber_id() -> PhpResult<usize> {
        let fiber = call_php("Fiber", "getCurrent", vec![])?;
        Ok(fiber
            .object()
            .map_or(0, |object| object as *const _ as usize))
    }

    fn check_deadline(&self, ctx: &Ctx<'_>) -> PhpResult<()> {
        if self.timed_out() || self.deadline.get().is_some_and(|d| Instant::now() >= d) {
            self.timed_out.set(true);
            drop(ctx.catch());
            return Err(PhpException::from_class::<
                crate::exceptions::QuickJSTimeoutException,
            >("JavaScript execution timed out".to_owned()));
        }
        Ok(())
    }

    pub fn run_jobs(&self, ctx: &Ctx<'_>, max_jobs: i64) -> PhpResult<i64> {
        let mut count = 0;
        while count < max_jobs {
            self.check_deadline(ctx)?;
            let mut job_ctx = std::ptr::null_mut();
            // SAFETY: eval_in holds the runtime lock. The returned context is
            // borrowed from that runtime; Ctx::from_raw acquires its own ref.
            let result = unsafe {
                rquickjs::qjs::JS_ExecutePendingJob(
                    rquickjs::qjs::JS_GetRuntime(ctx.as_raw().as_ptr()),
                    &mut job_ctx,
                )
            };
            self.check_deadline(ctx)?;
            if result < 0 {
                let c = unsafe { Ctx::from_raw(NonNull::new(job_ctx).expect("job error context")) };
                return Err(crate::error::js_error_to_php(
                    &c,
                    rquickjs::Error::Exception,
                ));
            }
            if result == 0 {
                break;
            }
            count += 1;
        }
        Ok(count)
    }

    /// Await returned Promises, yielding to Revolt when host I/O is pending.
    pub fn await_value<'js>(
        &self,
        ctx: &Ctx<'js>,
        value: Value<'js>,
        map_js_error: impl Fn(rquickjs::Error) -> PhpException,
    ) -> PhpResult<Value<'js>> {
        let promise = match value.as_promise() {
            Some(promise) => Some(promise.clone()),
            None => {
                let normalize: Function =
                    ctx.globals().get("__asPromise").map_err(&map_js_error)?;
                normalize
                    .call::<_, Option<Promise>>((value.clone(),))
                    .map_err(&map_js_error)?
            }
        };
        let Some(promise) = promise else {
            return Ok(value);
        };

        loop {
            self.check_deadline(ctx)?;
            if let Some(result) = promise.result() {
                return result.map_err(map_js_error);
            }
            if self.run_jobs(ctx, 1)? == 0 {
                self.suspend_on_revolt()?;
                self.check_deadline(ctx)?;
                self.drain_queued_callbacks(ctx)?;
            }
        }
    }

    fn suspend_on_revolt(&self) -> PhpResult<()> {
        let suspension = call_php("Revolt\\EventLoop", "getSuspension", vec![])?;
        let waiter = Rc::new(Waiter {
            suspension: suspension.shallow_clone(),
            woken: Cell::new(false),
        });
        let timer = self
            .deadline
            .get()
            .map(|deadline| -> PhpResult<Zval> {
                let waiter = waiter.clone();
                let callback = Closure::wrap(
                    Box::new(move || waiter.wake()) as Box<dyn Fn() -> PhpResult<()>>
                )
                .into_zval(false)?;
                let callback = call_php("Closure", "fromCallable", vec![&callback])?;
                let delay = deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs_f64();
                call_php("Revolt\\EventLoop", "delay", vec![&delay, &callback])
            })
            .transpose()?;
        *self.waiter.borrow_mut() = Some(waiter);
        let result = suspension.try_call_method("suspend", vec![]);
        self.waiter.borrow_mut().take();
        if let Some(timer) = timer {
            call_php("Revolt\\EventLoop", "cancel", vec![&timer])?;
        }
        result.map(|_| ()).map_err(Into::into)
    }

    fn drain_queued_callbacks(&self, ctx: &Ctx<'_>) -> PhpResult<()> {
        loop {
            self.check_deadline(ctx)?;
            let Some(queued) = self.queued_callbacks.borrow_mut().pop_front() else {
                return Ok(());
            };
            let result = crate::callback::invoke_callback(ctx, self, queued.id, &queued.args);
            *queued.result.borrow_mut() = Some(result);
            queued.suspension.try_call_method("resume", vec![])?;
        }
    }

    pub fn active_on_other_fiber(&self) -> PhpResult<bool> {
        let current = Self::current_fiber_id()?;
        Ok(self.is_active()
            && self
                .active_fiber
                .get()
                .is_some_and(|owner| owner != current))
    }

    pub fn queue_callback(&self, id: u64, args: Vec<MiddleValue>) -> PhpResult<Zval> {
        let waiter = self.waiter.borrow().clone().ok_or_else(|| {
            PhpException::default("QuickJS engine is active on another PHP Fiber".to_owned())
        })?;
        let suspension = call_php("Revolt\\EventLoop", "getSuspension", vec![])?;
        let result = Rc::new(RefCell::new(None));
        self.queued_callbacks
            .borrow_mut()
            .push_back(QueuedCallback {
                id,
                args,
                suspension: suspension.shallow_clone(),
                result: result.clone(),
            });
        waiter.wake()?;
        suspension.try_call_method("suspend", vec![])?;
        let result = result.borrow_mut().take().ok_or_else(|| {
            PhpException::default("JS callback resumed before completion".to_owned())
        })?;
        result
    }

    pub fn new(
        memory_limit: usize,
        timeout_ms: u64,
        max_stack: usize,
        isolated: bool,
    ) -> rquickjs::Result<Rc<Self>> {
        let rt = Runtime::new()?;
        sandbox::apply_limits(&rt, memory_limit, max_stack);
        let deadline = Rc::new(Cell::new(None));
        let timed_out = Rc::new(Cell::new(false));
        sandbox::install_interrupt(&rt, deadline.clone(), timed_out.clone());

        // Shared mode: one persistent realm. Isolated mode: a fresh realm per
        // eval (so each eval is its own world; cross-eval state is not kept).
        let shared_ctx = if isolated {
            None
        } else {
            Some(Context::full(&rt)?)
        };
        let state = BridgeState::new();
        let engine = Rc::new(Engine {
            rt,
            state: state.clone(),
            transpile: TranspileCache::new(256),
            shared_ctx,
            depth: Cell::new(0),
            active_ctx: Cell::new(None),
            active_fiber: Cell::new(None),
            queued_callbacks: RefCell::new(VecDeque::new()),
            waiter: RefCell::new(None),
            deadline,
            timed_out,
            timeout: (timeout_ms > 0).then(|| Duration::from_millis(timeout_ms)),
        });
        // Close the cycle so the bridge can reach back into the engine when it
        // needs to invoke JS callbacks held by PHP.
        state.set_engine(Rc::downgrade(&engine));
        Ok(engine)
    }

    /// Arm the wall-clock deadline for an eval and clear the timeout flag.
    pub fn arm_deadline(&self) {
        self.timed_out.set(false);
        self.deadline.set(self.timeout.map(|t| Instant::now() + t));
    }

    /// Disarm the deadline once an eval completes.
    pub fn disarm_deadline(&self) {
        self.deadline.set(None);
    }

    /// Whether the last eval was aborted by the wall-clock guard.
    pub fn timed_out(&self) -> bool {
        self.timed_out.get()
    }

    /// The persistent realm, if this engine has one (shared mode).
    pub fn shared_ctx(&self) -> Option<&Context> {
        self.shared_ctx.as_ref()
    }

    pub fn is_active(&self) -> bool {
        self.active_ctx.get().is_some()
    }

    /// Run on the current PHP stack. Reentrant callbacks reuse this engine's
    /// context; a callback belonging to a different engine gets its own context.
    pub fn eval_in<R>(&self, f: impl FnOnce(&Ctx<'_>) -> PhpResult<R>) -> PhpResult<R> {
        let current_fiber = Self::current_fiber_id()?;
        if let Some(ptr) = self.active_ctx.get() {
            if self.active_fiber.get() != Some(current_fiber) {
                return Err(PhpException::default(
                    "QuickJS engine is active on another PHP Fiber".to_owned(),
                ));
            }
            // SAFETY: the outer Context::with owns this context and its lock.
            // A suspended native stack may only be resumed by its owning Fiber.
            let ctx = unsafe { Ctx::from_raw(ptr) };
            self.check_deadline(&ctx)?;
            let result = f(&ctx);
            self.check_deadline(&ctx)?;
            return result;
        }
        let run = |ctx: &Context| {
            ctx.with(|c| {
                // SAFETY: the runtime is locked; refresh its stack limit for
                // this PHP Fiber at the outer entry, never during recursion.
                unsafe {
                    rquickjs::qjs::JS_UpdateStackTop(ctx.get_runtime_ptr());
                }
                self.active_ctx.set(Some(c.as_raw()));
                self.active_fiber.set(Some(current_fiber));
                self.arm_deadline();
                let _guard = ExecutionGuard { engine: self };
                crate::bridge::flush_pending_deletions(&c, &self.state)
                    .map_err(|e| self.callback_error(&c, e))?;
                self.check_deadline(&c)?;
                let result = f(&c);
                self.check_deadline(&c)?;
                result
            })
        };
        match &self.shared_ctx {
            Some(ctx) => run(ctx),
            None => {
                let ctx =
                    Context::full(&self.rt).map_err(|e| PhpException::default(e.to_string()))?;
                run(&ctx)
            }
        }
    }

    pub fn callback_error(
        &self,
        ctx: &Ctx<'_>,
        err: rquickjs::Error,
    ) -> ext_php_rs::exception::PhpException {
        if self.timed_out() {
            // Consume the interrupted JS exception before the next entry.
            drop(ctx.catch());
            return ext_php_rs::exception::PhpException::from_class::<
                crate::exceptions::QuickJSTimeoutException,
            >("JavaScript callback execution timed out".to_owned());
        }
        crate::error::js_error_to_php(ctx, err)
    }

    /// Enter one level of cross-boundary nesting; errors if the cap is hit.
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
}

/// RAII guard that decrements the depth counter on drop.
pub struct DepthGuard<'a> {
    depth: &'a Cell<usize>,
}

impl Drop for DepthGuard<'_> {
    fn drop(&mut self) {
        self.depth.set(self.depth.get() - 1);
    }
}

struct ExecutionGuard<'a> {
    engine: &'a Engine,
}

impl Drop for ExecutionGuard<'_> {
    fn drop(&mut self) {
        self.engine.active_ctx.set(None);
        self.engine.active_fiber.set(None);
        self.engine.disarm_deadline();
        let pending = std::mem::take(&mut *self.engine.queued_callbacks.borrow_mut());
        for queued in pending {
            *queued.result.borrow_mut() = Some(Err(PhpException::default(
                "JavaScript callback canceled: owning call ended".to_owned(),
            )));
            let _ = queued.suspension.try_call_method("resume", vec![]);
        }
    }
}
