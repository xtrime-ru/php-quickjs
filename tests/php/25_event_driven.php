<?php
require __DIR__ . '/_harness.php';
require __DIR__ . '/../../vendor/autoload.php';

// One notification can carry host calls for many suspended PHP Fibers.
// Waiting for all entries before resolving any exposes missed owner wakeups.
$js = new QuickJS();
$callbacks = [];
$ready = null;
$js->register('hold', static function ($resolve) use (&$callbacks, &$ready): void {
    $callbacks[] = $resolve;
    if (count($callbacks) === 16) $ready->complete();
});
$wait = $js->eval('() => new Promise(resolve => php.hold(resolve))', false);
try {
    for ($round = 0; $round < 30; ++$round) {
        $ready = new Amp\DeferredFuture();
        $tasks = [];
        for ($i = 0; $i < 16; ++$i) $tasks[] = Amp\async(static fn() => $wait());
        $ready->getFuture()->await(new Amp\TimeoutCancellation(2));
        foreach ($callbacks as $callback) $callback(42);
        $results = Amp\Future\await($tasks);
        ksort($results);
        eq(array_fill(0, 16, 42), $results, 'all concurrent host calls and Promise waiters progress');
        $callbacks = [];
    }
} finally {
    $js->close();
}
eq([], Revolt\EventLoop::getIdentifiers(), 'concurrent RPC cleanup leaves no watchers');
$shared = new QuickJS();
$entered = 0;
$ready = new Amp\DeferredFuture();
$shared->register('entered', static function () use (&$entered, $ready): void {
    if (++$entered === 16) $ready->complete();
});
$shared->eval('globalThis.finish = null; globalThis.sharedPromise = new Promise(resolve => finish = resolve); null', false);
$wait = $shared->eval('() => { php.entered(); return sharedPromise; }', false);
$finish = $shared->eval('() => finish(42)', false);
$tasks = [];
try {
    for ($i = 0; $i < 16; ++$i) $tasks[] = Amp\async(static fn() => $wait());
    $ready->getFuture()->await(new Amp\TimeoutCancellation(2));
    $finish();
    $results = Amp\Future\await($tasks);
    ksort($results);
    eq(array_fill(0, 16, 42), $results, 'one Promise can wake several independent requests');
} finally {
    $shared->close();
}
$turns = new QuickJS();
$dispatch = $turns->eval('globalThis.value = 0; start => { if (start) Promise.resolve().then(() => value = 42); else quickjs.postMessage(value); }', false);
$dispatch->post(true);
$dispatch->post(false);
eq([42], $turns->awaitMessages(), 'ready microtasks run before the next independent posted command');
$turns->close();
done();
