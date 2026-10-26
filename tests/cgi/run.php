<?php
// Run with: php tests/cgi/run.php /path/to/libphp_quickjs.so [php-cgi]
$extension = realpath($argv[1] ?? '');
if (!$extension) throw new RuntimeException('Pass the compiled extension path');
$cgi = $argv[2] ?? PHP_BINDIR . '/php-cgi';
$pipes = [];
$process = proc_open([$cgi, '-n', '-d', 'extension=' . $extension], [0 => ['pipe', 'r'], 1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes, null,
    [...getenv(), 'REDIRECT_STATUS' => '1', 'REQUEST_METHOD' => 'GET', 'SCRIPT_FILENAME' => __DIR__ . '/notification.php']);
if (!is_resource($process)) throw new RuntimeException('Cannot start php-cgi');
fclose($pipes[0]);
$output = stream_get_contents($pipes[1]);
$errors = stream_get_contents($pipes[2]);
fclose($pipes[1]); fclose($pipes[2]);
$status = proc_close($process);
if ($status !== 0 || !str_contains($output, "CGI notifications PASS\n") || str_contains($output, 'Fatal error')) {
    throw new RuntimeException("CGI failed ($status): $output $errors");
}
echo "CGI notifications PASS\n";
