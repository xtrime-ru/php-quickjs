// The constructor exposes camelCase PHP named arguments (`memoryLimit`,
// `timeoutMs`, `maxStack`) to match the public API; ext-php-rs derives PHP arg
// names from the Rust idents, so the non-snake-case lint is allowed here.
#![allow(non_snake_case)]

use ext_php_rs::prelude::*;
use ext_php_rs::types::Zval;
use std::rc::Rc;

mod bridge;
mod callback;
mod engine;
mod error;
mod exceptions;
mod handles;
mod manifest;
mod marshal;
mod sandbox;
mod transpile;
mod worker;

use engine::Engine;
use marshal::{middle_to_zval, zval_to_middle};

/// An embedded QuickJS sandbox with a typed, bidirectional PHP bridge.
#[php_class]
#[php(name = "QuickJS")]
pub struct QuickJS {
    engine: Rc<Engine>,
}

#[php_impl]
impl QuickJS {
    /// Construct a sandbox. Heap and time limits default to unbounded:
    /// - `memoryLimit`: max heap bytes (alloc-bomb guard)
    /// - `timeoutMs`: wall-clock budget per eval, callback, or job batch
    /// - `maxStack`: max native stack bytes
    /// - `maxQueuedMessageBytes`: maximum accounted bytes waiting in the message queue
    /// - `transpileCacheMaxBytes`: retained cache strings (default 32 MiB; 0 disables cache)
    /// - `transpileCacheMaxEntries`: retained cache entries (default 256; 0 disables cache)
    /// - `isolated`: when true, each `eval()` runs in a fresh global realm (its
    ///   own world); cross-eval globals and persistent JS callbacks are not
    ///   kept. Defaults to false (one shared, persistent realm per instance).
    #[php(defaults(memoryLimit = None, timeoutMs = None, maxStack = None, isolated = false, maxQueuedMessageBytes = None, transpileCacheMaxBytes = 33554432, transpileCacheMaxEntries = 256))]
    pub fn __construct(
        memoryLimit: Option<i64>,
        timeoutMs: Option<i64>,
        maxStack: Option<i64>,
        isolated: bool,
        maxQueuedMessageBytes: Option<i64>,
        transpileCacheMaxBytes: i64,
        transpileCacheMaxEntries: i64,
    ) -> PhpResult<Self> {
        let max_queued_message_bytes = match maxQueuedMessageBytes {
            None => bridge::DEFAULT_MAX_QUEUED_MESSAGE_BYTES,
            Some(limit) if limit > 0 => usize::try_from(limit).map_err(|_| {
                PhpException::from_message("maxQueuedMessageBytes is too large".to_owned())
            })?,
            _ => {
                return Err(PhpException::from_message(
                    "maxQueuedMessageBytes must be positive".to_owned(),
                ))
            }
        };
        let transpile_cache_max_bytes = usize::try_from(transpileCacheMaxBytes).map_err(|_| {
            PhpException::from_message(
                "transpileCacheMaxBytes must be a non-negative integer fitting usize".to_owned(),
            )
        })?;
        let transpile_cache_max_entries =
            usize::try_from(transpileCacheMaxEntries).map_err(|_| {
                PhpException::from_message(
                    "transpileCacheMaxEntries must be a non-negative integer fitting usize"
                        .to_owned(),
                )
            })?;
        let engine = Engine::new(
            memoryLimit.unwrap_or(0).max(0) as usize,
            timeoutMs.unwrap_or(0).max(0) as u64,
            maxStack.unwrap_or(0).max(0) as usize,
            isolated,
            max_queued_message_bytes,
            transpile_cache_max_entries,
            transpile_cache_max_bytes,
        )?;
        Ok(QuickJS { engine })
    }

    /// Register a PHP callable under a flat, dotted capability name, optionally
    /// with a TypeScript signature used for `.d.ts` generation. The name
    /// becomes callable from JS as `php.<dotted.name>(...)`.
    pub fn register(&self, name: String, callable: &Zval, types: Option<String>) -> PhpResult<()> {
        self.engine.ensure_open()?;
        self.engine
            .state
            .register(name, callable, types)
            .map_err(PhpException::from_message)
    }

    /// Evaluate source and marshal its result back to PHP. TypeScript is
    /// transpiled by default; false executes JavaScript directly.
    #[php(defaults(typescript = true))]
    pub fn eval(&self, code: String, typescript: bool) -> PhpResult<Zval> {
        self.engine.eval(code, typescript)
    }

