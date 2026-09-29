<?php
require __DIR__ . '/_harness.php';

$js = new QuickJS(timeoutMs: 20);
throws(fn() => $js->eval('while (true) {}'), QuickJSTimeoutException::class, 'existing wall-clock timeout interrupts JS');
eq(3, $js->eval('1 + 2'), 'engine recovers after timeout');
eq(4, (new QuickJS())->eval('Promise.resolve(4)'), 'Promise result is awaited without a timeout');
done();
