# php-quickjs — documentation

Implementation notes and internals for the php-quickjs extension. For the
user-facing API and quick start, see the [project README](../README.md).

- **[Installation](install.md)** — prebuilt binaries for self-hosted PHP, AWS
  Lambda (Bref), and macOS, plus building from source.
- **[API reference](api.md)** — the `QuickJS` class and every method.
- **[Architecture](architecture.md)** — the three worlds (PHP / Rust / QuickJS),
  the direct host bridge, how a call flows end to end, value marshaling, and
  bidirectional function passing.
- **[Execution modes](execution-modes.md)** — what a *realm* is, shared vs.
  isolated mode, the realm lifecycle, and how the JS-callback registry is kept
  and freed.
- **[Errors](errors.md)** — typed exception classes, JS↔PHP error bridging,
  TypeScript stack remapping, and the structured exception accessors.

Runnable examples live in [`examples/`](../examples):

- [`kitchen_sink.php`](../examples/kitchen_sink.php) — every feature in one script.
- [`modes.php`](../examples/modes.php) — shared vs. isolated, side by side.
- [`usage.php`](../examples/usage.php) — the minimal "hello world".

```sh
make build
php -d extension=$(pwd)/target/debug/libphp_quickjs.so examples/kitchen_sink.php
```

## Source map

| File | Responsibility |
|------|----------------|
| `src/lib.rs` | The `QuickJS` PHP class: `eval` / `register` / `grant` / `resolve` / `revoke` / `manifest` / `dts` / `roundtrip`. |
| `src/engine.rs` | Owns the QuickJS `Runtime`; realm lifecycle (shared vs isolated); deadline + re-entrancy state; the current-context stack. |
| `src/sandbox.rs` | In-engine resource containment: the memory limit, native stack size, and wall-clock deadline (interrupt handler). |
| `src/transpile.rs` | TypeScript → JavaScript via oxc, plus the content-hash transpile cache. |
| `src/bridge.rs` | The native host imports, dispatch table, frozen `php.*` facade, and registries. |
| `src/marshal.rs` | Native `JS value ↔ MiddleValue ↔ PHP zval` conversion. |
| `src/callback.rs` | `Js\Callback` — a JS function wrapped as an invocable PHP object. |
| `src/handles.rs` | The capability handle table (`int → live zval`). |
| `src/error.rs` | Error bridging both ways; JS-stack remapping to TS coordinates. |
| `src/exceptions.rs` | The typed `QuickJS*Exception` classes and rich exception construction. |
| `src/manifest.rs` | The registration manifest and `.d.ts` generation. |
| `src/js/runtime.js` | Host dispatch and the JS callback registry. |

## Stack

- **[`ext-php-rs`](https://github.com/davidcole1340/ext-php-rs)** — the Zend
  extension API; makes `QuickJS` a native PHP class. RAII deletes the manual
  `zval` refcounting bug class.
- **[`rquickjs`](https://github.com/DelSkayn/rquickjs)** — high-level bindings to
  QuickJS-NG (bundled — no system library). RAII deletes the manual `JSValue`
  refcounting bug class.
- **[`oxc`](https://github.com/oxc-project/oxc)** — the TypeScript transform and
  source maps, in-process.
- **`sourcemap`** — error remapping.

## Threading

PHP here is **NTS** (non-thread-safe) — one OS thread. The QuickJS `Runtime`,
`Context`, all `zval`s, and the bridge state live on that thread, so the
implementation uses `Rc`/`RefCell` rather than `Arc`/`Mutex`, and rquickjs's
`parallel` feature is deliberately off. Nothing crosses a thread boundary.
