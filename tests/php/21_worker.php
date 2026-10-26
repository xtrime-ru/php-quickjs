<?php
require __DIR__ . '/_harness.php';
require __DIR__ . '/../../vendor/autoload.php';
use Revolt\EventLoop;

eq('worker', QuickJS::executionMode(), 'worker execution is the only runtime mode');
throws(fn() => new QuickJS(memoryLimit: 1), Throwable::class, 'failed initialization joins its worker and reports an error');
$js = new QuickJS(timeoutMs: 2000);
$ticks = 0;
$timer = EventLoop::repeat(.002, static function () use (&$ticks): void { ++$ticks; });
try {
    eq(42, $js->eval('const end = Date.now()+100; while(Date.now()<end){}; 42', false), 'CPU-bound JS returns its result');
    ok($ticks >= 10, 'PHP timers run during CPU-bound JavaScript');
} finally { EventLoop::cancel($timer); }
$js->register('echo', static fn($data) => $data);
eq("\xff\x00\xfe", $js->eval('php.echo(new Uint8Array([255,0,254]))', false), 'binary host callback stays typed');
$js->register('close', static function () use ($js): void { $js->close(); });
throws(fn() => $js->eval('php.close()', false), Throwable::class, 'close inside a host callback does not deadlock');
$js->close();
throws(fn() => $js->eval('42', false), Throwable::class, 'closed runtime rejects entries');
throws(fn() => $js->register('x', fn()=>1), Throwable::class, 'closed runtime rejects registration');
throws(fn() => $js->grant(new stdClass), Throwable::class, 'closed runtime rejects new handles');

$runtime = new QuickJS();
$object = new stdClass();
$weak = WeakReference::create($object);
$runtime->grant($object);
unset($object);
$runtime->close();
eq(null, $weak->get(), 'close releases PHP handles');

$running = new QuickJS();
$task = Amp\async(static fn() => $running->eval('while(true){}', false));
Amp\delay(.02);
$start = hrtime(true);
$running->close();
throws(fn() => $task->await(), Throwable::class, 'closing an executing runtime resumes its PHP caller');
ok((hrtime(true)-$start)/1e6 < 1000, 'close interrupts a CPU loop promptly');

$budget = new QuickJS(timeoutMs: 20);
$budget->register('resolveThenBlock', static function ($resolve): void {
    $resolve(42);
    usleep(40000);
});
throws(fn() => $budget->eval('new Promise(resolve => { Promise.resolve().then(() => php.resolveThenBlock(resolve)); })', false), QuickJSTimeoutException::class, 'resolving a Promise inside PHP does not disarm the active JS job deadline');
eq(42, $budget->eval('42', false), 'runtime recovers after a resolved-but-blocked job timeout');
$budget->close();

$independent = new QuickJS(timeoutMs: 100);
$waiting = $independent->eval('() => new Promise(() => {})', false);
$child = null;
$independent->register('start', static function ($resolve) use ($waiting, &$child): void {
    $child = Amp\async(static fn() => $waiting());
    EventLoop::defer(static fn() => $resolve(1));
});
eq(1, $independent->eval('new Promise(resolve => php.start(resolve))', false), 'owner can finish before a callback Promise');
Amp\delay(.07);
eq(42, $independent->eval('(() => {const end=Date.now()+50;while(Date.now()<end){};return 42})()', false), 'an unrelated pending callback does not shorten a new eval budget');
throws(fn() => $child->await(), QuickJSTimeoutException::class, 'pending callback still observes its own timeout');
$independent->close();

$reentrant = new QuickJS();
$callable = new class($reentrant) {
    public function __construct(private QuickJS $runtime) {}
    public function __invoke(): int { return 1; }
    public function __destruct() { $this->runtime->register('other', static fn() => 2); }
};
$reentrant->register('value', $callable);
unset($callable);
$reentrant->register('value', static fn() => 3);
eq(5, $reentrant->eval('php.value()+php.other()', false), 'replacing capabilities releases PHP objects outside registry borrows');
$resource = new class($reentrant) {
    public function __construct(private QuickJS $runtime) {}
    public function __destruct() { $this->runtime->grant('replacement'); }
};
$handle = $reentrant->grant($resource);
unset($resource);
$reentrant->revoke($handle);
eq('replacement', $reentrant->resolve($handle+1), 'revoking handles permits PHP destructor reentry');
$reentrant->close();

$marshal = new QuickJS();
$called = false;
$value = $marshal->roundtrip(['then' => static function ($resolve) use (&$called): void { $called = true; $resolve(42); }]);
eq(true, is_array($value), 'roundtrip preserves records with a callable then property');
eq(false, $called, 'roundtrip does not execute thenables');
$marshal->close();

for($i=0;$i<20;$i++) {
    $temporary = new QuickJS();
    $temporary->eval('42', false);
    $temporary->close();
}
eq([], EventLoop::getIdentifiers(), 'repeated create/close leaves no Revolt watchers');
if (function_exists('pcntl_fork')) {
    $parent = new QuickJS();
    $pid = pcntl_fork();
    if ($pid === 0) {
        foreach ([fn() => $parent->eval('1', false), fn() => $parent->drainMessages(), fn() => $parent->close()] as $operation) {
            try { $operation(); exit(2); } catch (Throwable $error) {
                if (!str_contains($error->getMessage(), 'fork')) exit(3);
            }
        }
        exit(0);
    }
    if ($pid < 0) throw new RuntimeException('fork failed');
    pcntl_waitpid($pid, $status);
    eq(0, pcntl_wexitstatus($status), 'inherited runtimes fail clearly and child teardown does not deadlock');
    eq(42, $parent->eval('42', false), 'parent worker survives child exit');
    $parent->close();
}
done();
