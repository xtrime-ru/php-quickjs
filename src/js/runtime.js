// Function references live on their owning side of the native bridge.
// Keep the JS registry across evals in a shared realm; Rust releases entries
// when their Js\Callback wrappers are collected.
if (!globalThis.__registerJsFn) {
  (function () {
    "use strict";
    var jsFns = {};
    var nextId = 1;

    function registerFn(fn) {
      var id = nextId++;
      jsFns[id] = fn;
      return id;
    }
    function getFn(id) { return jsFns[id]; }
    function deleteFn(id) { delete jsFns[id]; }
    function makePhpFn(id) {
      return function () {
        return globalThis.__php_invoke(id, Array.prototype.slice.call(arguments));
      };
    }

    globalThis.__registerJsFn = registerFn;
    globalThis.__getJsFn = getFn;
    globalThis.__makePhpFn = makePhpFn;
    globalThis.__deleteJsFn = deleteFn;
    globalThis.__asPromise = function (value) {
      return value !== null &&
        (typeof value === "object" || typeof value === "function")
        ? Promise.resolve(value) : null;
    };
    globalThis.__jsFnCount = function () { return Object.keys(jsFns).length; };
  })();
}