    /// JavaScript always executes on a dedicated native thread.
    pub fn executionMode() -> &'static str {
        "worker"
    }

    /// Stop this runtime and release its native worker. Further entries fail.
    pub fn close(&self) -> PhpResult<()> {
        self.engine.close()
    }

    /// Whether Promise jobs are ready. This does not include pending host I/O.
    pub fn hasPendingJobs(&self) -> PhpResult<bool> {
        self.require_shared_jobs()?;
        Ok(self.engine.has_jobs()?.bool().unwrap_or(false))
    }

    /// Execute at most maxJobs ready jobs without waiting for host I/O.
    #[php(defaults(maxJobs = 100))]
    pub fn executePendingJobs(&self, maxJobs: i64) -> PhpResult<i64> {
        self.require_shared_jobs()?;
        if maxJobs <= 0 {
            return Err(PhpException::from_message(
                "maxJobs must be greater than zero".to_owned(),
            ));
        }
        Ok(self.engine.jobs(maxJobs)?.long().unwrap_or(0))
    }

    /// Drain messages emitted with `quickjs.postMessage`. Safe to call after
    /// an awaiting driver yields; no JS entry or Promise job is executed.
    pub fn drainMessages(&self) -> PhpResult<Zval> {
        let messages = marshal::MiddleValue::Array(self.engine.drain_messages()?);
        middle_to_zval(&messages, &self.engine.state).map_err(PhpException::from_message)
    }

    /// Return the registration manifest as an array of `['name'=>..., 'types'=>...]`.
    pub fn manifest(&self) -> PhpResult<Zval> {
        let state = &self.engine.state;
        let entries = state.manifest_snapshot();
        let mv = marshal::MiddleValue::Array(
            entries
                .iter()
                .map(|e| {
                    marshal::MiddleValue::Map(vec![
                        ("name".to_owned(), marshal::MiddleValue::Str(e.name.clone())),
                        (
                            "types".to_owned(),
                            e.types
                                .clone()
                                .map_or(marshal::MiddleValue::Null, marshal::MiddleValue::Str),
                        ),
                    ])
                })
                .collect(),
        );
        marshal::middle_to_zval(&mv, state).map_err(PhpException::from_message)
    }

    /// Generate a TypeScript `.d.ts` declaration for the `php` global from the
    /// current manifest. Guests can author against these types; they are erased
    /// at runtime (the host still validates at the boundary).
    pub fn dts(&self) -> String {
        manifest::to_dts(&self.engine.state.manifest_snapshot())
    }

    /// Grant JS access to a live PHP value (e.g. a database connection) as an
    /// opaque integer handle. The value is kept host-side; JS only ever sees
    /// the id and must pass it back to a capability to use it.
    pub fn grant(&self, resource: &Zval) -> PhpResult<i64> {
        self.engine.ensure_open()?;
        Ok(self.engine.state.handles.grant(resource))
    }

    /// Resolve a handle previously returned by `grant()` back to its live PHP
    /// value. Throws if the handle is unknown (e.g. already revoked).
    pub fn resolve(&self, handle: i64) -> PhpResult<Zval> {
        self.engine
            .state
            .handles
            .resolve(handle)
            .ok_or_else(|| PhpException::from_message(format!("unknown handle: {handle}")))
    }

    /// Revoke a handle, releasing the host-side reference. Returns whether the
    /// handle existed.
    pub fn revoke(&self, handle: i64) -> bool {
        self.engine.state.handles.revoke(handle)
    }

    /// Round-trip a PHP value through the JS engine and back, exercising every
    /// leg of the marshaling pipeline (PHP -> middle -> JS -> middle -> PHP).
    pub fn roundtrip(&self, value: &Zval) -> PhpResult<Zval> {
        let state = self.engine.state.clone();
        let middle = zval_to_middle(value, &state).map_err(PhpException::from_message)?;
        self.engine.roundtrip(middle)
    }
}

impl QuickJS {
    fn require_shared_jobs(&self) -> PhpResult<()> {
        if self.engine.isolated() {
            return Err(PhpException::from_message(
                "Promise jobs require shared mode (isolated: false)".to_owned(),
            ));
        }
        Ok(())
    }
}

#[php_module]
pub fn module(module: ModuleBuilder) -> ModuleBuilder {
    module
        .version(env!("CARGO_PKG_VERSION"))
        // Exceptions first so subclasses can resolve their parent class entry.
        .class::<exceptions::QuickJSException>()
        .class::<exceptions::QuickJSEvalException>()
        .class::<exceptions::QuickJSTimeoutException>()
        .class::<exceptions::QuickJSMemoryException>()
        .class::<QuickJS>()
        .class::<callback::JsCallback>()
}
