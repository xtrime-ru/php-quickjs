//! The TypeScript fast path. Guest source is transpiled to JavaScript with oxc
//! (types erased, esnext target — a near-identity transform) before it ever
//! reaches QuickJS, and the resulting source map is kept host-side so guest
//! errors can be remapped from generated-JS back to original-TS coordinates.
//!
//! Transpilation is content-addressed and cached: re-evaluating the same guest
//! is free, and the source map never enters the sandbox.

use lru::LruCache;
use oxc::codegen::CodegenReturn;
use oxc::diagnostics::Diagnostics;
use oxc::parser::ParseOptions;
use oxc::span::SourceType;
use oxc::transformer::TransformOptions;
use oxc::CompilerInterface;
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::path::Path;
use std::rc::Rc;

/// A transpiled guest module: the JS QuickJS will run, plus the source map
/// (as JSON) for remapping errors. `map` is `None` only if codegen emitted none.
#[derive(Clone)]
pub struct Transpiled {
    pub module_id: String,
    pub js: Rc<str>,
    pub map_json: Option<Rc<str>>,
}

/// A transpile (parse/transform) failure, located in the original TS source.
#[derive(Debug, Clone)]
pub struct TranspileError {
    pub message: String,
    /// 1-based line/column of the first diagnostic (0 if unknown).
    pub line: u32,
    pub col: u32,
}

/// Transpile TS -> JS. `module_id` names the module (used as the codegen source
/// path and the QuickJS filename, so stack frames are attributable). Returns a
/// located [`TranspileError`] on parse/transform failure.
pub fn transpile(
    source: &str,
    module_id: &str,
) -> Result<(String, Option<String>), TranspileError> {
    let options = TransformOptions::from_target("esnext").map_err(|e| TranspileError {
        message: format!("invalid transform target: {e}"),
        line: 0,
        col: 0,
    })?;
    let mut compiler = TsCompiler {
        options,
        code: None,
        map_json: None,
        errors: Vec::new(),
    };
    compiler.compile(source, SourceType::ts(), Path::new(module_id));

    if let Some((message, offset)) = compiler.errors.into_iter().next() {
        let (line, col) = offset.map_or((0, 0), |o| line_col(source, o));
        return Err(TranspileError { message, line, col });
    }
    let code = compiler.code.ok_or_else(|| TranspileError {
        message: "transpiler produced no output".to_owned(),
        line: 0,
        col: 0,
    })?;
    Ok((code, compiler.map_json))
}

