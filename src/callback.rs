//! `Js\Callback`: a PHP-facing wrapper around a JS function handed to PHP.
//!
//! The JS function itself never leaves the engine; the callback holds only an
//! integer id into the JS-side registry plus a handle back to the engine. When
//! invoked from PHP it re-enters JS — reusing the live context if a host call
//! is already in flight, else acquiring the runtime lock afresh.

use crate::engine::Engine;
use crate::marshal::{middle_to_zval, zval_to_middle, MiddleValue};
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
    /// Invoke the JS callback: `$cb(...$args)`.
    pub fn __invoke(&self, args: &[&Zval]) -> PhpResult<Zval> {
        self.invoke_inner(args)
    }

    /// Explicit form: `$cb->call([...$args])`.
    pub fn call(&self, args: &[&Zval]) -> PhpResult<Zval> {
        self.invoke_inner(args)
    }
}
