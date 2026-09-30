//! Value marshaling between JS values and PHP zvals through [`MiddleValue`].

use crate::bridge::BridgeState;
use crate::callback::JsCallback;
use ext_php_rs::convert::IntoZval;
use ext_php_rs::types::{ArrayKey, ZendClassObject, ZendHashTable, Zval};
use rquickjs::{Array, Ctx, Function, Object, TypedArray, Value};
/// The neutral value that bridges JS and PHP.
#[derive(Debug, Clone)]
pub enum MiddleValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Array(Vec<MiddleValue>),
    /// Insertion-ordered string-keyed map (matches JS object + PHP assoc array).
    Map(Vec<(String, MiddleValue)>),
    /// A PHP callable handed to JS (id into the host-side registry).
    PhpFn(u64),
    /// A JS function handed to PHP (id into the JS-side registry).
    JsFn(u64),
}

/// Map an `f64` to an int when it is integral and fits an `i64`, else keep it
/// a float. QuickJS already stores small integral numbers as int32, so this
/// only ever promotes the larger integral doubles that JS cannot tag as int.
fn int_or_float(f: f64) -> MiddleValue {
    if f.is_finite() && f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
        MiddleValue::Int(f as i64)
    } else {
        MiddleValue::Float(f)
    }
}

// ---------------------------------------------------------------------------
// JS <-> MiddleValue
// ---------------------------------------------------------------------------

pub const MAX_VALUE_DEPTH: usize = 64;
pub const VALUE_OVERHEAD: usize = 64;

/// Messages have a byte budget; the generic API remains unbounded in bytes.
/// Both paths bound recursion before visiting a value.
#[derive(Default)]
struct ConversionBudget {
    limit: Option<usize>,
    used: usize,
}
impl ConversionBudget {
    fn with_limit(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            used: 0,
        }
    }
    fn remaining(&self) -> Option<usize> {
        self.limit.map(|limit| limit - self.used)
    }
    fn charge(&mut self, bytes: usize) -> Result<(), &'static str> {
        let used = self
            .used
            .checked_add(bytes)
            .ok_or("bridge value exceeds size limit")?;
        if self.limit.is_some_and(|limit| used > limit) {
            return Err("bridge value exceeds size limit");
        }
        self.used = used;
        Ok(())
    }
    fn node(&mut self, depth: usize) -> Result<(), &'static str> {
        if depth > MAX_VALUE_DEPTH {
            return Err("bridge value exceeds maximum depth (64)");
        }
        self.charge(VALUE_OVERHEAD)
    }
}

/// Failed conversion never hands the registered functions to PHP. Defer their
/// deletion just like dropped wrappers, without executing JS during unwinding.
pub fn js_to_middle<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    state: &BridgeState,
) -> rquickjs::Result<MiddleValue> {
    let mut conversion = JsConversion {
        ctx,
        budget: ConversionBudget::default(),
        functions: true,
        registered: Vec::new(),
    };
    let result = conversion.convert(value, 0);
    if result.is_err() {
        for id in conversion.registered {
            state.queue_fn_deletion(id);
        }
    }
    result
}

/// Return the accounted size together with data, without traversing it twice.
pub fn js_to_data<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    max_bytes: usize,
) -> rquickjs::Result<(MiddleValue, usize)> {
    let mut conversion = JsConversion {
        ctx,
        budget: ConversionBudget::with_limit(max_bytes),
        functions: false,
        registered: Vec::new(),
    };
    let data = conversion.convert(value, 0)?;
    Ok((data, conversion.budget.used))
}

