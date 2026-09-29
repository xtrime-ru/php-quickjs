//! The trust boundary: native host imports dispatch registered PHP callables
//! through a flat allowlist and the frozen `php.*` facade.

use crate::engine::Engine;
use crate::error::{throw_host_error, HostError};
use crate::handles::HandleTable;
use crate::manifest::ManifestEntry;
use crate::marshal::{
    js_to_data, js_to_middle, middle_to_js, middle_to_zval, zval_to_middle, MiddleValue,
};
use ext_php_rs::convert::IntoZvalDyn;
use ext_php_rs::types::{ZendCallable, Zval};
use rquickjs::{Ctx, Exception, Function, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

/// Runtime support injected into each context.
const RUNTIME_JS: &str = include_str!("js/runtime.js");

/// Shared host-side state behind the bridge. Single-threaded (PHP NTS), so
/// `Rc`/`RefCell` interior mutability is sufficient and correct.
#[derive(Default)]
pub struct BridgeState {
    /// Dotted name -> PHP callable (the trust boundary allowlist).
    dispatch: RefCell<HashMap<String, Zval>>,
    /// Ordered registration manifest (drives the facade and the `.d.ts`).
    manifest: RefCell<Vec<ManifestEntry>>,
    /// Anonymous PHP callables handed to JS, keyed by id.
    php_funcs: RefCell<HashMap<u64, Zval>>,
    next_php: Cell<u64>,
    /// Live PHP objects granted to JS as opaque handles.
    pub handles: HandleTable,
    /// Back-reference to the owning engine (for invoking JS callbacks).
    engine: RefCell<Weak<Engine>>,
    /// JS-callback ids whose PHP wrapper was dropped, awaiting release from the
    /// JS registry (deferred to the next eval boundary; see `JsCallback::drop`).
    pending_fn_deletions: RefCell<Vec<u64>>,
    messages: RefCell<MessageQueue>,
}

impl BridgeState {
    pub fn new(max_queued_message_bytes: usize) -> Rc<Self> {
        Rc::new(Self {
            messages: RefCell::new(MessageQueue::new(max_queued_message_bytes)),
            ..Self::default()
        })
    }

    /// Queue a JS-callback id for release at the next eval boundary.
    pub fn queue_fn_deletion(&self, id: u64) {
        self.pending_fn_deletions.borrow_mut().push(id);
    }

    fn take_pending_deletions(&self) -> Vec<u64> {
        std::mem::take(&mut self.pending_fn_deletions.borrow_mut())
    }

    pub fn set_engine(&self, engine: Weak<Engine>) {
        *self.engine.borrow_mut() = engine;
    }

    pub fn engine(&self) -> Option<Rc<Engine>> {
        self.engine.borrow().upgrade()
    }

    /// Register a PHP callable under a flat, dotted name.
    pub fn register(
        &self,
        name: String,
        callable: &Zval,
        types: Option<String>,
    ) -> Result<(), String> {
        if !callable.is_callable() {
            return Err(format!("value registered as '{name}' is not callable"));
        }
        self.dispatch
            .borrow_mut()
            .insert(name.clone(), callable.shallow_clone());
        let mut manifest = self.manifest.borrow_mut();
        if let Some(existing) = manifest.iter_mut().find(|e| e.name == name) {
            existing.types = types;
        } else {
            manifest.push(ManifestEntry { name, types });
        }
        Ok(())
    }

    /// Register an anonymous PHP callable (handed to JS), returning its id.
    pub fn register_php_fn(&self, callable: &Zval) -> u64 {
        let id = self.next_php.get() + 1;
        self.next_php.set(id);
        self.php_funcs
            .borrow_mut()
            .insert(id, callable.shallow_clone());
        id
    }

    pub(crate) fn release_php_fns(&self, ids: &[u64]) {
        // Drop captured PHP values after releasing the RefCell borrow: PHP
        // destructors may call back into the extension.
        let removed: Vec<_> = {
            let mut functions = self.php_funcs.borrow_mut();
            ids.iter().filter_map(|id| functions.remove(id)).collect()
        };
        drop(removed);
    }

    fn push_message(&self, value: MiddleValue, bytes: usize) -> Result<(), &'static str> {
        self.messages.borrow_mut().push(value, bytes)
    }

    fn message_capacity(&self) -> usize {
        self.messages.borrow().remaining()
    }

    pub(crate) fn drain_messages(&self) -> Vec<MiddleValue> {
        self.messages.borrow_mut().drain()
    }

    pub fn get_php_fn(&self, id: u64) -> Option<Zval> {
        self.php_funcs.borrow().get(&id).map(Zval::shallow_clone)
    }

    pub fn manifest_snapshot(&self) -> Vec<ManifestEntry> {
        self.manifest.borrow().clone()
    }

    fn names(&self) -> Vec<String> {
        self.manifest
            .borrow()
            .iter()
            .map(|e| e.name.clone())
            .collect()
    }
}

