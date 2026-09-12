//! `Js\Callback`: a PHP-facing wrapper around a JS function handed to PHP.
//!
//! The JS function itself never leaves the engine; the callback holds only an
//! integer id into the JS-side registry plus a handle back to the engine. When
//! invoked from PHP it re-enters JS — reusing the live context if a host call
//! is already in flight, else acquiring the runtime lock afresh.

use crate::engine::Engine;
use crate::marshal::{middle_to_js, middle_to_zval, zval_to_middle, MiddleValue};
use ext_php_rs::prelude::*;
use ext_php_rs::types::Zval;
use rquickjs::{Ctx, Function, TypedArray, Value};
use std::rc::Rc;

#[php_class]
#[php(name = "Js\\Callback")]
pub struct JsCallback {
    pub id: u64,
    pub engine: Rc<Engine>,
}

impl JsCallback {
    pub fn new(id: u64, engine: Rc<Engine>) -> Self {
        JsCallback { id, engine }
    }
}

impl Drop for JsCallback {
    /// Queue this callback's registry entry for release. Deletion is deferred
    /// (not done here) and flushed at the next eval boundary: a JS function can
    /// be round-tripped PHP->JS within a single host call, dropping a transient
    /// wrapper while JS still needs the entry, so deleting eagerly would race.
    /// Queuing touches no JS and cannot re-enter the engine.
    fn drop(&mut self) {
        self.engine.state.queue_fn_deletion(self.id);
    }
}

impl JsCallback {
    /// Invoke the underlying JS function with the given (already PHP-side) args.
    fn invoke_inner(&self, args: &[&Zval]) -> PhpResult<Zval> {
        let _guard = self.engine.enter().map_err(PhpException::default)?;

        let mut middle_args = Vec::with_capacity(args.len());
        for a in args {
            middle_args.push(zval_to_middle(a, &self.engine.state).map_err(PhpException::default)?);
        }
        let payload = MiddleValue::Array(middle_args)
            .to_msgpack()
            .map_err(|e| PhpException::default(e.to_string()))?;
        let id = self.id;
        let engine = self.engine.clone();

        let run = move |ctx: &Ctx<'_>| -> PhpResult<Zval> {
            let globals = ctx.globals();
            let invoke: Function = globals
                .get("__invokeJs")
                .map_err(|e| PhpException::default(format!("__invokeJs missing: {e}")))?;
            let arg_bytes = TypedArray::new(ctx.clone(), payload.clone())
                .map_err(|e| PhpException::default(e.to_string()))?;
            // A JS error here re-surfaces a host exception (unwrapped to its
            // original PHP class) or becomes a QuickJSEvalException.
            let ret: Value = invoke
                .call((id as f64, arg_bytes))
                .map_err(|e| engine.callback_error(ctx, e))?;
            let ta = TypedArray::<u8>::from_value(ret).map_err(|e| {
                PhpException::default(format!("JS callback did not return bytes: {e}"))
            })?;
            let bytes = ta
                .as_bytes()
                .ok_or_else(|| PhpException::default("detached result buffer".to_owned()))?;
            let mv = MiddleValue::from_msgpack(bytes)
                .map_err(|e| PhpException::default(e.to_string()))?;
            middle_to_zval(&mv, &engine.state).map_err(PhpException::default)
        };

        if !self.engine.is_active() && self.engine.shared_ctx().is_none() {
            return Err(PhpException::default(
                "JS callback invoked outside its eval (isolated QuickJS instance)".to_owned(),
            ));
        }
        self.engine
            .eval_in(run)
            .map_err(|e| PhpException::default(e.to_string()))?
    }
}

#[php_impl]
impl JsCallback {
    /// Direct, data-only dispatch followed by a bounded job batch. Pass null
    /// instead of an argument list to continue jobs without invoking the callback.
    /// Returns queued messages, executed job count, and pending-job status.
    #[php(defaults(maxJobs = 100))]
    pub fn dispatch(&self, args: &Zval, maxJobs: i64) -> PhpResult<Zval> {
        if maxJobs <= 0 {
            return Err(PhpException::default(
                "maxJobs must be greater than zero".to_owned(),
            ));
        }
        if self.engine.shared_ctx().is_none() {
            return Err(PhpException::default(
                "dispatch requires shared mode".to_owned(),
            ));
        }
        if self.engine.is_active() {
            return Err(PhpException::default(
                "Cannot dispatch while JavaScript is executing".to_owned(),
            ));
        }
        let middle = zval_to_middle(args, &self.engine.state).map_err(PhpException::default)?;
        if !matches!(middle, MiddleValue::Null | MiddleValue::Array(_)) {
            return Err(PhpException::default(
                "args must be a list or null".to_owned(),
            ));
        }
        let _guard = self.engine.enter().map_err(PhpException::default)?;
        self.engine.state.collecting.set(true);
        let _messages = MessageGuard {
            state: &self.engine.state,
        };
        self.engine
            .eval_in(|ctx| {
                if let MiddleValue::Array(items) = &middle {
                    let get: Function = ctx
                        .globals()
                        .get("__getJsFn")
                        .map_err(|e| self.engine.callback_error(ctx, e))?;
                    let fun: Function = get
                        .call((self.id as f64,))
                        .map_err(|e| self.engine.callback_error(ctx, e))?;
                    let mut call_args = rquickjs::function::Args::new(ctx.clone(), items.len());
                    for item in items {
                        call_args
                            .push_arg(
                                middle_to_js(ctx, item, &self.engine.state)
                                    .map_err(|e| self.engine.callback_error(ctx, e))?,
                            )
                            .map_err(|e| self.engine.callback_error(ctx, e))?;
                    }
                    // Dispatch is a notification; its return value is deliberately ignored.
                    fun.call_arg::<Value>(call_args)
                        .map_err(|e| self.engine.callback_error(ctx, e))?;
                }
                let jobs = self.engine.run_jobs(ctx, maxJobs)?;
                let pending = unsafe {
                    rquickjs::qjs::JS_IsJobPending(rquickjs::qjs::JS_GetRuntime(
                        ctx.as_raw().as_ptr(),
                    ))
                };
                let messages = std::mem::take(&mut *self.engine.state.messages.borrow_mut());
                middle_to_zval(
                    &MiddleValue::Map(vec![
                        ("messages".to_owned(), MiddleValue::Array(messages)),
                        ("jobs".to_owned(), MiddleValue::Int(jobs)),
                        ("pending".to_owned(), MiddleValue::Bool(pending)),
                    ]),
                    &self.engine.state,
                )
                .map_err(PhpException::default)
            })
            .map_err(|e| PhpException::default(e.to_string()))?
    }

    /// Invoke the JS callback: `$cb(...$args)`.
    pub fn __invoke(&self, args: &[&Zval]) -> PhpResult<Zval> {
        self.invoke_inner(args)
    }

    /// Explicit form: `$cb->call([...$args])`.
    pub fn call(&self, args: &[&Zval]) -> PhpResult<Zval> {
        self.invoke_inner(args)
    }
}

/// Clear partial output on failure as well as successful drains.
struct MessageGuard<'a> {
    state: &'a crate::bridge::BridgeState,
}
impl Drop for MessageGuard<'_> {
    fn drop(&mut self) {
        self.state.collecting.set(false);
        self.state.messages.borrow_mut().clear();
        self.state.message_bytes.set(0);
    }
}
