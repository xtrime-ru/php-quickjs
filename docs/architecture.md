# Architecture

## Three worlds, one bridge

```
┌─ PHP (trusted, full Zend) ──┐   ┌─ Rust extension ─┐   ┌─ QuickJS (untrusted) ─┐
│ $js->register(...)          │   │  owns the engine  │   │  php.module.fn()       │
│ $js->eval(tsCode)           │◄─►│  host capabilities│◄─►│  frozen php.* facade   │
│ $js->grant($obj)            │   │  native marshal   │   │  guest TS-as-JS        │
└─────────────────────────────┘   └───────────────────┘   └────────────────────────┘
        ext-php-rs (zval ↔ Rust)        rquickjs (Rust ↔ JSValue)
```

It is **one process with a dedicated native thread per QuickJS instance**. The Rust extension is a `cdylib` that PHP
loads natively; `QuickJS`, `Js\Callback`, and the `QuickJS*Exception` classes are
real PHP classes implemented in Rust.

The capability trust boundary is a flat dispatch table reached through
`__host`. `quickjs.postMessage()` is a separate data-only output sink with a
bounded native queue.

## How one call flows, end to end

Take `php.math.add(2, 3)` from a guest script.

1. **`eval(tsCode)`** (`lib.rs` → `engine.rs` → worker RPC → `transpile.rs`) runs oxc on the worker: types are stripped,
   the target is esnext (a near-identity transform), and the output is JS plus a
   source map. The map is cached **host-side**, keyed by a content hash; it never
   enters the sandbox. QuickJS only ever sees JavaScript.

2. The guest calls `php.math.add(2, 3)`. `php` is not magic — it is a **frozen JS
   object tree** that `bridge.rs` generated from the registration manifest. The
   leaf `php.math.add` is a function:

   ```js
   php.math.add = function () {
     return globalThis.__host("math.add", Array.prototype.slice.call(arguments));
   };
   ```

3. `__host` in `worker.rs` converts JS arguments to owned `MiddleValue`
   data and sends a host-call request. `engine.rs` receives it on the PHP thread.
   `bridge.rs` looks the name up in the allowlist, converts arguments to Zend
   values and invokes the PHP callable. Only this PHP thread touches Zend.

4. The reply travels back through the command channel as owned Rust data. The
   worker reconstructs its JS result. No JSON encoding is involved. While waiting
   for this reply it can service nested JS function requests from PHP.

Adding a capability never changes the dispatch mechanism: all capabilities use
one dispatch table. The flat, dotted-name list (`manifest()`) is the complete
audit surface for PHP callables.

### The facade is generated, and frozen

`bridge.rs::build_facade` walks the manifest's dotted names into a nested object
tree, makes each leaf a function calling `__host("dotted.name", args)`, then
**deep-freezes** the whole tree. Freezing is a security requirement, not a
nicety: a guest must not be able to reassign `php.http.get` to fool other code.
The facade is rebuilt when the registration manifest changes. Its frozen object
is reused and rebound at entry otherwise, avoiding repeated compilation.

## Value marshaling

Each side converts against a neutral middle type, `marshal.rs::MiddleValue`.
All values cross through native conversion.

```
JS value  ──js_to_middle──►  MiddleValue  ──middle_to_zval──►  PHP zval
JS value  ◄─middle_to_js───  MiddleValue  ◄─zval_to_middle───  PHP zval
```

| JS                | MiddleValue | PHP                         |
|-------------------|-------------|-----------------------------|
| null / undefined  | Null        | null                        |
| boolean           | Bool        | bool                        |
| number (integer)  | Int (i64)   | int                         |
| number (float)    | Float (f64) | float                       |
| string            | Str         | string (UTF-8)              |
| Uint8Array        | Bytes       | binary string               |
| Array             | Array       | indexed array               |
| Object            | Map         | associative array           |
| function          | JsFn / PhpFn | `Js\Callback` ⇄ callable    |

Notes:
- A PHP array with sequential `0..n` keys becomes a JS `Array`; otherwise a JS
  object. A non-UTF-8 PHP string crosses as bytes (a `Uint8Array`).
