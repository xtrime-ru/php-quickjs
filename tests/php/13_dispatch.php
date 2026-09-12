<?php
require __DIR__ . '/_harness.php';
$q = new QuickJS(timeoutMs: 100);
$send = $q->eval('(kind, value) => { __quickjsEmit(kind, value); }');
foreach (['', "nul\0tail", 'Привет 🌍', "\xff\xfe" . str_repeat('a', 65536)] as $value) {
    eq([['data', $value]], $send->dispatch(['data', $value])['messages'], 'direct payload preserves bytes');
}
eq(['messages' => [], 'jobs' => 0, 'pending' => false], $send->dispatch(null), 'empty drain');
throws(fn() => $send->dispatch([], 0), Throwable::class, 'invalid budget');
throws(fn() => $send->dispatch(['named' => 1]), Throwable::class, 'argument map rejected');
throws(fn() => $q->eval('__quickjsEmit("bad", 1)'), Throwable::class, 'emission outside batch rejected');
$fail = $q->eval('() => { __quickjsEmit("partial", 1); throw new Error("failure"); }');
throws(fn() => $fail->dispatch([]), QuickJSEvalException::class, 'dispatch errors surfaced');
eq([], $send->dispatch(null)['messages'], 'failed batch discards partial output');
$cycle = $q->eval('() => { const a = {}; a.self = a; __quickjsEmit("cycle", a); }');
throws(fn() => $cycle->dispatch([]), Throwable::class, 'cyclic output rejected');
$fun = $q->eval('() => __quickjsEmit("function", () => 1)');
throws(fn() => $fun->dispatch([]), Throwable::class, 'function output rejected');
$large = $q->eval('() => __quickjsEmit("large", new Uint8Array(16777217))');
throws(fn() => $large->dispatch([]), Throwable::class, 'oversized payload rejected');
$flood = $q->eval('() => { for (let i=0;i<4097;i++) __quickjsEmit("many", i); }');
throws(fn() => $flood->dispatch([]), Throwable::class, 'message queue bounded');
eq([], $send->dispatch(null)['messages'], 'queue recovers after limit');
$chain = $q->eval('() => { Promise.resolve().then(() => __quickjsEmit("job", 1)).then(() => __quickjsEmit("job", 2)); }');
$first = $chain->dispatch([], 1);
eq([['job', 1]], $first['messages'], 'first bounded job');
eq(true, $first['pending'], 'continuation pending');
eq([['job', 2]], $chain->dispatch(null, 1)['messages'], 'continuation drained');
(new Fiber(function () use ($send) { eq([['fiber', 42]], $send->dispatch(['fiber', 42])['messages'], 'batch on Fiber stack'); }))->start();
$q->register('reenter', fn() => $send->dispatch(null));
$nested = $q->eval('() => php.reenter()');
throws(fn() => $nested->dispatch([]), Throwable::class, 'reentrant dispatch rejected');
eq([['ok', 1]], $send->dispatch(['ok', 1])['messages'], 'reentrant failure recovers');
$byteFlood = $q->eval('() => { const a = new Uint8Array(12000000); for(let i=0;i<3;i++) __quickjsEmit("bytes", a); }');
throws(fn() => $byteFlood->dispatch([]), Throwable::class, 'queue byte limit enforced across messages');
$getter = $q->eval('() => __quickjsEmit("getter", {get value() { throw new Error("getter failed"); }})');
throws(fn() => $getter->dispatch([]), QuickJSEvalException::class, 'getter error is propagated');
$deep = 1;
for ($i = 0; $i < 66; $i++) { $deep = [$deep]; }
throws(fn() => $send->dispatch(['deep', $deep]), Throwable::class, 'PHP input depth bounded');
eq([['ok', 2]], $send->dispatch(['ok', 2])['messages'], 'conversion failures leave usable batch');
done();
