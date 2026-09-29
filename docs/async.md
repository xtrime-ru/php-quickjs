# Async functions, Promise jobs and PHP Fibers

`eval()` and `Js\Callback` automatically await returned Promises and thenables:

```php
$double = $js->eval('async n => { await 0; return n * 2; }');
echo $double(21); // 42
echo $js->eval('(async () => 42)()'); // 42
```

Rejections become PHP exceptions. For external I/O, install and autoload
`revolt/event-loop`. The current flow yields through `EventLoop::getSuspension()`;
`timeoutMs` wakes it if the Promise does not settle in time. Like php-tokio,
registered PHP callbacks may await I/O while the native stack remains suspended.

## Detached low-level jobs

For detached work whose Promise is not returned to PHP, use `hasPendingJobs()`
and `executePendingJobs($maxJobs = 100)`. This manual mode needs no event loop.

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

Manual jobs require shared mode (`isolated: false`). `hasPendingJobs()` counts
ready jobs, not pending I/O. Run bounded batches after I/O; never busy-wait.
Automatic awaiting stops when the returned Promise settles, leaving later jobs
queued. PHP Future objects are not automatically converted to JS Promises.

## Fiber boundaries

Each active engine belongs to one PHP Fiber. While it awaits a Promise, saved
callbacks from other Revolt Fibers are queued for the owner; their callers wait
for the result or exception. Other concurrent entry is rejected. Sequential use
from different Fibers is supported; the extension remains single-threaded/NTS.

Nested JS → PHP → JS calls reuse the owner's context and deadline. Reentrant
`eval()` and job pumping are rejected; use a saved callback instead.
Blocking PHP/C callbacks cannot be interrupted: an overrun throws on return.

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