- Integers beyond 2^53 lose precision when represented as JS numbers.

## Bidirectional functions

Functions cross as **registry references**: the real callable remains on its
owning side.

- **`PhpFn(id)`** — a PHP callable handed to JS. The callable is stored in a
  host-side registry (`bridge.rs`); JS receives a wrapper function that routes
  back through `__php_invoke(id, …)`.
- **`JsFn(id)`** — a JS function handed to PHP. The function is stored in a
  **JS-side** registry (`jsFns` in `runtime.js`); PHP receives a `Js\Callback`
  object holding the integer `id`.

Rust converts these references without JS-side wrapping or decoding.

### Invoking a JS function from PHP

`Js\Callback::__invoke` (`callback.rs`) looks up the function with `__getJsFn`,
converts its arguments, invokes it, and awaits any returned Promise.

The PHP wrapper holds an integer function ID and an owning engine reference;
actual JS objects never cross threads. PHP callable and resource handles likewise
stay on the PHP side. Owned `MiddleValue` data is `Send`; no unsafe `Send` shim is
used to move a runtime, Zend value, context or function between threads.

`worker.rs` owns the runtime and processes requests, host replies and ready jobs.
Returned Promises are tracked independently. A synchronous host wait can process
nested function requests, preventing PHP -> JS -> PHP deadlocks. A depth cap of
200 and inherited deadlines bound recursion. Isolated runtimes cancel remaining
callback requests before releasing their context.

`engine.rs` shares one wait loop for RPC results and native message batches. A
coalesced descriptor notification dispatches results to their waiting Fibers;
host callbacks run on the calling PHP thread and may suspend. Manifest updates
are sent only when registrations change, and a cached facade is reused in JS.
Without an autoloaded Revolt driver, synchronous requests use a blocking receive
loop and unresolved external waits fail clearly. See [asynchronous execution](async.md).

The cached facade and pending Promise roots are explicitly released before
context teardown, including panic unwinding, so native closure cycles cannot
retain a dead runtime. `close()` interrupts and joins the worker, wakes every
waiter, then releases PHP capabilities and handles. User cleanup cannot prevent
those wakeups. The engine validates its creator PID on entry.

## Capability handles

A live PHP object (a PDO connection, a file handle) must never be serialized into
JS. `grant($obj)` stores it in a host-side table (`handles.rs`) and returns an
opaque `int`. JS can do nothing with that int but pass it back to a capability,
which calls `resolve($id)` to recover the live object. The handle **is** the
capability. `revoke($id)` releases it. Granted objects are refcount-bumped so
they survive PHP garbage collection while held.

## The TypeScript fast path

`eval()` accepts TypeScript and transpiles it in-process before QuickJS sees it
— the Bun/esbuild model: transpile-and-go, no type-checking on the hot path.

- **oxc** strips types and targets esnext, so the transform is near-identity and
  QuickJS-native syntax (private fields, optional chaining, …) is not downleveled.
- The result is cached in a content-hash LRU (`transpile.rs`); re-running the same
  source is free.
- The **source map stays host-side** and is used only to remap an error's stack
  back to the original TS coordinates (see [errors](errors.md)).

Type-only constructs (`interface`, `type`, generics, `as`) erase. Constructs that
emit runtime code (`enum`, `namespace`, decorators) are transformed by oxc and
work. Type *checking* is intentionally absent; the pipeline leaves a clean seam
to add a checker later without reshaping anything.

## Security model

| Layer | Contains |
|-------|----------|
| Frozen `php.*` + flat dispatch table | what JS can *name* / reach |
| Capability handles | which live objects JS can *use* |
| Memory / CPU / stack limits | resource abuse (loops, alloc bombs) |
| QuickJS (no JIT) | the worst JS-engine bug class |
| **Outer microVM / gVisor** | QuickJS C memory-corruption → host RCE |

The extension is the *embedder*. The trust boundary is the QuickJS context (for
capabilities) plus an outer VM (for memory safety) — never the extension itself.
For genuinely hostile code, nest the whole extension inside a microVM/gVisor.
