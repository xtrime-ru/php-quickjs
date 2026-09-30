<?php

declare(strict_types=1);

require __DIR__ . '/_harness.php';

// Load the canonical declarations under a private namespace so Reflection can
// compare them with the native classes without redeclaring extension symbols.
$source = file_get_contents($argv[1] ?? __DIR__ . '/../../stubs/php_quickjs.stubs.php');
$source = str_replace(
    ['namespace {', 'namespace Js {'],
    ['namespace StubSignatures {', 'namespace StubSignatures\\Js {'],
    $source,
);
eval(substr($source, strlen('<?php')));

function signature(ReflectionMethod $method): array
{
    $parameters = [];
    foreach ($method->getParameters() as $parameter) {
        $parameters[] = [
            $parameter->getName(),
            (string) ($parameter->getType() ?? 'mixed'),
            $parameter->isPassedByReference(),
            $parameter->isVariadic(),
            $parameter->isOptional(),
            $parameter->isDefaultValueAvailable() ? $parameter->getDefaultValue() : null,
        ];
    }
    return [$method->isStatic(), $parameters, (string) ($method->getReturnType() ?? 'mixed')];
}

foreach (['QuickJS', 'Js\\Callback', 'QuickJSException', 'QuickJSEvalException', 'QuickJSTimeoutException', 'QuickJSMemoryException'] as $class) {
    $native = new ReflectionClass($class);
    $stub = new ReflectionClass('StubSignatures\\' . $class);
    $nativeMethods = [];
    $stubMethods = [];
    foreach ($native->getMethods(ReflectionMethod::IS_PUBLIC) as $method) {
        if ($method->getDeclaringClass()->getName() === $class) {
            $nativeMethods[] = $method->getName();
        }
    }
    foreach ($stub->getMethods(ReflectionMethod::IS_PUBLIC) as $method) {
        if ($method->getDeclaringClass()->getName() !== $stub->getName()) {
            continue;
        }
        $stubMethods[] = $method->getName();
        ok($native->hasMethod($method->getName()), "$class::{$method->getName()} exists");
        if ($native->hasMethod($method->getName())) {
            eq(signature($method), signature($native->getMethod($method->getName())),
                "$class::{$method->getName()} matches canonical stub");
        }
    }
    sort($nativeMethods);
    sort($stubMethods);
    eq($stubMethods, $nativeMethods, "$class public methods match canonical stub");
}

done();
