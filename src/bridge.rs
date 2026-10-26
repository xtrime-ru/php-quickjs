//! The trust boundary: native host imports dispatch registered PHP callables
//! through a flat allowlist and the frozen `php.*` facade.

use crate::engine::Engine;
use crate::error::HostError;
use crate::handles::HandleTable;
use crate::manifest::ManifestEntry;
use crate::marshal::{middle_to_zval, zval_to_middle, MiddleValue};
use ext_php_rs::convert::IntoZvalDyn;
use ext_php_rs::types::{ZendCallable, Zval};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

/// Runtime support injected into each context.
pub(crate) const RUNTIME_JS: &str = include_str!("js/runtime.js");

/// Shared host-side state behind the bridge. Single-threaded (PHP NTS), so
/// `Rc`/`RefCell` interior mutability is sufficient and correct.
#[derive(Default)]
pub struct BridgeState {
    /// Dotted name -> PHP callable (the trust boundary allowlist).
    dispatch: RefCell<HashMap<String, Zval>>,
    /// Ordered registration manifest (drives the facade and the `.d.ts`).
    manifest: RefCell<Vec<ManifestEntry>>,
    manifest_revision: Cell<u64>,
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
}

impl BridgeState {
    pub fn new() -> Rc<Self> {
        Rc::new(Self { ..Self::default() })
    }

    /// Queue a JS-callback id for release at the next eval boundary.
    pub fn queue_fn_deletion(&self, id: u64) {
        self.pending_fn_deletions.borrow_mut().push(id);
    }

    pub(crate) fn take_pending_deletions(&self) -> Vec<u64> {
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
        let previous = self
            .dispatch
            .borrow_mut()
            .insert(name.clone(), callable.shallow_clone());
        let mut manifest = self.manifest.borrow_mut();
        if let Some(existing) = manifest.iter_mut().find(|e| e.name == name) {
            existing.types = types;
        } else {
            manifest.push(ManifestEntry { name, types });
        }
        drop(manifest);
        self.manifest_revision.set(self.manifest_revision.get() + 1);
        drop(previous);
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

    pub fn get_php_fn(&self, id: u64) -> Option<Zval> {
        self.php_funcs.borrow().get(&id).map(Zval::shallow_clone)
    }

    pub fn manifest_snapshot(&self) -> Vec<ManifestEntry> {
        self.manifest.borrow().clone()
    }

    pub fn manifest_revision(&self) -> u64 {
        self.manifest_revision.get()
    }

    pub fn clear(&self) {
        let dispatch = std::mem::take(&mut *self.dispatch.borrow_mut());
        let functions = std::mem::take(&mut *self.php_funcs.borrow_mut());
        self.manifest.borrow_mut().clear();
        self.pending_fn_deletions.borrow_mut().clear();
        self.handles.clear();
        drop(dispatch);
        drop(functions);
    }
}

/// Invoke a PHP callable with already-marshaled args, returning its result.
pub(crate) fn call_php(
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
pub(crate) fn host_call(
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
pub(crate) fn php_fn_call(
    state: &BridgeState,
    id: u64,
    args: Vec<MiddleValue>,
) -> Result<MiddleValue, HostError> {
    let callable_zv = state
        .get_php_fn(id)
        .ok_or_else(|| HostError::internal(format!("unknown PHP callable id {id}")))?;
    call_php(&callable_zv, &args, state)
}

/// Generate the JS that builds the frozen `php.*` tree from the dotted names.
pub(crate) fn build_facade(names: &[String]) -> String {
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
pub(crate) struct MessageQueue {
    messages: Vec<MiddleValue>,
    bytes: usize,
    limit: usize,
}
impl MessageQueue {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            messages: Vec::new(),
            bytes: 0,
            limit,
        }
    }

    pub(crate) fn push(&mut self, value: MiddleValue, bytes: usize) -> Result<(), &'static str> {
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

    pub(crate) fn drain(&mut self) -> Vec<MiddleValue> {
        self.bytes = 0;
        std::mem::take(&mut self.messages)
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub(crate) fn remaining(&self) -> usize {
        self.limit
            .saturating_sub(self.bytes)
            .saturating_sub(MESSAGE_OVERHEAD)
    }
}
