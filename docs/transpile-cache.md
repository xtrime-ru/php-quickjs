# Transpile cache

TypeScript evaluation caches Oxc output in a content-addressed LRU. Each engine
retains at most 256 entries and 32 MiB of string bytes across their original
sources, generated JavaScript, and source maps. Cache hits reuse the generated
strings and update recency. Oldest entries are evicted until both limits hold.

An entry larger than 32 MiB is transpiled and executed without caching. Replacing
an entry adjusts its byte accounting; the original source is checked on every
hash hit so a collision cannot return another guest's JavaScript.

The budget covers strings retained by the cache, not all Oxc memory, temporary
transpilation allocations, QuickJS heap, or output strings still held by an
active evaluation after cache eviction. It is independent of the message queue
limit and requires no public configuration.
