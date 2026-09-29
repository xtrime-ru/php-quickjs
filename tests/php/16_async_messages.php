<?php
require __DIR__ . '/_harness.php';

$js = new QuickJS();
$js->eval('globalThis.value = { nested: { x: 1 }, bytes: new Uint8Array([0, 255]) }; quickjs.postMessage(value); value.nested.x = 9; value.bytes[0] = 42');
$messages = $js->drainMessages();
eq(1, $messages[0]['nested']['x'], 'postMessage snapshots nested data');
eq("\0\xff", $messages[0]['bytes'], 'postMessage snapshots binary data');
eq([], $js->drainMessages(), 'drain clears the queue');

$js->eval('quickjs.postMessage(new Uint8Array(17 * 1024 * 1024))');
eq(17 * 1024 * 1024, strlen($js->drainMessages()[0]), 'message may exceed the old 16 MiB single-value cap');
throws(fn() => new QuickJS(maxQueuedMessageBytes: 0), Throwable::class, 'queue budget must be positive');

$js->eval('try { quickjs.postMessage(new Uint8Array(33554433)); } catch (error) { quickjs.postMessage(error.message); }');
$messages = $js->drainMessages();
ok(str_contains($messages[0], 'size limit'), 'oversized result rejects only that operation');

$js = new QuickJS(maxQueuedMessageBytes: 256);
$js->eval('quickjs.postMessage(1)');
throws(fn() => $js->eval('quickjs.postMessage(2)'), QuickJSEvalException::class, 'configured queue is bounded');
eq([1], $js->drainMessages(), 'queue survives rejection');
$js->eval('quickjs.postMessage(3)');
eq([3], $js->drainMessages(), 'queue recovers after drain');
done();
