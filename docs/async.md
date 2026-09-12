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
callback. Keep host callbacks short. The extension remains single-threaded/NTS;
Fiber support does not add ZTS or parallel-thread support.