/// Compute the 1-based (line, column) of a byte offset within `source`.
fn line_col(source: &str, offset: usize) -> (u32, u32) {
    let mut line = 1u32;
    let mut col = 1u32;
    for (i, ch) in source.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// An oxc compiler pipeline configured for type-stripping + source maps. The
/// trait runs parse -> semantic -> transform -> codegen; we capture the
/// generated code, the map, and any diagnostics.
struct TsCompiler {
    options: TransformOptions,
    code: Option<String>,
    map_json: Option<String>,
    /// First-seen-first diagnostics as `(message, byte offset of its span)`.
    errors: Vec<(String, Option<usize>)>,
}

impl CompilerInterface for TsCompiler {
    fn parse_options(&self) -> ParseOptions {
        ParseOptions::default()
    }

    fn transform_options(&self) -> Option<&TransformOptions> {
        Some(&self.options)
    }

    fn enable_sourcemap(&self) -> bool {
        true
    }

    fn handle_errors(&mut self, errors: Diagnostics) {
        for e in errors {
            // The first label's span gives the source location of the problem.
            let offset = (&e.labels).into_iter().next().map(|l| l.offset() as usize);
            self.errors.push((e.to_string(), offset));
        }
    }

    fn after_codegen(&mut self, ret: CodegenReturn<'_>) {
        self.code = Some(ret.code);
        self.map_json = ret.map.map(|m| m.to_json_string());
    }
}

// ---------------------------------------------------------------------------
// content-addressed cache
// ---------------------------------------------------------------------------

#[cfg(test)]
const CACHE_BYTE_BUDGET: usize = 32 * 1024 * 1024;

struct CachedModule {
    source: String,
    transpiled: Transpiled,
}

impl CachedModule {
    fn byte_len(&self) -> usize {
        self.source.len()
            + self.transpiled.js.len()
            + self.transpiled.map_json.as_ref().map_or(0, |map| map.len())
    }
}

struct CacheState {
    entries: LruCache<u64, CachedModule>,
    bytes: usize,
}

impl CacheState {
    fn insert(&mut self, key: u64, module: CachedModule, budget: usize) {
        let bytes = module.byte_len();
        // Large guests still execute, without evicting useful cached modules.
        if bytes > budget {
            return;
        }
        if let Some(previous) = self.entries.pop(&key) {
            self.bytes -= previous.byte_len();
        }
        while self.bytes + bytes > budget || self.entries.len() == self.entries.cap().get() {
            if let Some((_, oldest)) = self.entries.pop_lru() {
                self.bytes -= oldest.byte_len();
            } else {
                break;
            }
        }
        self.entries.put(key, module);
        self.bytes += bytes;
    }
}

/// A small LRU mapping `hash(source)` -> transpiled output. Single-threaded
/// (PHP NTS), so a `RefCell` is sufficient.
pub struct TranspileCache {
    inner: RefCell<CacheState>,
    byte_budget: usize,
}

impl TranspileCache {
    pub fn new(capacity: usize, byte_budget: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).unwrap();
        TranspileCache {
            byte_budget: if capacity == 0 { 0 } else { byte_budget },
            inner: RefCell::new(CacheState {
                entries: LruCache::new(cap),
                bytes: 0,
            }),
        }
    }

    /// Return the transpiled form of `source`, transpiling and caching on miss.
    /// The cache key is a content hash; the stored source is compared on a hit
    /// to rule out a hash collision returning the wrong JS.
    pub fn get_or_transpile(&self, source: &str) -> Result<Transpiled, TranspileError> {
        let key = hash(source);
        if let Some(hit) = self.inner.borrow_mut().entries.get(&key) {
            if hit.source == source {
                return Ok(hit.transpiled.clone());
            }
        }
        // A clean, stable label for stack frames (the hash is the cache key,
        // not user-facing). Single-source guests share one filename.
        let module_id = "guest.ts".to_owned();
        let (js, map_json) = transpile(source, &module_id)?;
        let transpiled = Transpiled {
            module_id,
            js: Rc::from(js.as_str()),
            map_json: map_json.map(|m| Rc::from(m.as_str())),
        };
        if self.byte_budget > 0 {
            self.inner.borrow_mut().insert(
                key,
                CachedModule {
                    source: source.to_owned(),
                    transpiled: transpiled.clone(),
                },
                self.byte_budget,
            );
        }
        Ok(transpiled)
    }
}