struct JsConversion<'a, 'js> {
    ctx: &'a Ctx<'js>,
    budget: ConversionBudget,
    functions: bool,
    registered: Vec<u64>,
}
impl<'js> JsConversion<'_, 'js> {
    fn convert(&mut self, value: Value<'js>, depth: usize) -> rquickjs::Result<MiddleValue> {
        let ctx = self.ctx;
        self.budget
            .node(depth)
            .map_err(|e| rquickjs::Exception::throw_type(ctx, e))?;
        if value.is_null() || value.is_undefined() {
            return Ok(MiddleValue::Null);
        }
        if let Some(b) = value.as_bool() {
            return Ok(MiddleValue::Bool(b));
        }
        if value.is_int() {
            return Ok(MiddleValue::Int(value.as_int().unwrap() as i64));
        }
        if value.is_float() {
            return Ok(int_or_float(value.as_float().unwrap()));
        }
        if let Some(s) = value.as_string() {
            let text = s.to_string()?;
            self.budget
                .charge(text.len())
                .map_err(|e| rquickjs::Exception::throw_type(ctx, e))?;
            return Ok(MiddleValue::Str(text));
        }
        if value.is_function() {
            if !self.functions {
                return Err(rquickjs::Exception::throw_type(
                    ctx,
                    "direct messages cannot contain functions",
                ));
            }
            // Register the function JS-side; PHP receives an opaque id.
            let register: Function = ctx.globals().get("__registerJsFn")?;
            let id: f64 = register.call((value.clone(),))?;
            self.registered.push(id as u64);
            return Ok(MiddleValue::JsFn(id as u64));
        }
        // Uint8Array -> Bytes (checked before the generic object branch).
        if value.is_object() {
            if let Ok(ta) = TypedArray::<u8>::from_value(value.clone()) {
                if let Some(bytes) = ta.as_bytes() {
                    self.budget
                        .charge(bytes.len())
                        .map_err(|e| rquickjs::Exception::throw_type(ctx, e))?;
                    return Ok(MiddleValue::Bytes(bytes.to_vec()));
                }
            }
        }
        if value.is_array() {
            let arr = value.into_array().unwrap();
            if self
                .budget
                .remaining()
                .is_some_and(|remaining| arr.len() > remaining / VALUE_OVERHEAD)
            {
                return Err(rquickjs::Exception::throw_type(
                    ctx,
                    "bridge array exceeds size limit",
                ));
            }
            let mut out = Vec::with_capacity(arr.len());
            for i in 0..arr.len() {
                out.push(self.convert(arr.get(i)?, depth + 1)?);
            }
            return Ok(MiddleValue::Array(out));
        }
        if value.is_object() {
            let obj = value.into_object().unwrap();
            let mut out = Vec::new();
            for entry in obj.props::<String, Value>() {
                let (k, v) = entry?;
                self.budget
                    .charge(k.len())
                    .map_err(|e| rquickjs::Exception::throw_type(ctx, e))?;
                out.push((k, self.convert(v, depth + 1)?));
            }
            return Ok(MiddleValue::Map(out));
        }
        Err(rquickjs::Exception::throw_type(
            ctx,
            "unsupported JS value type",
        ))
    }
}

/// Convert the neutral representation into a JS value.
pub fn middle_to_js<'js>(ctx: &Ctx<'js>, value: &MiddleValue) -> rquickjs::Result<Value<'js>> {
    Ok(match value {
        MiddleValue::Null => Value::new_null(ctx.clone()),
        MiddleValue::Bool(b) => Value::new_bool(ctx.clone(), *b),
        MiddleValue::Int(i) => {
            if let Ok(i32v) = i32::try_from(*i) {
                Value::new_int(ctx.clone(), i32v)
            } else {
                // Beyond i32: represent as a JS number (exact up to 2^53).
                Value::new_float(ctx.clone(), *i as f64)
            }
        }
        MiddleValue::Float(f) => Value::new_float(ctx.clone(), *f),
        MiddleValue::Str(s) => rquickjs::String::from_str(ctx.clone(), s)?.into_value(),
        MiddleValue::Bytes(b) => TypedArray::new(ctx.clone(), b.clone())?.into_value(),
        MiddleValue::Array(items) => {
            let arr = Array::new(ctx.clone())?;
            for (i, it) in items.iter().enumerate() {
                arr.set(i, middle_to_js(ctx, it)?)?;
            }
            arr.into_value()
        }
        MiddleValue::Map(entries) => {
            let obj = Object::new(ctx.clone())?;
            for (k, v) in entries {
                obj.set(k.as_str(), middle_to_js(ctx, v)?)?;
            }
            obj.into_value()
        }
        // Reconstruct callables from the JS-side helpers.
        MiddleValue::JsFn(id) => {
            let get: Function = ctx.globals().get("__getJsFn")?;
            get.call((*id as f64,))?
        }
        MiddleValue::PhpFn(id) => {
            let make: Function = ctx.globals().get("__makePhpFn")?;
            make.call((*id as f64,))?
        }
    })
}

// ---------------------------------------------------------------------------
// PHP Zval <-> MiddleValue
// ---------------------------------------------------------------------------

/// Registrations are committed only once the complete input is valid.
pub fn zval_to_middle(zv: &Zval, state: &BridgeState) -> Result<MiddleValue, String> {
    let mut conversion = PhpConversion::new(state);
    let value = conversion.convert(zv, 0)?;
    conversion.registered.clear();
    Ok(value)
}

pub fn arguments_to_middle(
    args: &[&Zval],
    state: &BridgeState,
) -> Result<Vec<MiddleValue>, String> {
    let mut conversion = PhpConversion::new(state);
    let result = args
        .iter()
        .map(|arg| conversion.convert(arg, 0))
        .collect::<Result<Vec<_>, _>>()?;
    conversion.registered.clear();
    Ok(result)
}

