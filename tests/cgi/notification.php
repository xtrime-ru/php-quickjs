<?php
require dirname(__DIR__, 2) . '/vendor/autoload.php';
$js = new QuickJS();
$js->register('echo', static fn($value) => $value);
for ($i = 0; $i < 10; ++$i) {
    if ($js->eval('php.echo(42)', false) !== 42) throw new RuntimeException('CGI result mismatch');
}
$batch = Amp\async(static fn() => $js->awaitMessages());
Amp\delay(.001);
$js->eval('Promise.resolve().then(() => quickjs.postMessage(42)); null', false);
if ($batch->await() !== [42]) throw new RuntimeException('CGI message pump mismatch');
$js->close();
// Request shutdown force-closes suspended Fibers. Its PHP exception must escape
// the native wait loop instead of retrying suspend forever.
$unfinished = new QuickJS();
Amp\async(static fn() => $unfinished->awaitMessages())->ignore();
$hostWait = new QuickJS();
$hostWait->register('wait', static fn() => Amp\delay(60));
Amp\async(static fn() => $hostWait->eval('php.wait()', false))->ignore();
Amp\delay(.001);
echo "CGI notifications PASS\n";
