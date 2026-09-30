<?php
require __DIR__ . '/_harness.php';

$js = new QuickJS(timeoutMs: 20);
$js->register('fail', fn() => throw new LogicException('host failure'));
$callback = $js->eval('() => php.fail()', false);
try {
    $callback();
    ok(false, 'callback PHP exception must escape');
} catch (LogicException $e) {
    eq('host failure', $e->getMessage(), 'callback restores the original PHP class and message');
}
eq(42, $js->eval('42', false), 'engine recovers after callback PHP exception');

$broken = $js->eval('() => { throw new RangeError("callback failure"); }', false);
throws(fn() => $broken(), QuickJSEvalException::class, 'callback JS error keeps its previous PHP type');
$runaway = $js->eval('() => { while (true) {} }', false);
throws(fn() => $runaway(), QuickJSTimeoutException::class, 'callback timeout keeps its specialized type');
eq(42, $js->eval('42', false), 'engine recovers after callback timeout');

$limited = new QuickJS(memoryLimit: 2 * 1024 * 1024);
$allocate = $limited->eval('() => { let data = []; while (true) data.push(new Array(100000).fill(0)); }', false);
throws(fn() => $allocate(), QuickJSEvalException::class, 'callback memory error retains its existing generic type');
unset($allocate);
eq(42, $limited->eval('42', false), 'engine recovers after callback memory error');

$job = new QuickJS(timeoutMs: 20);
$job->eval('Promise.resolve().then(() => { while (true) {} }); void 0', false);
throws(fn() => $job->executePendingJobs(), QuickJSTimeoutException::class, 'job timeout keeps its specialized type');
eq(42, $job->eval('42', false), 'engine recovers after job timeout');

$async = $js->eval('async () => { await 0; throw new TypeError("async callback failure"); }', false);
throws(fn() => $async(), QuickJSEvalException::class, 'async callback rejection is classified once');
eq(42, $js->eval('42', false), 'engine recovers after async callback error');

done();