/// Invoke a PHP callable with already-marshaled args, returning its result.
fn call_php(
    callable_zv: &Zval,
    args: &[MiddleValue],
    state: &BridgeState,
) -> Result<MiddleValue, HostError> {
    let zvals: Vec<Zval> = args
        .iter()
        .map(|m| middle_to_zval(m, state))
        .collect::<Result<_, _>>()
        .map_err(HostError::internal)?;
    let params: Vec<&dyn IntoZvalDyn> = zvals.iter().map(|z| z as &dyn IntoZvalDyn).collect();

    let callable =
        ZendCallable::new(callable_zv).map_err(|e| HostError::internal(e.to_string()))?;
    let ret = callable
        .try_call(params)
        .map_err(crate::error::php_exception_info)?;
    zval_to_middle(&ret, state).map_err(HostError::internal)
}

/// Dispatch a named host call. Returns `Err` for an unknown capability (the
/// trust-boundary rejection) or a failed call.
fn host_call(
    state: &BridgeState,
    name: &str,
    args: Vec<MiddleValue>,
) -> Result<MiddleValue, HostError> {
    let callable_zv = state
        .dispatch
        .borrow()
        .get(name)
        .map(Zval::shallow_clone)
        .ok_or_else(|| HostError::internal(format!("unknown capability: {name}")))?;
    call_php(&callable_zv, &args, state)
}

/// Invoke an anonymous PHP callable (one previously handed to JS) by id.
fn php_fn_call(
    state: &BridgeState,
    id: u64,
    args: Vec<MiddleValue>,
) -> Result<MiddleValue, HostError> {
    let callable_zv = state
        .get_php_fn(id)
        .ok_or_else(|| HostError::internal(format!("unknown PHP callable id {id}")))?;
    call_php(&callable_zv, &args, state)
}

fn invoke_host<'js>(
    ctx: &Ctx<'js>,
    state: &BridgeState,
    payload: Value<'js>,
    call: impl FnOnce(Vec<MiddleValue>) -> Result<MiddleValue, HostError>,
) -> rquickjs::Result<Value<'js>> {
    if !payload.is_array() {
        return Err(Exception::throw_type(
            ctx,
            "host arguments must be an array",
        ));
    }
    let MiddleValue::Array(args) = js_to_middle(ctx, payload, state)? else {
        unreachable!("a JS array converts to MiddleValue::Array")
    };
    let result = call(args).map_err(|error| throw_host_error(ctx, &error))?;
    middle_to_js(ctx, &result)
}

/// Install native imports, runtime support, and frozen
/// `php.*` facade. Call once per `eval`, before guest code runs.
pub fn install<'js>(ctx: &Ctx<'js>, state: Rc<BridgeState>) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    // Snapshot before borrowing the queue: getters can execute arbitrary JS.
    if globals
        .get::<_, Option<rquickjs::Object>>("quickjs")?
        .is_none()
    {
        let message_state = state.clone();
        let post_message = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, payload: Value<'js>| -> rquickjs::Result<()> {
                let (value, bytes) = js_to_data(&ctx, payload, message_state.message_capacity())?;
                message_state
                    .push_message(value, bytes)
                    .map_err(|e| Exception::throw_type(&ctx, e))
            },
        )?;
        let quickjs = rquickjs::Object::new(ctx.clone())?;
        quickjs.set("postMessage", post_message)?;
        globals.set("quickjs", quickjs)?;
        ctx.eval::<(), _>("Object.freeze(globalThis.quickjs); Object.defineProperty(globalThis, 'quickjs', {value: globalThis.quickjs, writable: false, configurable: false})")?;
    }
    // JS -> PHP capability calls use the same native conversion as eval.
    let host_state = state.clone();
    let host = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, name: String, payload: Value<'js>| -> rquickjs::Result<Value<'js>> {
            invoke_host(&ctx, &host_state, payload, |args| {
                host_call(&host_state, &name, args)
            })
        },
    )?;
    globals.set("__host", host)?;

    // JS -> host: invoke an anonymous PHP callable handed to JS earlier.
    let php_state = state.clone();
    let php_invoke = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, id: f64, payload: Value<'js>| -> rquickjs::Result<Value<'js>> {
            invoke_host(&ctx, &php_state, payload, |args| {
                php_fn_call(&php_state, id as u64, args)
            })
        },
    )?;
    globals.set("__php_invoke", php_invoke)?;

    // Runtime support must precede the frozen facade.
    ctx.eval::<(), _>(RUNTIME_JS)?;
    ctx.eval::<(), _>(build_facade(&state.names()))?;

    flush_pending_deletions(ctx, &state)
}

