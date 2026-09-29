<?php
require __DIR__ . '/_harness.php';

$js = new QuickJS();
eq(false, $js->hasPendingJobs(), 'new runtime has no jobs');
eq(0, $js->executePendingJobs(), 'empty queue returns immediately');

eq(42, $js->eval('(async () => 42)()'), 'fulfilled async eval is awaited automatically');
eq(42, $js->eval('(async () => { await 0; return 42; })()'), 'async eval drains its own jobs');
$asyncDouble = $js->eval('async n => { await 0; return n * 2; }');
eq(42, $asyncDouble(21), 'async JS callback returns its fulfilled value to PHP');
throws(
    fn() => $js->eval('(async () => { await 0; throw new Error("async boom"); })()'),
    QuickJSEvalException::class,
    'async rejection becomes a PHP evaluation exception'
);
eq(42, $js->eval('({ then(resolve) { resolve(42); } })'), 'thenables are awaited');
eq(42, $js->eval('let chain = Promise.resolve(42); for (let i = 0; i < 100; i++) chain = chain.then(value => value); chain'), 'settled Promise at quantum boundary needs no event loop');
eq(42, $js->eval('({ get then() { if (this.read) throw new Error("then read twice"); this.read = true; return resolve => resolve(42); } })'), 'thenable getter is read once');
eq(42, (new QuickJS(isolated: true))->eval('(async () => { await 0; return 42; })()'), 'isolated eval awaits its Promise');
$stop = new QuickJS();
eq(42, $stop->eval('globalThis.detached = 0; Promise.resolve().then(() => { Promise.resolve().then(() => { detached = 1; }); return 42; })'), 'await stops when its Promise settles');
eq(0, $stop->eval('detached'), 'await leaves later detached jobs queued');
$stop->executePendingJobs();
eq(1, $stop->eval('detached'), 'detached jobs can still be drained explicitly');
throws(fn() => (new QuickJS())->eval('new Promise(() => {})'), Throwable::class, 'external wait without autoloaded Revolt fails clearly');

$js->eval('globalThis.answer = 0; Promise.resolve(21).then(n => { answer = n * 2; }); void 0;');
eq(0, $js->eval('answer'), 'eval does not implicitly drain jobs');
eq(true, $js->hasPendingJobs(), 'Promise reaction is pending');
eq(1, $js->executePendingJobs(1), 'one job executed');
eq(42, $js->eval('answer'), 'reaction executed');
eq(false, $js->hasPendingJobs(), 'queue drained');

$resolve = null;
$js->register('capture', function ($fn) use (&$resolve) { $resolve = $fn; });
$js->eval('globalThis.later = 0; new Promise(resolve => php.capture(resolve)).then(n => { later = n; }); void 0;');
eq(false, $js->hasPendingJobs(), 'unresolved Promise is not a ready job');
$resolve(17);
eq(true, $js->hasPendingJobs(), 'host resolution enqueues a reaction');
$js->executePendingJobs();
eq(17, $js->eval('later'), 'host-resolved Promise completes');

$js->eval('globalThis.failure = null; Promise.resolve().then(() => { throw new Error("expected"); }).catch(e => { failure = e.message; }); void 0;');
$js->executePendingJobs();
eq('expected', $js->eval('failure'), 'rejections retain JS catch semantics');

$js->eval('globalThis.jobs = 0; globalThis.keepGoing = true; function again() { jobs++; if (keepGoing) Promise.resolve().then(again); } Promise.resolve().then(again); void 0;');
eq(7, $js->executePendingJobs(7), 'self-scheduling queue is bounded');
eq(7, $js->eval('jobs'), 'job limit is exact');
eq(true, $js->hasPendingJobs(), 'remaining work stays queued');
$js->eval('keepGoing = false; void 0;');
$js->executePendingJobs();
eq(false, $js->hasPendingJobs(), 'queue can finish on a later turn');
throws(fn() => $js->executePendingJobs(0), Throwable::class, 'zero budget rejected');
throws(fn() => $js->executePendingJobs(-1), Throwable::class, 'negative budget rejected');

$js->register('nested', fn() => $js->executePendingJobs());
throws(fn() => $js->eval('php.nested()'), Throwable::class, 'reentrant draining rejected instead of deadlocking');
$js->register('apply', fn($fn) => $fn(6));
$js->eval('globalThis.nestedResult = 0; Promise.resolve().then(() => { nestedResult = php.apply(n => n * 7); }); void 0;');
$js->executePendingJobs();
eq(42, $js->eval('nestedResult'), 'jobs can synchronously call PHP and JS');

$isolated = new QuickJS(isolated: true);
throws(fn() => $isolated->hasPendingJobs(), Throwable::class, 'isolated jobs rejected');
throws(fn() => $isolated->executePendingJobs(), Throwable::class, 'isolated draining rejected');

$limited = new QuickJS(timeoutMs: 20);
$limited->eval('Promise.resolve().then(() => { while (true) {} }); void 0;');
throws(fn() => $limited->executePendingJobs(), QuickJSTimeoutException::class, 'single runaway job observes execution timeout');
eq(3, $limited->eval('1 + 2'), 'engine recovers after job timeout');
// Host calls cannot be interrupted, but the next job must respect the batch budget.
$batch = new QuickJS(timeoutMs: 20);
$slowCalls = 0;
$batch->register('slow', function () use (&$slowCalls) { ++$slowCalls; usleep(40000); });
$batch->eval('for (let i = 0; i < 3; i++) Promise.resolve().then(() => php.slow()); void 0;');
throws(fn() => $batch->executePendingJobs(), QuickJSTimeoutException::class, 'short jobs enforce wall time between host calls');
eq(1, $slowCalls, 'expired batch does not execute the next host call');
eq(true, $batch->hasPendingJobs(), 'timeout preserves jobs not yet executed');
eq(3, $batch->eval('1 + 2'), 'engine recovers after host-call batch timeout');

// Callback-only event loops must not require eval() to release callback entries.
$cleanup = new QuickJS();
[$make, $count] = $cleanup->eval('[() => () => 1, () => __jsFnCount()]');
for ($i = 0; $i < 20; ++$i) {
    $temporary = $make();
    unset($temporary);
}
eq(2, $count(), 'outer callback entries flush released callback references');
$observedCount = null;
$cleanup->register('observe', function ($n) use (&$observedCount) { $observedCount = $n; });
$cleanup->eval('Promise.resolve().then(() => php.observe(__jsFnCount())); void 0;');
$temporary = $make();
unset($temporary);
$cleanup->executePendingJobs();
eq(2, $observedCount, 'job batches flush released callback references before execution');
done();
