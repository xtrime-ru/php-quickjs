# Async functions, Promise jobs and PHP Fibers

`eval()` and `Js\Callback` automatically await returned Promises and thenables.
Rejected Promises become PHP exceptions. External I/O yields through
`Revolt\EventLoop::getSuspension()`; autoload `revolt/event-loop` when guest code
waits on host I/O.

`timeoutMs` is an optional wall-clock deadline for a complete `eval()`, callback
or manual job batch, including Promise waits. With `null` it is disabled. PHP
consumers can impose operation deadlines with their own futures and cancellation.
Canceling a PHP future does not interrupt synchronous JS already running in the
PHP process.

## Native messages

JavaScript can send a data-only value to PHP:

```js
quickjs.postMessage({type: 'result', id: 7, value: 42});
```

`postMessage()` copies the value immediately. It accepts null, booleans, numbers,
strings, `Uint8Array`, arrays, and objects containing those types. Functions,
symbols and cycles are rejected. Nesting is limited to 64 levels. The `quickjs`
global and its method are frozen. PHP retrieves and clears the queue with
`$js->drainMessages()`; this does not enter JS or execute Promise jobs.

The queue has a configurable aggregate accounted-byte limit (32 MiB by default):

```php
$js = new QuickJS(maxQueuedMessageBytes: 32 * 1024 * 1024);
```

Accounting includes container and per-message overhead. A message that exceeds
the remaining budget throws in its sending operation. Earlier messages remain
queued, so a request handler can catch the error and report that request's
failure after draining space. There is no separate message-count or per-message
byte limit.

## Detached jobs

`hasPendingJobs()` reports ready Promise jobs; unresolved host I/O is not a ready
job. `executePendingJobs($maxJobs = 100)` executes at most that many ready jobs
and returns their count without waiting for I/O. Both methods require shared
mode. `eval()` and callbacks await a Promise they return; detached work can be
pumped explicitly.

## Fiber scheduling

Each active engine runs on one PHP Fiber. Callbacks arriving from another Fiber
while the owner awaits are queued. The owner starts each queued callback and
tracks its Promise independently; a pending callback does not prevent the owner
from handling another queued callback. Ready jobs run in bounded quanta of 100,
with control returned to Revolt between full quanta. Nested JS → PHP → JS calls
reuse the owner's context and wall-clock deadline. Reentrant `eval()` and manual
job pumping are rejected.

Blocking PHP and synchronous JS cannot be interrupted by PHP future cancellation.
If `timeoutMs` is set, the QuickJS interrupt hook can interrupt synchronous JS;
a blocking PHP callback is checked when it returns.