pub(crate) fn flush_pending_deletions(ctx: &Ctx<'_>, state: &BridgeState) -> rquickjs::Result<()> {
    let stale = state.take_pending_deletions();
    if !stale.is_empty() {
        // A fresh isolated realm has no registry; its preceding realm is gone.
        let Ok(del) = ctx.globals().get::<_, Function>("__deleteJsFn") else {
            return Ok(());
        };
        for id in stale {
            del.call::<_, ()>((id as f64,))?;
        }
    }
    Ok(())
}

/// Generate the JS that builds the frozen `php.*` tree from the dotted names.
fn build_facade(names: &[String]) -> String {
    let mut src = String::from(
        "(function(){\n\
         var php = {};\n",
    );

    for name in names {
        let parts: Vec<&str> = name.split('.').collect();
        let mut path = String::from("php");
        for part in &parts[..parts.len() - 1] {
            let next = format!("{path}[{}]", js_string(part));
            src.push_str(&format!("{next} = {next} || {{}};\n"));
            path = next;
        }
        let leaf = format!("{path}[{}]", js_string(parts[parts.len() - 1]));
        src.push_str(&format!(
            "{leaf} = function(){{ return globalThis.__host({}, Array.prototype.slice.call(arguments)); }};\n",
            js_string(name)
        ));
    }

    src.push_str(
        "(function deepFreeze(o){ Object.keys(o).forEach(function(k){ var v=o[k]; if(v && (typeof v==='object'||typeof v==='function')) deepFreeze(v); }); Object.freeze(o); })(php);\n\
         globalThis.php = php;\n\
         })();\n",
    );
    src
}

/// Produce a valid JS string literal for `s`.
fn js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facade_builds_nested_paths() {
        let src = build_facade(&["db.query".into(), "log.info".into()]);
        assert!(src.contains("php[\"db\"] = php[\"db\"] || {};"));
        assert!(src.contains("php[\"db\"][\"query\"] = function()"));
        assert!(src.contains("__host(\"db.query\""));
        assert!(src.contains("Object.freeze"));
    }
}

pub const DEFAULT_MAX_QUEUED_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const MESSAGE_OVERHEAD: usize = 128;

/// Detached data snapshots waiting for a PHP drain.
#[derive(Default)]
struct MessageQueue {
    messages: Vec<MiddleValue>,
    bytes: usize,
    limit: usize,
}
impl MessageQueue {
    fn new(limit: usize) -> Self {
        Self {
            messages: Vec::new(),
            bytes: 0,
            limit,
        }
    }

    fn push(&mut self, value: MiddleValue, bytes: usize) -> Result<(), &'static str> {
        let total = self
            .bytes
            .saturating_add(bytes)
            .saturating_add(MESSAGE_OVERHEAD);
        if total > self.limit {
            return Err("message queue limit exceeded");
        }
        self.bytes = total;
        self.messages.push(value);
        Ok(())
    }

    fn drain(&mut self) -> Vec<MiddleValue> {
        self.bytes = 0;
        std::mem::take(&mut self.messages)
    }

    fn remaining(&self) -> usize {
        self.limit
            .saturating_sub(self.bytes)
            .saturating_sub(MESSAGE_OVERHEAD)
    }
}
