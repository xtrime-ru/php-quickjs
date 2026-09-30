# Execution modes

A `QuickJS` *instance* is the unit of isolation. The `isolated` constructor flag
chooses how much of that unit each `eval()` gets.

```php
$shared = new QuickJS();                  // default: one persistent world
$isolated = new QuickJS(isolated: true);  // each eval() is its own world
```

See [`examples/modes.php`](../examples/modes.php) for the two side by side.

## What a "realm" is

A QuickJS **`Context`** is a *realm*: its own `globalThis`, its own intrinsics
(`Array`, `JSON`, …), and its own top-level scope. The **`Runtime`** (heap, GC,
memory limit, interrupt handler, and Promise job queue) is separate. Shared mode
keeps one runtime per instance; isolated mode creates and drops a runtime for each
outer `eval()`. Nested callbacks reuse the active runtime.

The modes differ in **runtime and realm lifecycle**:

| | Shared (default) | Isolated (`isolated: true`) |
|---|---|---|
| Realms per instance | one, for the instance's life | a fresh one per `eval()`, discarded after |
| Guest globals across evals | persist | gone next eval |
| Top-level `const`/`let` re-declare | clashes (`SyntaxError`) | fine (fresh realm) |
| `globalThis` carries over | yes | no |
| Registered capabilities | work | work |
| Capability handles | work | work |
| Synchronous callbacks (within an eval) | work | work |
| JS callback stored in PHP, called later | works | **throws** (realm gone) |
| Memory limit | shared across evals | applied separately to each eval |
| Detached Promise jobs | remain queued | discarded when eval ends |

## Shared mode — a persistent session

One realm for the instance's whole life. Think **REPL / long-lived tenant**:
state accumulates, and a JS function handed to PHP stays callable for the
instance's lifetime.

```php
$js = new QuickJS();
$js->eval('var visits = 0; function visit(){ return ++visits; }');
$js->eval('visit(); visit(); visits;');   // => 2   (sees the earlier eval)

$handler = null;
$js->register('on', function ($cb) use (&$handler) { $handler = $cb; });
$js->eval('php.on((n) => n * 100)');
$js->eval('1 + 1');                        // a later, unrelated eval
$handler(2);                               // => 200 (the callback still works)
```

The cost is that this is *not* isolation between unrelated scripts: top-level
`const`/`let`/`function` collide if you re-run them, and a guest can leave data
on `globalThis` for the next eval to read.

## Isolated mode — a stateless runner

A fresh runtime and realm per `eval()`, discarded afterward. A returned Promise
is awaited; any remaining detached jobs are discarded without being executed. Think **independent script
runner**: every eval is hermetic.

```php
$js = new QuickJS(isolated: true);
$js->eval('var x = 1;');
$js->eval('typeof x;');          // => "undefined"  (different world)
$js->eval('const C = 1;');
$js->eval('const C = 2; C;');    // => 2  (no clash)
```

Everything host-side is unaffected — capabilities, handles, marshaling, and
*synchronous* callbacks all work — but a JS callback **cannot outlive the eval
that created it** (its realm is discarded). Invoking a stored one throws a clear
error rather than misbehaving:

```php
$held = null;
$js->register('keep', function ($cb) use (&$held) { $held = $cb; });
$js->eval('php.keep(() => 1)');
$held();   // Exception: "JS callback invoked outside its eval (isolated …)"
```

## Why exactly those differences (the mechanism)

The behavioral split comes down to **what lives in the realm vs. host-side**:

- The `php.*` facade, the capability dispatch table, and the handle table are
  rebuilt per eval or live host-side → **unaffected** by realm lifetime → both
  modes behave identically there.
- Guest globals and the **JS function registry** (`jsFns`, in `runtime.js`) live
  **in the realm** → shared mode keeps them, isolated mode drops them with the
  realm. Detached Promise jobs live in the runtime, which isolated mode also
  discards.

## The JS callback registry: persistence and cleanup

This is the part that took the most care to get right.

A JS function handed to PHP is stored in `jsFns` inside `runtime.js`, and PHP
holds only the integer id (in a `Js\Callback`). Two requirements pull against
each other:

1. **Persist across evals (shared mode).** The bridge is (re)installed every
   eval; `runtime.js` is therefore guarded (`if (!globalThis.__registerJsFn) …`) so a
   re-install does **not** recreate `jsFns`. The registry survives for the realm's
   life, so a stored callback keeps working after later evals.

2. **Don't leak.** A `jsFns` entry must be released when its PHP `Js\Callback` is
   garbage-collected. But deletion can't happen *eagerly* in `Drop`: a JS function
   can round-trip PHP→JS within a single host call (e.g. `php.identity(fn)`
   returns `fn` straight back to JS), which drops a transient wrapper while JS
   still needs the entry — deleting then would race reconstruction. So `Drop` only
   **queues** the id (touching no JS, no locks), and the queued ids are flushed at
   the **next eval boundary**, when no round-trip is in flight.

The net effect: stored callbacks work for as long as PHP holds them, and the
registry is reclaimed shortly after PHP lets go. In isolated mode the whole realm
(and its `jsFns`) is dropped per eval, so there is nothing to leak.

## Choosing a mode

- **Shared (default):** a session where guests build up state and register
  handlers you call back later. Caveat: not isolation between scripts; globals
  leak forward.
- **Isolated:** running many independent guest scripts, each hermetic (no leakage,
  no collisions, automatic per-eval cleanup). Caveat: don't stash a JS callback in
  PHP to fire after the eval — pass it and use it *within* the eval.
- **Strongest isolation:** a brand-new `QuickJS` per tenant. That gives a fresh
  host-side capability registrations and handles too. Isolated mode already
  gives each eval a separate heap and memory limit.

## JavaScript without transpilation

`eval(string $code, bool $typescript = true)` uses Oxc and source maps by
default. For JavaScript that is already ready to execute, pass
`$js->eval($source, typescript: false)` to skip Oxc and its transpile cache.
Both paths use the same bridge, Promise awaiting and resource limits, in both
execution modes. JavaScript errors retain their original `guest.js` coordinates;
TypeScript errors are remapped to `guest.ts`.
