# Architecture

## Three worlds, one bridge

```
┌─ PHP (trusted, full Zend) ──┐   ┌─ Rust extension ─┐   ┌─ QuickJS (untrusted) ─┐
│ $js->register(...)          │   │  owns the engine  │   │  php.module.fn()       │
│ $js->eval(tsCode)           │◄─►│  host capabilities│◄─►│  frozen php.* facade   │
│ $js->grant($obj)            │   │  msgpack marshal  │   │  guest TS-as-JS        │
└─────────────────────────────┘   └───────────────────┘   └────────────────────────┘
        ext-php-rs (zval ↔ Rust)        rquickjs (Rust ↔ JSValue)
```

It is **one process, one thread**. The Rust extension is a `cdylib` that PHP
loads natively; `QuickJS`, `Js\Callback`, and the `QuickJS*Exception` classes are
real PHP classes implemented in Rust.

The capability trust boundary is a flat dispatch table reached through
`__host`. `quickjs.postMessage()` is a separate data-only output sink with a
bounded native queue.

## How one call flows, end to end

Take `php.math.add(2, 3)` from a guest script.

1. **`eval(tsCode)`** (`lib.rs` → `transpile.rs`) runs oxc: types are stripped,
   the target is esnext (a near-identity transform), and the output is JS plus a
   source map. The map is cached **host-side**, keyed by a content hash; it never
   enters the sandbox. QuickJS only ever sees JavaScript.

2. The guest calls `php.math.add(2, 3)`. `php` is not magic — it is a **frozen JS
   object tree** that `bridge.rs` generated from the registration manifest. The
   leaf `php.math.add` is a function:

   ```js
   php.math.add = function () {
     return globalThis.__rt.callHost("math.add", Array.prototype.slice.call(arguments));
   };
   ```

3. `__rt.callHost` (`src/js/runtime.js`) **msgpack-encodes** the argument array
   and calls `__host("math.add", bytes)`.

4. `__host` is the native entry point for registered PHP capabilities. In
   `bridge.rs` it:
   - decodes the msgpack payload to a `MiddleValue` list,
   - looks `"math.add"` up in the **dispatch table** (rejects if not registered —
     this is the trust boundary),
   - converts each arg `MiddleValue → zval`,
   - calls the PHP callable via `ZendCallable::try_call`.

5. The result travels back `zval → MiddleValue → msgpack bytes`, and `__rt`
   decodes it in the realm. `5` lands in the guest.

Adding a capability never changes this ABI: all registered capabilities use
one dispatch table. The flat, dotted-name list (`manifest()`) is the complete
audit surface for PHP callables.

### The facade is generated, and frozen

`bridge.rs::build_facade` walks the manifest's dotted names into a nested object
tree, makes each leaf a function calling `__rt.callHost("dotted.name", args)`, then
**deep-freezes** the whole tree. Freezing is a security requirement, not a
nicety: a guest must not be able to reassign `php.http.get` to fool other code.
The facade is (re)built at the start of every `eval` so newly registered
capabilities appear.

## Value marshaling

Each side implements exactly one conversion against a neutral middle type,
`marshal.rs::MiddleValue`, which (de)serializes to **native** msgpack (not
serde's tagged-enum form), so the in-sandbox JS codec interoperates byte-for-byte.

```
JS value  ──js_to_middle──►  MiddleValue  ──middle_to_zval──►  PHP zval
JS value  ◄─middle_to_js───  MiddleValue  ◄─zval_to_middle───  PHP zval
                                  │
                            msgpack bytes        (the __host wire form)
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
- Why msgpack at all, in one process? It gives a clean, binary-safe, documented
  ABI for the one `__host` import, and a single canonical serialization that both
  the Rust and JS sides share.

## Bidirectional functions

Functions can't be msgpack-encoded, so they cross as **tagged references** and
the real callable is held in a registry on the owning side.

- **`{"$__phpfn": id}`** — a PHP callable handed to JS. The callable is stored in
  a host-side registry (`bridge.rs`); JS receives a wrapper function that routes
  back through `__php_invoke(id, …)`.
- **`{"$__jsfn": id}`** — a JS function handed to PHP. The function is stored in a
  **JS-side** registry (`jsFns` in `runtime.js`); PHP receives a `Js\Callback`
  object holding the integer `id`.

For JS → PHP calls, `wrap()` replaces functions with refs before encoding and
`unwrap()` restores them after decoding. Saved JS callbacks use native value
conversion in Rust.

### Invoking a JS function from PHP

`Js\Callback::__invoke` (`callback.rs`) looks up the function with `__getJsFn`,
converts its arguments, invokes it, and awaits any returned Promise.

The subtlety is **re-entrancy**. Each engine records its own active context
while inside `Context::with`. A nested callback reuses that context instead of
acquiring the runtime lock again. A callback owned by another engine enters its
own context. The active pointer is cleared by a scope guard on return, including
errors. A re-entrancy depth cap (200) bounds recursive bridge calls.

At an outer entry, QuickJS's stack limit is refreshed for the current PHP Fiber.
The engine records that Fiber as its owner. Like php-tokio, a host callback may
suspend while its native Rust/QuickJS stack remains alive, but another Fiber may
not enter the same engine. External Promise callbacks are queued by Revolt and
executed after the owner resumes. The same entry guard arms and clears the
execution deadline for evals, callbacks, and job batches. See
[asynchronous execution](async.md).

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
