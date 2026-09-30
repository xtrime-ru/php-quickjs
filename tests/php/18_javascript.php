<?php
require __DIR__ . '/_harness.php';

foreach ([false, true] as $isolated) {
    $js = new QuickJS(isolated: $isolated, timeoutMs: 20);
    eq(42, $js->eval('const answer: number = 42; answer'), 'TypeScript remains the default');
    eq(42, $js->eval('41 + 1', typescript: false), 'native JavaScript result');
    eq(42, $js->eval('(async () => { await 0; return 42; })()', false), 'native JavaScript awaits Promises');
    $js->register('apply', fn($callback) => $callback(6));
    eq(42, $js->eval('php.apply(n => n * 7)', false), 'native JavaScript shares the PHP bridge');
    $js->eval('quickjs.postMessage({ answer: 42 }); void 0', false);
    eq([['answer' => 42]], $js->drainMessages(), 'native JavaScript shares the message queue');
    try {
        $js->eval("\n\nthrow new TypeError('native boom')", false);
        ok(false, 'native JavaScript error should throw');
    } catch (QuickJSEvalException $e) {
        eq('TypeError', $e->getJsName(), 'native JS exception retains its type');
        eq('guest.js', $e->getFile(), 'native JS error has the original filename');
        eq(3, $e->getLine(), 'native JS error keeps its original line');
        ok(str_contains($e->getJsStack(), 'guest.js:3:'), 'native JS stack retains its original coordinates');
    }
    throws(fn() => $js->eval('const n: number = 42;', false), QuickJSEvalException::class, 'native JS rejects TypeScript');
    throws(fn() => $js->eval('while (true) {}', false), QuickJSTimeoutException::class, 'native JS respects timeout');
    eq(42, $js->eval('42', false), 'native JS engine recovers after timeout');
    $limited = new QuickJS(isolated: $isolated, memoryLimit: 2 * 1024 * 1024);
    throws(fn() => $limited->eval('let data = []; while (true) data.push(new Array(100000).fill(0));', false), QuickJSMemoryException::class, 'native JS respects heap limit');
}

done();
