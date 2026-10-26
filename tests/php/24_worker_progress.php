<?php
require __DIR__ . '/_harness.php';
require __DIR__ . '/../../vendor/autoload.php';

$js = new QuickJS(timeoutMs: 1000);
$kick = $js->eval('() => { Promise.resolve().then(() => quickjs.postMessage("worker progressed")); }', false);
$js->register('blockPhp', static function () use ($js, $kick): array {
    $kick->post();
    // No event-loop turns or JS entries after post(): only the independent
    // worker can execute the queued microtask while this PHP call is active.
    for ($i = 0; $i < 200; ++$i) {
        usleep(1000);
        $messages = $js->drainMessages();
        if ($messages !== []) return $messages;
    }
    return [];
});
eq(['worker progressed'], $js->eval('php.blockPhp()', false), 'worker executes JS jobs while its PHP host callback blocks');
eq(42, $js->eval('42', false), 'runtime remains usable after concurrent PHP and JS progress');
$js->close();
done();
