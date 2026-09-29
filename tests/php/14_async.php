<?php
require __DIR__ . '/_harness.php';
if (!is_file(__DIR__ . '/../../vendor/autoload.php')) {
    echo "SKIP: run composer install for Revolt/Amp integration tests\n";
    done();
}
require __DIR__ . '/../../vendor/autoload.php';

$js = new QuickJS(timeoutMs: 1000);
$js->register('later', static function ($resolve): void {
    Revolt\EventLoop::delay(0.001, static fn() => $resolve(42));
});
eq(42, $js->eval('new Promise(resolve => php.later(resolve))'), 'main flow awaits external resolution');
eq(42, Amp\async(fn() => $js->eval('new Promise(resolve => php.later(resolve))'))->await(), 'Amp Fiber awaits external resolution');
eq([], Revolt\EventLoop::getIdentifiers(), 'completion cancels the deadline timer');

$js->register('sleep', static function (): int { Amp\delay(0.001); return 42; });
eq(42, Amp\async(fn() => $js->eval('(async () => php.sleep())()'))->await(), 'registered PHP callbacks may await Amp I/O');

// An event-loop Fiber must receive the callback result, including async results.
foreach (['resolve => { resolve(42); return 7; }', 'async resolve => { await 0; resolve(42); return 7; }'] as $source) {
    $callback = $js->eval($source);
    $future = null;
    $js->register('later', static function ($resolve) use ($callback, &$future): void {
        $future = Amp\async(static fn() => $callback($resolve));
    });
    eq(42, $js->eval('new Promise(resolve => php.later(resolve))'), 'queued callback resolves the owning Promise');
    eq(7, $future->await(), 'queued callback returns its value to the calling Fiber');
}

$failing = $js->eval('() => { throw new TypeError("queued failure"); }');
$js->register('later', static function ($resolve) use ($failing, &$future): void {
    $future = Amp\async(static function () use ($failing, $resolve): void {
        try { $failing(); } finally { $resolve(42); }
    });
});
eq(42, $js->eval('new Promise(resolve => php.later(resolve))'), 'owner continues after a queued callback fails');
throws(fn() => $future->await(), QuickJSEvalException::class, 'queued callback delivers its exception to the calling Fiber');

$futures = [];
$js->register('all', static function ($callback) use (&$futures): void {
    for ($i = 0; $i < 10; ++$i) {
        $futures[] = Amp\async(static fn() => $callback($i));
    }
});
eq(45, $js->eval('new Promise(resolve => { let count = 0, sum = 0; php.all(n => { sum += n; if (++count === 10) resolve(sum); return n * 2; }); })'), 'multiple event-loop callbacks settle one Promise');
eq(range(0, 18, 2), array_map(static fn($future) => $future->await(), $futures), 'each queued caller receives its own result');

$first = $js->eval('() => new Promise(resolve => { globalThis.resolveFirst = resolve; })');
$second = $js->eval('resolveOwner => { resolveFirst(41); resolveOwner(7); return 42; }');
$js->register('startConcurrent', static function ($resolveOwner) use ($first, $second, &$futures): void {
    $futures = [
        Amp\async(static fn() => $first()),
        Amp\async(static fn() => $second($resolveOwner)),
    ];
});
eq(7, $js->eval('new Promise(resolve => php.startConcurrent(resolve))'), 'second callback can settle the first pending Promise');
eq([41, 42], array_map(static fn($future) => $future->await(), $futures), 'concurrent callback results resume independently');

$waiting = $js->eval('() => new Promise(resolve => { globalThis.finishWaiting = resolve; })');
$js->register('startPending', static function ($resolveOwner) use ($waiting, &$future): void {
    $future = Amp\async(static fn() => $waiting());
    Revolt\EventLoop::defer(static fn() => $resolveOwner(8));
});
eq(8, $js->eval('new Promise(resolve => php.startPending(resolve))'), 'owner may finish while a callback Promise is pending');
$js->eval('finishWaiting(43)');
$js->executePendingJobs();
eq(43, $future->await(), 'pending callback survives the owner entry');

$fair = new QuickJS(timeoutMs: 1000);
$stop = $fair->eval('() => { globalThis.stopped = true; }');
$fair->register('scheduleStop', static function () use ($stop): void {
    Revolt\EventLoop::delay(0.001, static fn() => $stop());
});
eq(1, $fair->eval('globalThis.stopped = false; php.scheduleStop(); new Promise(resolve => {
    function spin() { if (stopped) resolve(1); else Promise.resolve().then(spin); }
    spin();
})'), 'microtask chain yields to Revolt timers');

$limited = new QuickJS(timeoutMs: 20);
$start = hrtime(true);
throws(fn() => $limited->eval('new Promise(() => {})'), QuickJSTimeoutException::class, 'external wait obeys timeoutMs');
ok((hrtime(true) - $start) / 1e6 < 1000, 'deadline wakes a Promise without external events');
eq(3, $limited->eval('1 + 2'), 'engine recovers after an external wait timeout');
eq([], Revolt\EventLoop::getIdentifiers(), 'timeout leaves no timer behind');
throws(fn() => Amp\async(fn() => $limited->eval('new Promise(() => {})'))->await(), QuickJSTimeoutException::class, 'deadline wakes an Amp Fiber too');
eq(3, $limited->eval('1 + 2'), 'engine recovers after an Amp Fiber timeout');

// Delay the owner after a foreign callback is queued, letting its deadline expire.
$limited->register('cancel', static function ($callback) use (&$future): void {
    $future = Amp\async(static fn() => $callback());
    Revolt\EventLoop::queue(static fn() => usleep(40000));
});
throws(fn() => Amp\async(fn() => $limited->eval('new Promise(() => php.cancel(() => { globalThis.late = true; return 42; }))'))->await(), QuickJSTimeoutException::class, 'expired owner does not execute queued callbacks');
throws(fn() => $future->await(), Exception::class, 'canceled callback wakes its caller with an exception');
eq('undefined', $limited->eval('typeof late'), 'canceled callback has no side effects');
done();
