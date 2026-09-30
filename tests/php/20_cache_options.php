<?php
require __DIR__ . '/_harness.php';

foreach ([false, true] as $isolated) {
    foreach ([[], ['transpileCacheMaxBytes' => 0], ['transpileCacheMaxEntries' => 0],
        ['transpileCacheMaxBytes' => 1, 'transpileCacheMaxEntries' => 1]] as $options) {
        $js = new QuickJS(...['isolated' => $isolated, ...$options]);
        eq(42, $js->eval('const answer: number = 42; answer;'), 'TS runs with configured cache');
        eq(43, $js->eval('43', typescript: false), 'direct JS runs with configured cache');
    }
}
throws(fn() => new QuickJS(transpileCacheMaxBytes: -1), Exception::class, 'negative byte budget rejected');
throws(fn() => new QuickJS(transpileCacheMaxEntries: -1), Exception::class, 'negative entry limit rejected');
done();
