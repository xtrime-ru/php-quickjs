//! Owns the QuickJS runtime/context and the re-entrancy machinery.

use crate::bridge::BridgeState;
use crate::sandbox;
use crate::transpile::TranspileCache;
use rquickjs::{Context, Ctx, Runtime};
use std::cell::Cell;
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
    /// Per-entry wall-clock deadline; `None` when no eval is in flight.
    deadline: Rc<Cell<Option<Instant>>>,
    /// Set by the interrupt handler when it aborts on the deadline.
    timed_out: Rc<Cell<bool>>,
    /// Per-entry timeout; `None` disables the wall-clock guard.
    timeout: Option<Duration>,
}

impl Engine {
    pub fn run_jobs(&self, ctx: &Ctx<'_>, max_jobs: i64) -> ext_php_rs::prelude::PhpResult<i64> {
        let mut count = 0;
        while count < max_jobs {
            let mut job_ctx = std::ptr::null_mut();
            // SAFETY: eval_in owns the runtime lock. QuickJS returns a
            // borrowed context pointer on failure, valid in this runtime.
            let result = unsafe {
                rquickjs::qjs::JS_ExecutePendingJob(
                    rquickjs::qjs::JS_GetRuntime(ctx.as_raw().as_ptr()),
                    &mut job_ctx,
                )
            };
            if self.timed_out()
                || self
                    .deadline
                    .get()
                    .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.timed_out.set(true);
                // Check wall time even if short jobs never reach QuickJS's
                // interrupt poll, including jobs that call slow PHP callbacks.
                // Promise reactions can turn an interrupt into a rejection;
                // still surface the execution budget to the host.
                if result < 0 {
                    let c = unsafe {
                        rquickjs::Ctx::from_raw(
                            std::ptr::NonNull::new(job_ctx).expect("job error context"),
                        )
                    };
                    drop(c.catch());
                }
                return Err(ext_php_rs::exception::PhpException::from_class::<
                    crate::exceptions::QuickJSTimeoutException,
                >(
                    "JavaScript job execution timed out".to_owned()
                ));
            }
            if result < 0 {
                let c = unsafe {
                    rquickjs::Ctx::from_raw(
                        std::ptr::NonNull::new(job_ctx).expect("job error context"),
                    )
                };
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
    pub fn eval_in<R>(&self, f: impl FnOnce(&Ctx<'_>) -> R) -> rquickjs::Result<R> {
        if let Some(ptr) = self.active_ctx.get() {
            // SAFETY: the outer Context::with owns this context and its lock.
            // Fiber switching is blocked until that call returns.
            let ctx = unsafe { Ctx::from_raw(ptr) };
            return Ok(f(&ctx));
        }
        let run = |ctx: &Context| {
            ctx.with(|c| {
                // SAFETY: c belongs to this locked runtime. PHP Fibers can enter
                // on a different native stack, so refresh QuickJS's stack limit
                // at the outer boundary only (never during JS recursion).
                unsafe {
                    rquickjs::qjs::JS_UpdateStackTop(ctx.get_runtime_ptr());
                    zend_fiber_switch_block();
                }
                self.active_ctx.set(Some(c.as_raw()));
                self.arm_deadline();
                let _guard = ExecutionGuard { engine: self };
                crate::bridge::flush_pending_deletions(&c, &self.state)?;
                Ok(f(&c))
            })
        };
        match &self.shared_ctx {
            Some(ctx) => run(ctx),
            None => {
                let ctx = Context::full(&self.rt)?;
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

// These Zend APIs maintain a nesting counter. Blocking switches prevents PHP
// from suspending while Rust borrows and the QuickJS runtime lock are live.
unsafe extern "C" {
    fn zend_fiber_switch_block();
    fn zend_fiber_switch_unblock();
}

struct ExecutionGuard<'a> {
    engine: &'a Engine,
}

impl Drop for ExecutionGuard<'_> {
    fn drop(&mut self) {
        self.engine.active_ctx.set(None);
        self.engine.disarm_deadline();
        // SAFETY: paired with the block in eval_in, including error unwinding.
        unsafe { zend_fiber_switch_unblock() };
    }
}