struct PhpConversion<'a> {
    state: &'a BridgeState,
    budget: ConversionBudget,
    registered: Vec<u64>,
}
impl<'a> PhpConversion<'a> {
    fn new(state: &'a BridgeState) -> Self {
        Self {
            state,
            budget: ConversionBudget::default(),
            registered: Vec::new(),
        }
    }
    fn convert(&mut self, zv: &Zval, depth: usize) -> Result<MiddleValue, String> {
        self.budget.node(depth)?;
        if zv.is_null() {
            return Ok(MiddleValue::Null);
        }
        if zv.is_bool() {
            return Ok(MiddleValue::Bool(zv.bool().unwrap_or(false)));
        }
        if zv.is_long() {
            return Ok(MiddleValue::Int(zv.long().unwrap()));
        }
        if zv.is_double() {
            return Ok(MiddleValue::Float(zv.double().unwrap()));
        }
        if zv.is_string() {
            let bytes = zv.zend_str().map(|zs| zs.as_bytes()).unwrap_or(&[]);
            self.budget.charge(bytes.len())?;
            return Ok(match std::str::from_utf8(bytes) {
                Ok(s) => MiddleValue::Str(s.to_owned()),
                Err(_) => MiddleValue::Bytes(bytes.to_owned()),
            });
        }
        if let Some(array) = zv.array() {
            return self.array(array, depth);
        }
        if let Some(cb) = zv.extract::<&ZendClassObject<JsCallback>>() {
            let owner = self.state.engine().ok_or("engine no longer available")?;
            if !std::rc::Rc::ptr_eq(&owner, &cb.engine) {
                return Err("JS callback belongs to a different QuickJS instance".to_owned());
            }
            cb.check_realm()?;
            return Ok(MiddleValue::JsFn(cb.id));
        }
        if zv.is_callable() {
            let id = self.state.register_php_fn(zv);
            self.registered.push(id);
            return Ok(MiddleValue::PhpFn(id));
        }
        Err("unsupported PHP value type for marshaling".to_owned())
    }
    fn array(&mut self, ht: &ZendHashTable, depth: usize) -> Result<MiddleValue, String> {
        if self
            .budget
            .remaining()
            .is_some_and(|remaining| ht.len() > remaining / VALUE_OVERHEAD)
        {
            return Err("bridge array exceeds size limit".to_owned());
        }
        if ht.has_sequential_keys() {
            let mut out = Vec::with_capacity(ht.len());
            for (_, value) in ht.iter() {
                out.push(self.convert(value, depth + 1)?);
            }
            Ok(MiddleValue::Array(out))
        } else {
            let mut out = Vec::with_capacity(ht.len());
            for (key, value) in ht.iter() {
                let key = match key {
                    ArrayKey::Long(i) => i.to_string(),
                    ArrayKey::String(s) => s,
                    ArrayKey::Str(s) => s.to_owned(),
                    ArrayKey::ZendString(s) => s.try_into().unwrap_or_default(),
                };
                self.budget.charge(key.len())?;
                out.push((key, self.convert(value, depth + 1)?));
            }
            Ok(MiddleValue::Map(out))
        }
    }
}
impl Drop for PhpConversion<'_> {
    fn drop(&mut self) {
        self.state.release_php_fns(&self.registered);
    }
}

/// Convert the neutral representation into a PHP value.
pub fn middle_to_zval(value: &MiddleValue, state: &BridgeState) -> Result<Zval, String> {
    let mut zv = Zval::new();
    match value {
        MiddleValue::Null => zv.set_null(),
        MiddleValue::Bool(b) => zv.set_bool(*b),
        MiddleValue::Int(i) => zv.set_long(*i),
        MiddleValue::Float(f) => zv.set_double(*f),
        MiddleValue::Str(s) => zv
            .set_string(s, false)
            .map_err(|e| format!("string conversion failed: {e}"))?,
        MiddleValue::Bytes(b) => zv.set_binary(b.clone()),
        MiddleValue::Array(items) => {
            let mut ht = ZendHashTable::new();
            for it in items {
                ht.push(middle_to_zval(it, state)?)
                    .map_err(|e| format!("array push failed: {e}"))?;
            }
            zv.set_hashtable(ht);
        }
        MiddleValue::Map(entries) => {
            let mut ht = ZendHashTable::new();
            for (k, v) in entries {
                ht.insert(k.as_str(), middle_to_zval(v, state)?)
                    .map_err(|e| format!("map insert failed: {e}"))?;
            }
            zv.set_hashtable(ht);
        }
        // A PHP callable handed to JS and returned unchanged: original callable.
        MiddleValue::PhpFn(id) => {
            return state
                .get_php_fn(*id)
                .ok_or_else(|| format!("unknown PHP callable id {id}"));
        }
        // A JS function handed to PHP: an invocable Js\Callback object.
        MiddleValue::JsFn(id) => {
            let engine = state
                .engine()
                .ok_or("engine no longer available for JS callback")?;
            let cb = JsCallback::new(*id, engine);
            return ZendClassObject::new(cb)
                .into_zval(false)
                .map_err(|e| format!("failed to build Js\\Callback: {e}"));
        }
    }
    Ok(zv)
}
