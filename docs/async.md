# Worker execution, Promise jobs and PHP Fibers

Since 0.1.0, each `QuickJS` owns a native OS thread. The runtime, contexts,
JavaScript values, Promise jobs, TypeScript compiler and transpile cache stay on
that thread. `QuickJS::executionMode()` returns `worker`; there is no inline mode.
The PHP API remains synchronous in shape: `eval()` and `Js\Callback` await their
result, including returned Promises and thenables. Detached jobs remain queued
until `executePendingJobs()` or a returned Promise requires them.

## PHP scheduling

Autoload `revolt/event-loop` to suspend the calling PHP Fiber during JS execution.
A nonblocking Unix notification socket wakes Revolt for completed requests and
PHP capability calls. The watcher is removed when no PHP callers remain. Without
Revolt, simple synchronous calls use a blocking receive loop on the PHP thread;
external Promise waits fail with an explicit autoload error. Guest code still
runs on its worker in both cases.

JS calls such as `php.math.add(2, 3)` preserve their synchronous semantics. The
worker sends copied arguments and waits for PHP to dispatch the allowlisted
capability on the original PHP thread. A PHP callback can await Amp I/O. Its
Fiber yields, and other PHP Fibers and independent JS workers can progress.
CPU-bound PHP callbacks still block PHP and should be offloaded separately.

Nested PHP -> `Js\Callback` calls are serviced by the same worker while it awaits
its host reply. Promise results are tracked independently, so one pending
callback does not prevent another callback from resolving it. Nested calls share
the active wall-clock deadline and have a depth limit of 200. Reentrant `eval()`
and manual job pumping are rejected. A callback may complete on its worker while
PHP is blocked; this differs from the old PHP-thread execution order.

## Deadlines and shutdown

`timeoutMs` bounds an eval, callback or job batch, including host waits. `null`
disables it. QuickJS's interrupt hook checks the deadline on the worker. A
blocking PHP callback cannot itself be interrupted; its late reply is discarded
if the worker has already timed out. The runtime can recover after a timeout.

Call `$js->close()` to interrupt executing JS, reject pending calls, join the
worker and release registered PHP capabilities, handles and queued messages.
Close is idempotent; new entries and registrations fail afterwards. The last
owner also joins the worker when destroyed. A saved `Js\Callback` retains its
owning runtime until released or explicitly closed. PHP Future cancellation
alone does not call `close()`; consumers decide whether to abort the whole runtime.

Close live runtimes before `fork()`, then create new runtimes in the child. An inherited runtime cannot be used in a child.
Linux and macOS are supported; notification sockets use `php://fd`.

## Native messages

`quickjs.postMessage(data)` snapshots null, booleans, numbers, strings,
`Uint8Array`, arrays and records. Functions, symbols, cycles and nesting beyond
64 levels are rejected. The `quickjs` global and method are frozen. PHP retrieves
and clears messages with `$js->drainMessages()` without entering JS.

The shared native queue has an aggregate accounted-byte limit, 32 MiB by default,
configured through `maxQueuedMessageBytes`. Accounting includes containers and
per-message overhead. Conversion runs outside the queue mutex: JS getters may
call PHP, which may drain the queue. A rejected message leaves older messages
intact. Generic values returned from eval/callbacks have no separate byte cap.
Transport commands are bounded to 256 outstanding PHP operations; nested bridge
calls also obey the depth limit. These are admission and message budgets, not a
process RSS limit. Copies and the JS heap consume additional memory.

## Detached jobs

`hasPendingJobs()` reports ready Promise jobs, excluding unresolved I/O.
`executePendingJobs($maxJobs = 100)` executes at most that many ready jobs and
returns their count without waiting for external I/O. Both require shared mode.
The worker awaits only the returned Promise, leaving later detached jobs queued.
Isolated mode discards its context and runtime, including detached jobs and
unfinished nested callbacks, at the end of each eval.