fn hash(source: &str) -> u64 {
    let mut h = DefaultHasher::new();
    source.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_types() {
        let (js, map) = transpile("const x: number = 41;\nconst y = x + 1;", "m.ts").unwrap();
        assert!(
            !js.contains(": number"),
            "type annotation not stripped: {js}"
        );
        assert!(js.contains("41"));
        assert!(map.is_some(), "source map should be emitted");
    }

    #[test]
    fn keeps_private_fields_native() {
        // esnext target must NOT downlevel #private to WeakMaps.
        let (js, _) = transpile("class C { #id = 1; get(){ return this.#id; } }", "m.ts").unwrap();
        assert!(js.contains("#id"), "private field downleveled: {js}");
    }

    #[test]
    fn syntax_error_is_located() {
        // The error should carry a message and a 1-based line/col.
        let err = transpile("const a = 1;\nconst = ;", "m.ts").unwrap_err();
        assert!(!err.message.is_empty());
        assert_eq!(err.line, 2, "error is on the second line");
        assert!(err.col > 0);
    }

    #[test]
    fn cache_hits_return_same_output() {
        let cache = TranspileCache::new(8, CACHE_BYTE_BUDGET);
        let a = cache.get_or_transpile("const a: number = 1; a;").unwrap();
        let b = cache.get_or_transpile("const a: number = 1; a;").unwrap();
        assert_eq!(a.js, b.js);
        assert!(Rc::ptr_eq(&a.js, &b.js));
        let state = cache.inner.borrow();
        assert_eq!(state.entries.len(), 1);
        assert_eq!(
            state.bytes,
            state
                .entries
                .peek(&hash("const a: number = 1; a;"))
                .unwrap()
                .byte_len()
        );
        assert_eq!(a.module_id, b.module_id);
    }

    #[test]
    fn configurable_limits_can_disable_or_bound_cache() {
        let source = "const answer: number = 42; answer;";
        for (entries, bytes) in [(0, CACHE_BYTE_BUDGET), (8, 0), (8, 1)] {
            let cache = TranspileCache::new(entries, bytes);
            let first = cache.get_or_transpile(source).unwrap();
            let second = cache.get_or_transpile(source).unwrap();
            assert_eq!(first.js, second.js);
            assert!(!Rc::ptr_eq(&first.js, &second.js));
            assert!(cache.inner.borrow().entries.is_empty());
            assert_eq!(cache.inner.borrow().bytes, 0);
        }
        let cache = TranspileCache::new(1, CACHE_BYTE_BUDGET);
        let first = cache.get_or_transpile(source).unwrap();
        cache
            .get_or_transpile("const other: number = 1; other;")
            .unwrap();
        let repeated = cache.get_or_transpile(source).unwrap();
        assert!(!Rc::ptr_eq(&first.js, &repeated.js));
        assert_eq!(cache.inner.borrow().entries.len(), 1);
    }

    fn module(source: &str, js: &str, map: Option<&str>) -> CachedModule {
        CachedModule {
            source: source.to_owned(),
            transpiled: Transpiled {
                module_id: "guest.ts".to_owned(),
                js: Rc::from(js),
                map_json: map.map(Rc::from),
            },
        }
    }

    fn state(capacity: usize) -> CacheState {
        CacheState {
            entries: LruCache::new(NonZeroUsize::new(capacity).unwrap()),
            bytes: 0,
        }
    }

    #[test]
    fn cache_accounts_source_js_and_map_bytes() {
        let mut cache = state(8);
        cache.insert(1, module("é", "abc", Some("map")), 32);
        assert_eq!(cache.bytes, 8);
        cache.insert(2, module("src", "js", None), 32);
        assert_eq!(cache.bytes, 13);
    }

    #[test]
    fn cache_evicts_lru_for_entry_and_byte_limits() {
        let mut cache = state(2);
        cache.insert(1, module("one", "js", None), 12);
        cache.insert(2, module("two", "js", None), 12);
        cache.entries.get(&1);
        cache.insert(3, module("three", "js", None), 12);
        assert!(cache.entries.peek(&2).is_none());
        assert!(cache.entries.peek(&1).is_some());
        assert_eq!(cache.bytes, 12);

        cache.insert(4, module("four", "js", None), 12);
        assert!(cache.entries.peek(&1).is_none());
        assert!(cache.entries.peek(&3).is_none());
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, 6);
    }

    #[test]
    fn cache_entry_limit_evicts_even_with_byte_budget_remaining() {
        let mut cache = state(2);
        cache.insert(1, module("one", "js", None), 100);
        cache.insert(2, module("two", "js", None), 100);
        cache.entries.get(&1);
        cache.insert(3, module("three", "js", None), 100);
        assert!(cache.entries.peek(&2).is_none());
        assert!(cache.entries.peek(&1).is_some());
        assert!(cache.entries.peek(&3).is_some());
        assert_eq!(cache.bytes, 12);
    }

    #[test]
    fn cache_replacements_and_oversized_entries_preserve_accounting() {
        let mut cache = state(8);
        cache.insert(1, module("one", "js", Some("map")), 10);
        cache.insert(1, module("two", "js", None), 10);
        assert_eq!(cache.bytes, 5);
        assert_eq!(cache.entries.len(), 1);
        cache.insert(1, module("too large", "js", None), 10);
        assert_eq!(cache.bytes, 5);
        assert_eq!(cache.entries.peek(&1).unwrap().source, "two");
        cache.insert(2, module("oversized", "js", None), 10);
        assert_eq!(cache.entries.len(), 1);
        cache.insert(2, module("small", "12345", None), 10);
        assert_eq!(cache.bytes, 10);
        assert!(cache.entries.peek(&1).is_none());
    }

    #[test]
    fn hash_collision_transpiles_and_replaces_wrong_entry() {
        let cache = TranspileCache::new(8, CACHE_BYTE_BUDGET);
        let source = "const correct: number = 42; correct;";
        cache.inner.borrow_mut().insert(
            hash(source),
            module("different source", "wrong JS", Some("wrong map")),
            CACHE_BYTE_BUDGET,
        );
        let output = cache.get_or_transpile(source).unwrap();
        assert!(output.js.contains("42"));
        assert!(!output.js.contains("wrong JS"));
        let state = cache.inner.borrow();
        assert_eq!(state.entries.len(), 1);
        let entry = state.entries.peek(&hash(source)).unwrap();
        assert_eq!(entry.source, source);
        assert_eq!(state.bytes, entry.byte_len());
    }
}
