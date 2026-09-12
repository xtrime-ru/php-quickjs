# Promise jobs and PHP Fibers

QuickJS provides Promises, but the host owns I/O and scheduling. The extension
exposes `hasPendingJobs()` and `executePendingJobs($maxJobs = 100)` so a PHP event loop can
advance JavaScript without blocking on network activity. No event loop library
is required by the extension.

```php
$js = new QuickJS(timeoutMs: 100);
$resolve = null;
$js->register('capture', function ($fn) use (&$resolve) { $resolve = $fn; });
$js->register('completed', function ($value) { echo $value, "\n"; });
$js->eval('new Promise(resolve => php.capture(resolve)).then(php.completed); void 0;');

// Later, after host I/O completes and outside an active JS call:
$resolve(42);
$js->executePendingJobs(); // prints 42
```

Keep the instance in shared mode (`isolated: false`, the default). `executePendingJobs()`
and `hasPendingJobs()` reject isolated mode, whose contexts do not survive their
eval boundary. A Promise waiting for I/O will not keep `hasPendingJobs()` true.

For an event loop, execute a bounded batch after delivering I/O results. If jobs
remain, schedule another batch on a later loop turn. Do not busy-wait on pending
Promises, and do not drain an unbounded self-scheduling queue before servicing
I/O. Resolve an application-level PHP Future from a registered completion
callback; PHP Futures are not converted to JS Promises automatically.

## Fiber boundaries

An instance or saved callback may be used sequentially from different PHP
Fibers. At each outer JS entry the extension refreshes QuickJS's native stack
limit. Nested callbacks reuse the owning engine's context and retain the outer
execution deadline. Saved callbacks and job batches obey `timeoutMs` too.

PHP must return from the extension before switching Fibers. Switching while JS
is active is rejected by Zend: a suspended Rust/QuickJS stack would keep live
borrows and a runtime lock. Register callbacks that enqueue work and return;
perform asynchronous I/O after control returns to PHP. This also applies to
starting another Fiber synchronously from a host callback.

Synchronous JS → PHP → JS callbacks remain supported, including callbacks owned
by a different QuickJS instance. Calling `eval()` or `executePendingJobs()` reentrantly on
the active instance is rejected; use a saved JS callback for synchronous reentry.

The execution deadline interrupts JavaScript, not blocking PHP/C code in a host
callback. An overrun throws when PHP returns to the extension. Keep host callbacks short. The extension remains single-threaded/NTS;
Fiber support does not add ZTS or parallel-thread support.

## Batched direct dispatch

`Js\Callback::dispatch(?array $args, int $maxJobs = 100)` invokes a saved
callback with a positional argument list, then executes at most `maxJobs` Promise
jobs. Passing `null` skips invocation and only advances queued jobs. Its result is
`['messages' => [[kind, payload], ...], 'jobs' => int, 'pending' => bool]`.

```php
$dispatch = $js->eval('(kind, payload) => __quickjsEmit(kind, payload)');
$batch = $dispatch->dispatch(['result', ['answer' => 42]]);
// $batch['messages'] === [['result', ['answer' => 42]]]
```

During a batch, guest code can call `__quickjsEmit(kind, payload)` to enqueue a
message without invoking PHP. The host processes the returned messages after
QuickJS returns, so asynchronous PHP handlers may suspend safely there. Emitting
outside `dispatch()` throws. Dispatch uses native value conversion without
a MessagePack encode/decode round trip; valid UTF-8 strings stay strings and
binary PHP strings become `Uint8Array` and round-trip byte-for-byte.

Message payloads accept null, booleans, numbers, strings, `Uint8Array`, arrays and
objects containing data; functions are rejected. PHP arguments likewise accept only
data, not Closure objects or saved JS callbacks; use call() for callable arguments.
Each output payload and the complete PHP argument list are limited to 64 nesting
levels and 16 MiB of accounted storage, including container overhead. Generic
eval(), call() and roundtrip() retain the depth limit but have no transport byte cap. A batch queue allows 4096 messages and 32 MiB of accounted storage.
These host-side caps are separate from QuickJS's `memoryLimit`. Cycles, oversized
values and queue overflow throw JS errors. PHP input nesting is also limited to
64 levels. Errors escaping a batch discard its partial messages; remaining
Promise jobs stay queued, so applications must decide whether to resume or
abandon that operation. Callback return values are ignored.

Timeouts are checked before and after each job and at native-call return, as well as by QuickJS's interrupt hook. A
blocking PHP callback cannot be interrupted, but no further job starts after
its batch deadline has elapsed. Dropped callback registry entries are reclaimed
at the next outer engine entry, including callback-only and job-only loops.

Failed generic value conversion rolls back callback registrations. Passing a saved
JS callback as an argument requires the same owning QuickJS instance; invoking a
foreign callback directly from PHP remains supported.
