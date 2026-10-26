<?php
require __DIR__ . '/_harness.php';
require __DIR__ . '/../../vendor/autoload.php';

use Revolt\EventLoop;

$js = new QuickJS(timeoutMs: 1000);
$maps = 0;
$js->register('map', static function (array $values, callable $callback) use (&$maps): array {
    ++$maps;
    return array_map($callback, $values);
});
eq([2, 4, 6], $js->eval('php.map([1, 2, 3], n => n * 2)'), 'ordinary API supports synchronous nested callbacks');
eq([[2, 4], [4, 6]], $js->eval('php.map([1, 2], n => php.map([n, n+1], m => m*2))'), 'nested PHP calls retain RPC semantics');
eq(4, $maps, 'every PHP map is called exactly once');
$js->register('identity', static fn($value) => $value);
eq([['text' => 'hello', 'bytes' => "\0\xff", 'flag' => true, 'nil' => null]], $js->eval('php.map([1], () => php.identity({text:"hello", bytes:new Uint8Array([0,255]), flag:true, nil:null}))'), 'nested RPC retains generic typed marshaling');

$js->register('recover', static function (callable $callback): string {
    try { $callback(); } catch (Throwable $error) { return $error->getMessage(); }
    return 'missing error';
});
eq('nested failure', $js->eval('php.recover(() => { throw new Error("nested failure"); })'), 'JS exceptions cross the RPC callback boundary');
$js->register('throwPhp', static fn() => throw new LogicException('php failure'));
eq('LogicException:php failure', $js->eval('php.map([1], () => { try { php.throwPhp(); } catch(e) { return e.phpClass+":"+e.message; } })[0]'), 'nested PHP exceptions retain their class');
eq(42, $js->eval('42'), 'worker resumes after nested exceptions');

$js->register('apply', static fn(callable $callback) => $callback());
eq(42, $js->eval('php.apply(async () => { await 0; await 0; return 42; })'), 'nested Promise executes ready jobs');
$js->register('later', static function (callable $resolve): void {
    EventLoop::delay(.002, static fn() => $resolve(42));
});
eq(42, $js->eval('php.apply(() => new Promise(resolve => php.later(resolve)))'), 'nested Promise resumes through external I/O');

$js->register('sleep', static function (int $n): int { Amp\delay(.001); return $n * 2; });
eq([2,4,6], Amp\async(static fn() => $js->eval('php.map([1,2,3], n => php.sleep(n))'))->await(), 'a nested PHP callback may suspend its Fiber');

$saved = $js->eval('n => php.map([n], m => m+1)[0]');
$js->register('otherFiber', static function () use ($saved): int {
    $future = Amp\async(static fn() => $saved(41));
    return $future->await();
});
eq(42, $js->eval('php.otherFiber()'), 'another Fiber can enter the exclusively borrowed session');

$post = $js->eval('value => quickjs.postMessage(value)');
$consumer = Amp\async(static fn() => $js->awaitMessages());
Amp\delay(.001);
$js->register('postAndWait', static function () use ($post, $consumer): int {
    $post->post(42);
    return $consumer->await()[0];
});
eq(42, $js->eval('php.postAndWait()'), 'posted calls wake a separate consumer before the PHP host call returns');

$sync = $js->eval('n => n');
$push = $js->eval('n => quickjs.postMessage(n)');
$js->register('finishQueued', static fn() => $sync(42));
$busy = $js->eval('() => { const end=Date.now()+20; while(Date.now()<end){}; return php.finishQueued(); }');
$inFlight = Amp\async(static fn() => $busy());
Amp\delay(.001);
$push->post(7);
unset($push);
eq(42, $inFlight->await(), 'nested callbacks preserve earlier post admission');
eq([7], $js->awaitMessages(), 'queued posts run before their deferred function deletion');

$emitBackground = $js->eval('value => quickjs.postMessage(value)');
$backgroundConsumer = Amp\async(static fn() => $js->awaitMessages());
Amp\delay(.001);
$backgroundDone = false;
$js->register('backgroundHost', static function () use ($emitBackground, $backgroundConsumer, &$backgroundDone): int {
    $emitBackground->post(11);
    $result = $backgroundConsumer->await()[0];
    $backgroundDone = true;
    return $result;
});
$js->eval('Promise.resolve().then(() => { globalThis.backgroundResult = php.backgroundHost(); }); null');
eq([11], $backgroundConsumer->await(), 'background host calls can await an independent message consumer');
while (!$backgroundDone) Amp\delay(.001);
eq(11, $js->eval('backgroundResult'), 'background host replies resume their suspended JS job');

$close = new QuickJS();
$close->register('apply', static fn(callable $callback) => $callback());
$close->register('close', static fn() => $close->close());
throws(static fn() => $close->eval('php.apply(() => php.close())'), Throwable::class, 'close from a nested host call does not deadlock');
$close->close();

$queued = new QuickJS();
$called = false;
$queued->register('queued', static function () use (&$called): void { $called = true; });
$queuedCall = $queued->eval('() => php.queued()');
$queuedCall->post();
usleep(5000); // Let the worker enqueue its host request without running Revolt.
$start = hrtime(true);
$queued->close();
eq(false, $called, 'close discards an unconsumed host request');
ok((hrtime(true)-$start)/1e6 < 1000, 'close joins a worker parked on an unconsumed host request');

for ($i = 0; $i < 20; ++$i) {
    $abandoned = new QuickJS();
    $abandoned->register('noop', static fn() => null);
    $abandonedCall = $abandoned->eval('() => php.noop()');
    $abandonedCall->post();
    unset($abandonedCall, $abandoned);
}
ok(true, 'destruction joins workers racing with unconsumed host requests');

$doomed = new QuickJS();
$doomed->register('apply', static fn(callable $callback) => $callback());
$paused = false;
$doomed->register('pause', static function () use (&$paused): void {
    $paused = true;
    Fiber::suspend();
});
$dying = new Fiber(static fn() => $doomed->eval('php.apply(() => php.pause())'));
$dying->start();
Amp\delay(.01);
eq(true, $paused, 'nested host callback is suspended before Fiber destruction');
unset($dying);
gc_collect_cycles();
throws(static fn() => $doomed->eval('42'), Throwable::class, 'destroying a host Fiber closes the runtime without retaining its pending RPC');
$doomed->close();

$budget = new QuickJS(timeoutMs: 20);
$budget->register('apply', static fn(callable $callback) => $callback());
throws(static fn() => $budget->eval('php.apply(() => { while(true) {} })'), QuickJSTimeoutException::class, 'nested JS callbacks retain execution deadlines');
eq(42, $budget->eval('42'), 'runtime recovers after nested callback timeout');
throws(static fn() => $budget->eval('php.apply(() => new Promise(() => {}))'), QuickJSTimeoutException::class, 'nested external wait expires during a PHP host call');
eq(42, $budget->eval('42'), 'runtime recovers after nested Promise timeout');
$budget->close();
$js->close();
eq([], EventLoop::getIdentifiers(), 'nested RPC waits leave no watchers');
done();
