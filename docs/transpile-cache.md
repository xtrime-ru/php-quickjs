# Transpile cache

TypeScript evaluation caches Oxc output in a content-addressed LRU. Each engine
defaults to retaining at most 256 entries and 32 MiB of string bytes across their original
sources, generated JavaScript, and source maps. Cache hits reuse the generated
strings and update recency. Oldest entries are evicted until both limits hold.

An entry larger than the configured byte budget is transpiled and executed without caching. Replacing
an entry adjusts its byte accounting; the original source is checked on every
hash hit so a collision cannot return another guest's JavaScript.

The budget covers strings retained by the cache, not all Oxc memory, temporary
transpilation allocations, QuickJS heap, or output strings still held by an
active evaluation after cache eviction. It is independent of the message queue
limit. Constructor arguments `transpileCacheMaxBytes` (default 33554432) and
`transpileCacheMaxEntries` (default 256) configure these bounds per instance.
Both must be non-negative; `0` in either argument disables caching.
