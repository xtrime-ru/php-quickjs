<?php
require __DIR__ . '/_harness.php';

$js = new QuickJS(timeoutMs: 100);
$callback = $js->eval('(n) => n * 2');
$fiber = new Fiber(function () use ($js, $callback) {
    eq(3, $js->eval('1 + 2'), 'main-stack engine runs in a Fiber');
    eq(42, $callback(21), 'main-stack callback runs in a Fiber');
    $js->eval('globalThis.value = 0; Promise.resolve().then(() => { value = 17; }); void 0;');
    Fiber::suspend();
    $js->executePendingJobs();
    eq(17, $js->eval('value'), 'jobs run after Fiber resumption');
});
$fiber->start();
eq(true, $js->hasPendingJobs(), 'ready jobs visible from main stack');
$fiber->resume();
eq(6, $callback(3), 'callback returns to main stack');

$inside = null;
(new Fiber(function () use (&$inside) { $inside = new QuickJS(); $inside->eval('globalThis.createdInFiber = 23;'); }))->start();
eq(23, $inside->eval('createdInFiber'), 'engine outlives its creation Fiber');

$other = new QuickJS();
$otherCallback = $other->eval('(n) => n + 100');
$js->register('other', fn($n) => $otherCallback($n));
eq(107, $js->eval('php.other(7)'), 'cross-engine callback uses its own context');

$js->register('suspendValue', fn() => Fiber::suspend('inside JS'));
$suspending = new Fiber(function () use ($js) {
    eq(42, $js->eval('php.suspendValue(); 42'), 'host callback resumes on its owning Fiber');
});
eq('inside JS', $suspending->start(), 'host callback may suspend like php-tokio');
throws(fn() => $js->eval('1'), Throwable::class, 'another Fiber cannot enter a suspended engine');
$suspending->resume();
eq(true, $suspending->isTerminated(), 'owning Fiber finishes after resumption');
eq(3, $js->eval('1 + 2'), 'engine remains usable after Fiber resumption');

$js->register('apply', fn($fn) => $fn());
(new Fiber(function () use ($js) {
    eq(9, $js->eval('php.apply(() => 9)'), 'same-engine synchronous reentrancy still works');
}))->start();
$isolated = new QuickJS(isolated: true);
(new Fiber(function () use ($isolated) {
    eq(42, $isolated->eval('6 * 7'), 'isolated context can be created on another Fiber stack');
}))->start();
$js->register('reenterEval', fn() => $js->eval('1'));
throws(fn() => $js->eval('php.reenterEval()'), Throwable::class, 'reentrant eval is rejected instead of deadlocking');
eq(3, $js->eval('1 + 2'), 'engine recovers after rejected eval');

$loop = $js->eval('() => { while (true) {} }');
throws(fn() => $loop(), QuickJSTimeoutException::class, 'saved callbacks obey the execution timeout');
eq(3, $js->eval('1 + 2'), 'engine recovers after callback timeout');

$short = new QuickJS(timeoutMs: 20);
$short->register('slow', function () { usleep(50000); return 42; });
$slow = $short->eval('() => php.slow()');
throws(fn() => $slow(), QuickJSTimeoutException::class, 'blocking callback timeout detected on return');
eq(3, $short->eval('1+2'), 'engine recovers after blocking callback timeout');

done();
