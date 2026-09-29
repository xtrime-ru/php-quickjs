//! `Js\Callback`: a PHP-facing wrapper around a JS function handed to PHP.
//!
//! The JS function itself never leaves the engine; the callback holds only an
//! integer id into the JS-side registry plus a handle back to the engine. When
//! invoked from PHP it re-enters JS — reusing the live context if a host call
//! is already in flight, else acquiring the runtime lock afresh.

use crate::engine::Engine;
use crate::marshal::{
    arguments_to_middle, data_arguments, js_to_middle, middle_to_js, middle_to_zval, MiddleValue,
};
use ext_php_rs::prelude::*;
use ext_php_rs::types::{ZendHashTable, Zval};
use rquickjs::{Ctx, Function, Value};
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

        let middle_args =
            arguments_to_middle(args, &self.engine.state).map_err(PhpException::default)?;
        let id = self.id;
        let engine = self.engine.clone();

        if self.engine.active_on_other_fiber()? {
            return self.engine.queue_callback(id, middle_args);
        }

        let run = move |ctx: &Ctx<'_>| -> PhpResult<Zval> {
            invoke_callback(ctx, &engine, id, &middle_args)
        };

        if !self.engine.is_active() && self.engine.shared_ctx().is_none() {
            return Err(PhpException::default(
                "JS callback invoked outside its eval (isolated QuickJS instance)".to_owned(),
            ));
        }
        self.engine.eval_in(run)
    }
}

pub(crate) fn invoke_callback(
    ctx: &Ctx<'_>,
    engine: &Engine,
    id: u64,
    args: &[MiddleValue],
) -> PhpResult<Zval> {
    let map_error = |e| engine.callback_error(ctx, e);
    let value = call_js(ctx, engine, id, args)?;
    let value = engine.await_value(ctx, value, map_error)?;
    let middle = js_to_middle(ctx, value, &engine.state).map_err(map_error)?;
    middle_to_zval(&middle, &engine.state).map_err(PhpException::default)
}

fn call_js<'js>(
    ctx: &Ctx<'js>,
    engine: &Engine,
    id: u64,
    args: &[MiddleValue],
) -> PhpResult<Value<'js>> {
    let map_error = |e| engine.callback_error(ctx, e);
    let get: Function = ctx.globals().get("__getJsFn").map_err(&map_error)?;
    let function: Function = get.call((id as f64,)).map_err(&map_error)?;
    let mut call_args = rquickjs::function::Args::new(ctx.clone(), args.len());
    for arg in args {
        call_args
            .push_arg(middle_to_js(ctx, arg, &engine.state).map_err(&map_error)?)
            .map_err(&map_error)?;
    }
    function.call_arg(call_args).map_err(map_error)
}

#[php_impl]
impl JsCallback {
    /// Direct, data-only dispatch followed by a bounded job batch. Pass null
    /// instead of an argument list to continue jobs without invoking the callback.
    /// Returns queued messages, executed job count, and pending-job status.
    #[php(defaults(maxJobs = 100))]
    pub fn dispatch(&self, args: Option<&ZendHashTable>, maxJobs: i64) -> PhpResult<Zval> {
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
        let middle = args
            .map(|args| data_arguments(args, &self.engine.state))
            .transpose()
            .map_err(PhpException::default)?;
        let _guard = self.engine.enter().map_err(PhpException::default)?;
        let _batch = self.engine.state.begin_batch();
        self.engine.eval_in(|ctx| {
            if let Some(MiddleValue::Array(items)) = &middle {
                // Dispatch is a notification; its return value is deliberately ignored.
                call_js(ctx, &self.engine, self.id, items)?;
            }
            let jobs = self.engine.run_jobs(ctx, maxJobs)?;
            let pending = unsafe {
                rquickjs::qjs::JS_IsJobPending(rquickjs::qjs::JS_GetRuntime(ctx.as_raw().as_ptr()))
            };
            let messages = self.engine.state.take_messages();
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
