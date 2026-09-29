<?php
require __DIR__ . '/_harness.php';

$js = new QuickJS(maxQueuedMessageBytes: 1024);
eq('0.0.3', phpversion('php_quickjs'), 'extension reports its build version');
$send = $js->eval('(value) => quickjs.postMessage(value)');
foreach (['', "nul\0tail", 'Привет 🌍', "\xff\xfe"] as $value) {
    $send($value);
    eq([$value], $js->drainMessages(), 'message preserves data and bytes');
}
eq([], $js->drainMessages(), 'drain clears the queue');
eq(true, $js->eval('Object.isFrozen(quickjs) && Object.getOwnPropertyDescriptor(globalThis, "quickjs").writable === false'), 'message API is immutable');
eq('undefined', $js->eval('typeof __quickjsEmitAsync'), 'beta emit API removed');
eq(false, method_exists($send, 'dispatch'), 'beta dispatch API removed');

$js->eval('quickjs.postMessage({value: 1})');
throws(fn() => $js->eval('quickjs.postMessage(() => 1)'), QuickJSEvalException::class, 'function rejected');
eq([['value' => 1]], $js->drainMessages(), 'earlier messages survive a failed emission');
throws(fn() => $js->eval('const cycle = {}; cycle.self = cycle; quickjs.postMessage(cycle)'), QuickJSEvalException::class, 'cycle rejected');
eq([], $js->drainMessages(), 'cycle did not enter queue');

$js->eval('quickjs.postMessage({get value() { quickjs.postMessage("nested"); return 2; }})');
eq(['nested', ['value' => 2]], $js->drainMessages(), 'getter can emit without borrowing queue');
$js->eval('quickjs.postMessage(new Uint8Array(600))');
throws(fn() => $js->eval('quickjs.postMessage(new Uint8Array(600))'), QuickJSEvalException::class, 'aggregate byte limit enforced');
eq(1, count($js->drainMessages()), 'overflow preserves queued result');
$js->eval('quickjs.postMessage(42)');
eq([42], $js->drainMessages(), 'queue recovers after drain');

done();
