# API reference

The extension exposes a single `QuickJS` class. For the bigger picture see
[architecture](architecture.md); for realms and the callback lifecycle see
[execution modes](execution-modes.md).

### `new QuickJS(?int $memoryLimit = null, ?int $timeoutMs = null, ?int $maxStack = null, bool $isolated = false, ?int $maxQueuedMessageBytes = null)`

Limits default to unbounded; pass non-zero values to contain resource abuse.
`isolated: true` runs each `eval()` in a fresh realm (see
[execution modes](execution-modes.md)).
`maxQueuedMessageBytes` defaults to 32 MiB and must be positive. `timeoutMs`
bounds the complete call, including Promise waits; `null` disables it.

### `register(string $name, callable $fn, ?string $types = null): void`

Expose a PHP callable to JS under a flat, dotted name — it becomes
`php.<dotted.name>(...)` in the guest. `$types` is an optional TypeScript signature
surfaced by `dts()`. This flat registry is the PHP callback allowlist.

### `eval(string $code): mixed`

Run TypeScript or JavaScript, await a returned Promise, and marshal its result to PHP. Errors raise a
`QuickJSEvalException` located at the original TS line/column (see [errors](errors.md)).

### `grant(mixed $resource): int` / `resolve(int $h): mixed` / `revoke(int $h): bool`

Capability handles for live, stateful objects (DB connections, file handles). The
object stays host-side; JS only ever sees an opaque integer it can pass back to a
capability. The handle **is** the capability.

```php
$pdo = new PDO('sqlite:app.db');
$h   = $js->grant($pdo);
$js->register('db.query', fn(int $handle, string $sql) => $js->resolve($handle)->query($sql)->fetchAll());
```

### `manifest(): array` / `dts(): string`

The registration manifest and generated TypeScript `.d.ts` for the `php` and
`quickjs` globals. The `php` declaration comes from the registration manifest.

### `roundtrip(mixed $value): mixed`

Diagnostic helper: send a PHP value through the full marshaling pipeline
(PHP → MiddleValue → JS → MiddleValue → PHP) and return the result. Useful for
testing value fidelity across the boundary; not needed in normal use.


### `hasPendingJobs(): bool`

Whether a Promise continuation is ready to run. An unresolved Promise waiting
for host I/O is not a ready job. Available in shared mode only.

### `executePendingJobs(int $maxJobs = 100): int`

Execute up to `maxJobs` ready Promise jobs and return the number executed. The
budget must be positive. Returns immediately when the queue is empty; never
waits for external I/O. Jobs queued by a running job count towards the same
budget. The constructor's `timeoutMs` also applies to this call, including an
individual job that does not return; a timeout raises `QuickJSTimeoutException`.

Only shared mode supports job pumping. Calling `executePendingJobs()` from an active JS
call is rejected. `eval()` and `Js\Callback` automatically await a Promise they
return, including the jobs needed to settle it. They do not drain unrelated,
detached jobs when their own result is not a Promise.
Promise rejections retain JavaScript semantics: use `.catch()`/rejection handlers;
`executePendingJobs()` is not an unhandled-rejection reporting API.

See [asynchronous execution](async.md) for host event loop integration and PHP
Fiber boundaries.

### `drainMessages(): array`

Return and clear messages sent by JS through `quickjs.postMessage(value)`.
Values are copied at send time and contain data only. This method does not
enter JS or execute jobs. See [asynchronous execution](async.md) for limits.
