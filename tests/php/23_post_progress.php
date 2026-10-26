<?php
require __DIR__ . '/_harness.php';
require __DIR__ . '/../../vendor/autoload.php';

// More than one command quantum, with coalesced wakeups and no Promise jobs.
// A stopped producer cannot send another notification to unblock admission.
for ($trial = 0; $trial < 20; ++$trial) {
    $js = new QuickJS();
    $push = $js->eval('value => quickjs.postMessage(value)', false);
    for ($i = 0; $i < 512; ++$i) $push->post($i);
    $values = [];
    while (count($values) < 512) array_push($values, ...$js->awaitMessages());
    eq(range(0, 511), $values, "coalesced posts make progress and preserve FIFO ($trial)");
    $js->close();
}
done();
