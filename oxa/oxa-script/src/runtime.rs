//! A15: the QuickJS runtime wrapper (limits, eval, calls, value bridge). Owner: task A15. Signatures below are frozen; bodies are yours.
use crate::{CallCx, FnRef, HostFn, ModuleLoader};
use oxa_proto::{codes, OxaError, Value};
use rquickjs::function::{Args, Rest};
use rquickjs::{Ctx, Function, Object, Persistent};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Resource limits of one runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Hard heap cap in bytes.
    pub memory_bytes: usize,
    /// Wall-clock bound of one outermost call into the script; `None` = unbounded.
    pub deadline_ms: Option<u64>,
    /// Native stack cap in bytes.
    pub stack_bytes: usize,
}

impl Limits {
    /// Extensions: 256 MiB, 30 s per handler call, 1 MiB stack.
    pub const EXTENSION: Limits = Limits {
        memory_bytes: 256 << 20,
        deadline_ms: Some(30_000),
        stack_bytes: 1 << 20,
    };
    /// codemode scripts: 256 MiB (pi's documented limit), no deadline unless the options line
    /// sets `timeout_ms`, 1 MiB stack.
    pub const CODEMODE: Limits = Limits {
        memory_bytes: 256 << 20,
        deadline_ms: None,
        stack_bytes: 1 << 20,
    };
}

/// Stops the running call of a runtime from any thread. Cloneable, `Send + Sync`.
#[derive(Debug, Clone, Default)]
pub struct InterruptHandle(pub(crate) Arc<AtomicBool>);

impl InterruptHandle {
    /// Make the running (or, if idle, the next) outermost call fail with `script`
    /// ("interrupted"). The flag clears when that call returns.
    pub fn interrupt(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// An evaluated ES module. Valid until its runtime is dropped.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleRef {
    pub(crate) id: u64,
    /// The name it was evaluated under.
    pub name: String,
}

/// Where `console.*` output goes: `(level, line)` with level one of `log`, `info`, `warn`,
/// `error`, `debug`; arguments joined with one space, non-strings as JSON.
pub type ConsoleFn = Box<dyn FnMut(&str, &str)>;

/// One QuickJS runtime + context. `!Send`: create and use it on one thread (see
/// [`crate::RuntimeThread`]).
pub struct Runtime {
    pub(crate) limits: Limits,
    pub(crate) interrupt: InterruptHandle,
    /// Private state of the implementation (rquickjs runtime, context, tables). A15 may replace
    /// this field's type; nothing outside this crate can see it.
    pub(crate) inner: Option<Box<dyn std::any::Any>>,
    pub(crate) _not_send: PhantomData<*const ()>,
}

/// Process-wide id source: ids of functions and modules never repeat across runtimes, so a
/// stale reference from a dropped runtime can never resolve to another runtime's object.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Shared state of one runtime. Host closures hold `Weak<Inner>`, so there is no reference
/// cycle: dropping the `Runtime` frees everything.
pub(crate) struct Inner {
    // Persistent handles first: they must go before the QuickJS runtime they belong to.
    pub(crate) fns: RefCell<HashMap<u64, Persistent<rquickjs::Value<'static>>>>,
    pub(crate) modules: RefCell<HashMap<u64, Persistent<Object<'static>>>>,
    pub(crate) helpers: RefCell<Option<Persistent<Object<'static>>>>,
    pub(crate) loader: RefCell<Option<Box<dyn ModuleLoader>>>,
    pub(crate) console: RefCell<Option<ConsoleFn>>,
    /// The context of the JS call in progress (set only while inside `Context::with`).
    pub(crate) cur_ctx: RefCell<Option<Ctx<'static>>>,
    /// Name of the module whose code made the host call in progress.
    pub(crate) cur_module: RefCell<Option<String>>,
    /// The `Runtime` that owns this state, as of the current outermost call.
    pub(crate) rt_ptr: Cell<*mut Runtime>,
    pub(crate) deadline: Cell<Option<Instant>>,
    pub(crate) deadline_ms: Cell<u64>,
    pub(crate) timed_out: Cell<bool>,
    pub(crate) depth: Cell<u32>,
    pub(crate) interrupt: InterruptHandle,
    pub(crate) ctx: rquickjs::Context,
    pub(crate) rt: rquickjs::Runtime,
}

/// Lifetime-only change of a context handle. The handle is only used while the `with` call that
/// produced it is active (see [`Inner::enter`]), and it is cleared before that call returns.
fn erase_ctx<'js>(ctx: Ctx<'js>) -> Ctx<'static> {
    unsafe { std::mem::transmute::<Ctx<'js>, Ctx<'static>>(ctx) }
}

impl Inner {
    /// Interrupt check run by QuickJS every few thousand operations.
    fn check_interrupt(&self) -> bool {
        if self.interrupt.0.load(Ordering::SeqCst) {
            return true;
        }
        if let Some(t) = self.deadline.get() {
            if Instant::now() >= t {
                self.timed_out.set(true);
                return true;
            }
        }
        false
    }

    fn arm(&self, deadline_ms: Option<u64>) {
        self.deadline
            .set(deadline_ms.map(|m| Instant::now() + Duration::from_millis(m)));
        self.deadline_ms.set(deadline_ms.unwrap_or(0));
        self.timed_out.set(false);
    }

    fn disarm(&self) {
        self.deadline.set(None);
        self.interrupt.0.store(false, Ordering::SeqCst);
    }

    fn current_ctx(&self) -> Option<Ctx<'static>> {
        self.cur_ctx.borrow().clone()
    }
}

/// Run `f` with the runtime's context, as the outermost call (arming the deadline) or as a
/// nested call inside a call already in progress (re-entrant, same deadline).
pub(crate) fn enter<R, F>(rt: &mut Runtime, f: F) -> Result<R, OxaError>
where
    F: for<'js> FnOnce(&Rc<Inner>, Ctx<'js>) -> Result<R, OxaError>,
{
    let inner = state_rc(rt)?;
    inner.rt_ptr.set(rt as *mut Runtime);
    let depth = inner.depth.get();
    if depth == 0 {
        inner.arm(rt.limits.deadline_ms);
    }
    inner.depth.set(depth + 1);
    let result = match inner.current_ctx() {
        Some(ctx) => f(&inner, ctx),
        None => inner.ctx.with(|ctx| {
            *inner.cur_ctx.borrow_mut() = Some(erase_ctx(ctx.clone()));
            let r = f(&inner, ctx);
            *inner.cur_ctx.borrow_mut() = None;
            r
        }),
    };
    inner.depth.set(depth);
    if depth == 0 {
        inner.disarm();
    }
    result
}

fn state_rc(rt: &Runtime) -> Result<Rc<Inner>, OxaError> {
    state_ref(rt)
        .cloned()
        .ok_or_else(|| OxaError::new(codes::INTERNAL, "runtime state is missing"))
}

pub(crate) fn state_ref(rt: &Runtime) -> Option<&Rc<Inner>> {
    rt.inner.as_ref()?.downcast_ref::<Rc<Inner>>()
}

/// Map a rquickjs error raised inside `ctx` to an `OxaError`.
pub(crate) fn rq(ctx: &Ctx<'_>, e: rquickjs::Error, inner: &Inner) -> OxaError {
    match e {
        rquickjs::Error::Exception => exception_oxa(ctx.catch(), inner),
        rquickjs::Error::WouldBlock => {
            OxaError::new(codes::SCRIPT, "promise can never settle: nothing is pending")
        }
        other => OxaError::new(codes::SCRIPT, other.to_string()),
    }
}

fn exception_oxa(v: rquickjs::Value<'_>, inner: &Inner) -> OxaError {
    if inner.timed_out.get() {
        return OxaError::new(
            codes::SCRIPT,
            format!("script exceeded its deadline of {} ms", inner.deadline_ms.get()),
        );
    }
    if let Some(obj) = v.as_object() {
        let name = prop_string(obj, "name").unwrap_or_else(|| "Error".to_string());
        let message = prop_string(obj, "message").unwrap_or_default();
        // An error thrown by a host function (or rethrown by the script) keeps its stable code,
        // so the embedder reports e.g. `unsupported_in_oxa` rather than a generic `script`.
        let code = prop_string(obj, "code")
            .and_then(|c| known_code(&c))
            .unwrap_or(codes::SCRIPT);
        let mut e = OxaError::new(code, format!("{name}: {message}"));
        // An explicit `hint` (set on errors thrown from host functions and the loader) wins
        // over the stack trace.
        let hint = prop_string(obj, "hint").or_else(|| prop_string(obj, "stack"));
        if let Some(hint) = hint.filter(|s| !s.is_empty()) {
            e = e.with_hint(hint);
        }
        return e;
    }
    let text = v
        .as_string()
        .and_then(|s| s.to_string().ok())
        .unwrap_or_else(|| "thrown value".to_string());
    OxaError::new(codes::SCRIPT, format!("Error: {text}"))
}

/// The `codes` constant named `c`, for an error code read back from a script error.
fn known_code(c: &str) -> Option<&'static str> {
    [
        codes::ENGINE_DOWN,
        codes::ENGINE_HTTP,
        codes::CONTEXT_OVERFLOW,
        codes::STREAM_BROKEN,
        codes::ABORTED,
        codes::UNSUPPORTED_IN_OXA,
        codes::IO,
        codes::NOT_FOUND,
        codes::INVALID,
        codes::USAGE,
        codes::DENIED,
        codes::NO_SANDBOX,
        codes::GUARD_REFUSED,
        codes::INVARIANT,
        codes::UNSAFE_PATH,
        codes::BUSY,
        codes::ENGINE_BUSY,
        codes::SCRIPT,
        codes::MCP,
        codes::MCP_NEEDS_AUTH,
        codes::TIMEOUT,
        codes::UNTRUSTED,
        codes::INTERNAL,
    ]
    .into_iter()
    .find(|k| *k == c)
}

fn prop_string(obj: &Object<'_>, key: &str) -> Option<String> {
    let v: rquickjs::Value<'_> = obj.get(key).ok()?;
    v.as_string().and_then(|s| s.to_string().ok())
}

/// The JS error object for an `OxaError` (`message`, `code`, and `hint` when present).
pub(crate) fn oxa_error_value<'js>(ctx: &Ctx<'js>, e: &OxaError) -> rquickjs::Result<rquickjs::Value<'js>> {
    let exc = rquickjs::Exception::from_message(ctx.clone(), &e.message)?;
    let obj = exc.as_object();
    obj.set("code", e.code)?;
    if let Some(h) = &e.hint {
        obj.set("hint", h.as_str())?;
    }
    Ok(exc.into_value())
}

pub(crate) fn throw_oxa(ctx: &Ctx<'_>, e: &OxaError) -> rquickjs::Error {
    match oxa_error_value(ctx, e) {
        Ok(v) => ctx.throw(v),
        Err(err) => err,
    }
}

/// Resolve a promise (awaiting it by pumping jobs); other values pass through.
pub(crate) fn await_value<'js>(v: rquickjs::Value<'js>) -> rquickjs::Result<rquickjs::Value<'js>> {
    if v.is_promise() {
        if let Some(p) = v.into_promise() {
            return p.finish::<rquickjs::Value<'js>>();
        }
        return Err(rquickjs::Error::Exception);
    }
    Ok(v)
}

/// Call a JS helper defined at runtime creation (not visible to scripts).
fn helper<'js>(ctx: &Ctx<'js>, inner: &Inner, name: &str) -> Result<Function<'js>, OxaError> {
    let guard = inner.helpers.borrow();
    let persisted = guard
        .as_ref()
        .ok_or_else(|| OxaError::new(codes::INTERNAL, "helpers are missing"))?;
    let obj: Object<'js> = persisted.clone().restore(ctx).map_err(|e| rq(ctx, e, inner))?;
    let f: rquickjs::Value<'js> = obj.get(name).map_err(|e| rq(ctx, e, inner))?;
    f.into_function()
        .ok_or_else(|| OxaError::new(codes::INTERNAL, format!("helper {name} is missing")))
}

fn call_helper<'js>(
    ctx: &Ctx<'js>,
    inner: &Inner,
    name: &str,
    arg: rquickjs::Value<'js>,
) -> Result<rquickjs::Value<'js>, OxaError> {
    let f = helper(ctx, inner, name)?;
    f.call((arg,)).map_err(|e| rq(ctx, e, inner))
}

/// JS -> Rust value bridge (see the crate docs).
pub(crate) fn to_json<'js>(
    ctx: &Ctx<'js>,
    inner: &Inner,
    v: &rquickjs::Value<'js>,
    depth: usize,
) -> Result<serde_json::Value, OxaError> {
    use rquickjs::Type;
    use serde_json::Value as J;
    if depth > 512 {
        return Err(OxaError::new(
            codes::INVALID,
            "value nests too deeply or is cyclic",
        ));
    }
    Ok(match v.type_of() {
        Type::Bool => J::Bool(v.as_bool().unwrap_or(false)),
        Type::Int => J::from(v.as_int().unwrap_or(0)),
        Type::Float => match v.as_float().and_then(serde_json::Number::from_f64) {
            Some(n) => J::Number(n),
            None => J::Null,
        },
        Type::String => match v.as_string().map(|s| s.to_string()) {
            Some(Ok(s)) => J::String(s),
            _ => J::Null,
        },
        Type::Function | Type::Constructor => {
            let id = next_id();
            inner
                .fns
                .borrow_mut()
                .insert(id, Persistent::save(ctx, v.clone()));
            FnRef(id).to_value()
        }
        Type::Exception => {
            let mut m = serde_json::Map::new();
            if let Some(obj) = v.as_object() {
                m.insert("name".into(), prop_string(obj, "name").map_or(J::Null, J::String));
                m.insert(
                    "message".into(),
                    prop_string(obj, "message").map_or(J::Null, J::String),
                );
                m.insert(
                    "stack".into(),
                    prop_string(obj, "stack").map_or(J::Null, J::String),
                );
            }
            let mut out = serde_json::Map::new();
            out.insert("$error".into(), J::Object(m));
            J::Object(out)
        }
        Type::Array => {
            let arr = v
                .as_array()
                .ok_or_else(|| OxaError::new(codes::INTERNAL, "array expected"))?;
            let mut out = Vec::with_capacity(arr.len());
            for i in 0..arr.len() {
                let item: rquickjs::Value<'js> = arr.get(i).map_err(|e| rq(ctx, e, inner))?;
                out.push(to_json(ctx, inner, &item, depth + 1)?);
            }
            J::Array(out)
        }
        Type::Object | Type::Promise => {
            let is_bytes = call_helper(ctx, inner, "isBytes", v.clone())?;
            if is_bytes.as_bool() == Some(true) {
                let arr = call_helper(ctx, inner, "bytesOf", v.clone())?;
                let mut bytes = Vec::new();
                if let Some(a) = arr.as_array() {
                    for i in 0..a.len() {
                        let b: u32 = a.get(i).map_err(|e| rq(ctx, e, inner))?;
                        bytes.push(b as u8);
                    }
                }
                let mut out = serde_json::Map::new();
                out.insert("$bytes".into(), J::String(b64_encode(&bytes)));
                return Ok(J::Object(out));
            }
            let mut out = serde_json::Map::new();
            if let Some(obj) = v.as_object() {
                for item in obj.props::<String, rquickjs::Value<'js>>() {
                    let (k, val) = item.map_err(|e| rq(ctx, e, inner))?;
                    out.insert(k, to_json(ctx, inner, &val, depth + 1)?);
                }
            }
            J::Object(out)
        }
        _ => J::Null,
    })
}

/// Rust -> JS value bridge (see the crate docs).
pub(crate) fn from_json<'js>(
    ctx: &Ctx<'js>,
    inner: &Inner,
    v: &serde_json::Value,
) -> Result<rquickjs::Value<'js>, OxaError> {
    use serde_json::Value as J;
    let map = |e: rquickjs::Error| rq(ctx, e, inner);
    Ok(match v {
        J::Null => rquickjs::Value::new_null(ctx.clone()),
        J::Bool(b) => rquickjs::Value::new_bool(ctx.clone(), *b),
        J::Number(n) => match n.as_i64().and_then(|i| i32::try_from(i).ok()) {
            Some(i) => rquickjs::Value::new_int(ctx.clone(), i),
            None => rquickjs::Value::new_float(ctx.clone(), n.as_f64().unwrap_or(f64::NAN)),
        },
        J::String(s) => rquickjs::String::from_str(ctx.clone(), s)
            .map_err(map)?
            .into_value(),
        J::Array(items) => {
            let arr = rquickjs::Array::new(ctx.clone()).map_err(map)?;
            for (i, item) in items.iter().enumerate() {
                let val = from_json(ctx, inner, item)?;
                arr.set(i, val).map_err(map)?;
            }
            arr.into_value()
        }
        J::Object(m) => {
            if m.len() == 1 {
                if let Some(id) = m.get(FnRef::KEY).and_then(|x| x.as_u64()) {
                    let fns = inner.fns.borrow();
                    let p = fns
                        .get(&id)
                        .ok_or_else(|| OxaError::new(codes::SCRIPT, "stale function reference"))?;
                    return p.clone().restore(ctx).map_err(map);
                }
                if let Some(data) = m.get("$bytes").and_then(|x| x.as_str()) {
                    let bytes =
                        b64_decode(data).ok_or_else(|| OxaError::new(codes::INVALID, "malformed $bytes"))?;
                    let arr = rquickjs::Array::new(ctx.clone()).map_err(map)?;
                    for (i, b) in bytes.iter().enumerate() {
                        arr.set(i, *b as i32).map_err(map)?;
                    }
                    return call_helper(ctx, inner, "mkBytes", arr.into_value());
                }
            }
            if m.len() == 1 {
                if let Some(err) = m.get("$error").and_then(|x| x.as_object()) {
                    let msg = err
                        .get("message")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let exc = rquickjs::Exception::from_message(ctx.clone(), &msg).map_err(map)?;
                    let obj = exc.as_object();
                    if let Some(name) = err.get("name").and_then(|x| x.as_str()) {
                        obj.set("name", name).map_err(map)?;
                    }
                    if let Some(stack) = err.get("stack").and_then(|x| x.as_str()) {
                        obj.set("stack", stack).map_err(map)?;
                    }
                    return Ok(exc.into_value());
                }
            }
            let obj = Object::new(ctx.clone()).map_err(map)?;
            for (k, item) in m {
                let val = from_json(ctx, inner, item)?;
                obj.set(k.as_str(), val).map_err(map)?;
            }
            obj.into_value()
        }
    })
}

/// Invoke a JS-visible host function: args go to the Rust side, the result comes back.
fn invoke<'js>(
    c: &Ctx<'js>,
    inner: &Rc<Inner>,
    f: &HostFn,
    args: Vec<rquickjs::Value<'js>>,
) -> Result<rquickjs::Value<'js>, OxaError> {
    let mut json_args = Vec::with_capacity(args.len());
    for a in &args {
        json_args.push(to_json(c, inner, a, 0)?);
    }
    let rt_ptr = inner.rt_ptr.get();
    if rt_ptr.is_null() {
        return Err(OxaError::new(
            codes::INTERNAL,
            "host function called outside a runtime call",
        ));
    }
    let module = c.script_or_module_name(0).and_then(|a| a.to_string().ok());
    let prev = inner.cur_module.replace(module);
    let up = crate::host::current_link();
    let out = {
        // SAFETY: `rt_ptr` was set by `enter` from the `&mut Runtime` that is running this JS
        // call. The Runtime is suspended in that call, so nothing else touches it meanwhile; the
        // state reached through `inner` is interior-mutable and never written through `rt`.
        let rt: &mut Runtime = unsafe { &mut *rt_ptr };
        let mut cx = CallCx { rt, up };
        // A1/A2: contained (quietly) on the script thread; the script sees an exception.
        oxa_proto::panic::contain(|| f(&mut cx, &json_args)).unwrap_or_else(|error| {
            Err(OxaError::new(
                codes::INTERNAL,
                format!("host function panicked: {error}"),
            ))
        })
    };
    *inner.cur_module.borrow_mut() = prev;
    let v = out?;
    from_json(c, inner, &v)
}

fn host_function<'js>(ctx: Ctx<'js>, weak: Weak<Inner>, f: Rc<HostFn>) -> rquickjs::Result<Function<'js>> {
    Function::new(
        ctx,
        move |c: Ctx<'js>, args: Rest<rquickjs::Value<'js>>| match weak.upgrade() {
            Some(inner) => match invoke(&c, &inner, &f, args.0) {
                Ok(v) => Ok(v),
                Err(e) => Err(throw_oxa(&c, &e)),
            },
            None => Err(throw_oxa(
                &c,
                &OxaError::new(codes::SCRIPT, "runtime was dropped"),
            )),
        },
    )
}

fn host_async_function<'js>(
    ctx: Ctx<'js>,
    weak: Weak<Inner>,
    f: Rc<HostFn>,
) -> rquickjs::Result<Function<'js>> {
    Function::new(ctx, move |c: Ctx<'js>, args: Rest<rquickjs::Value<'js>>| {
        let (promise, resolve, reject) = c.promise()?;
        let outcome = match weak.upgrade() {
            Some(inner) => invoke(&c, &inner, &f, args.0),
            None => Err(OxaError::new(codes::SCRIPT, "runtime was dropped")),
        };
        match outcome {
            Ok(v) => resolve.call::<_, ()>((v,))?,
            Err(e) => {
                let err = oxa_error_value(&c, &e)?;
                reject.call::<_, ()>((err,))?;
            }
        }
        Ok::<_, rquickjs::Error>(promise.into_value())
    })
}

fn console_fn<'js>(ctx: Ctx<'js>, weak: Weak<Inner>, level: &'static str) -> rquickjs::Result<Function<'js>> {
    Function::new(ctx, move |c: Ctx<'js>, args: Rest<rquickjs::Value<'js>>| {
        if let Some(inner) = weak.upgrade() {
            let parts: Vec<String> = args.0.iter().map(|v| fmt_arg(&c, &inner, v)).collect();
            let line = parts.join(" ");
            let sink = inner.console.borrow_mut().take();
            if let Some(mut s) = sink {
                s(level, &line);
                *inner.console.borrow_mut() = Some(s);
            }
        }
        Ok::<(), rquickjs::Error>(())
    })
}

fn fmt_arg<'js>(ctx: &Ctx<'js>, inner: &Inner, v: &rquickjs::Value<'js>) -> String {
    if let Some(s) = v.as_string() {
        return s.to_string().unwrap_or_default();
    }
    match to_json(ctx, inner, v, 0) {
        Ok(j) => j.to_string(),
        Err(_) => "[unrepresentable]".to_string(),
    }
}

/// Set `path` (dotted) to `value`, creating intermediate plain objects.
pub(crate) fn set_path<'js>(ctx: &Ctx<'js>, path: &str, value: rquickjs::Value<'js>) -> rquickjs::Result<()> {
    let parts: Vec<&str> = path.split('.').collect();
    let Some((last, mids)) = parts.split_last() else {
        return Err(rquickjs::Error::Unknown);
    };
    let mut parent: Object<'js> = ctx.globals();
    for seg in mids {
        let child: rquickjs::Value<'js> = parent.get(*seg)?;
        parent = match child.as_object() {
            Some(o) => o.clone(),
            None => {
                let o = Object::new(ctx.clone())?;
                parent.set(*seg, o.clone())?;
                o
            }
        };
    }
    parent.set(*last, value)
}

fn get_module<'js>(ctx: &Ctx<'js>, inner: &Inner, m: &ModuleRef) -> Result<Object<'js>, OxaError> {
    let modules = inner.modules.borrow();
    let p = modules
        .get(&m.id)
        .ok_or_else(|| OxaError::new(codes::SCRIPT, "stale module reference"))?;
    p.clone().restore(ctx).map_err(|e| rq(ctx, e, inner))
}

/// Call a script function and await its result; the result is bridged back to Rust.
fn call_js<'js>(
    ctx: &Ctx<'js>,
    inner: &Inner,
    func: Function<'js>,
    args: &[serde_json::Value],
) -> Result<serde_json::Value, OxaError> {
    let mut a = Args::new(ctx.clone(), args.len());
    for arg in args {
        let v = from_json(ctx, inner, arg)?;
        a.push_arg(v).map_err(|e| rq(ctx, e, inner))?;
    }
    let out: rquickjs::Value<'js> = func.call_arg(a).map_err(|e| rq(ctx, e, inner))?;
    let out = await_value(out).map_err(|e| rq(ctx, e, inner))?;
    to_json(ctx, inner, &out, 0)
}

/// Call a function captured earlier (`FnRef`) from inside a JS call that is in progress.
pub(crate) fn call_ref(
    inner: &Inner,
    f: &FnRef,
    args: &[serde_json::Value],
) -> Result<serde_json::Value, OxaError> {
    let ctx = inner
        .current_ctx()
        .ok_or_else(|| OxaError::new(codes::SCRIPT, "no script call is in progress on this runtime"))?;
    let func = {
        let fns = inner.fns.borrow();
        let p = fns
            .get(&f.0)
            .ok_or_else(|| OxaError::new(codes::SCRIPT, "stale function reference"))?;
        let v: rquickjs::Value<'static> = p.clone().restore(&ctx).map_err(|e| rq(&ctx, e, inner))?;
        v.into_function()
            .ok_or_else(|| OxaError::new(codes::INVALID, "referenced value is not a function"))?
    };
    call_js(&ctx, inner, func, args)
}

// --- base64 for the $bytes bridge (no extra crate) ---

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = B64.iter().position(|&x| x == c)? as u32;
        acc = ((acc << 6) | v) & 0xFFFF;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Helpers the bridge needs (byte arrays). Installed as a private object, never as a global.
const HELPERS_SRC: &str = r#"(function () {
  const U8 = Uint8Array;
  const arrFrom = Array.from;
  return {
    isBytes: function (v) { return v instanceof U8; },
    bytesOf: function (v) { return arrFrom(v); },
    mkBytes: function (a) { return U8.from(a); }
  };
})()"#;

/// Bridge from the runtime's loader hooks to the embedder's [`ModuleLoader`].
struct Bridge {
    weak: Weak<Inner>,
}

impl rquickjs::loader::Resolver for Bridge {
    fn resolve<'js>(&mut self, ctx: &Ctx<'js>, base: &str, name: &str) -> rquickjs::Result<String> {
        let Some(inner) = self.weak.upgrade() else {
            return Err(rquickjs::Error::Unknown);
        };
        let guard = inner.loader.borrow();
        match guard.as_ref() {
            None => Err(throw_oxa(
                ctx,
                &OxaError::new(codes::NOT_FOUND, format!("module not found: {name}"))
                    .with_hint("no module loader is installed for this runtime"),
            )),
            Some(l) => l.resolve(base, name).map_err(|e| throw_oxa(ctx, &e)),
        }
    }
}

impl rquickjs::loader::Loader for Bridge {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
    ) -> rquickjs::Result<rquickjs::Module<'js, rquickjs::module::Declared>> {
        let Some(inner) = self.weak.upgrade() else {
            return Err(rquickjs::Error::Unknown);
        };
        let source = {
            let guard = inner.loader.borrow();
            match guard.as_ref() {
                None => Err(OxaError::new(
                    codes::NOT_FOUND,
                    format!("module not found: {name}"),
                )),
                Some(l) => l.load(name),
            }
        };
        match source {
            Ok(src) => rquickjs::Module::declare(ctx.clone(), name, src),
            Err(e) => Err(throw_oxa(ctx, &e)),
        }
    }
}

/// Install `console` and the bridge helpers into a fresh context.
fn install(ctx: &Ctx<'_>, inner: &Rc<Inner>) -> rquickjs::Result<()> {
    let weak = Rc::downgrade(inner);
    let console = Object::new(ctx.clone())?;
    for (name, level) in [
        ("log", "log"),
        ("info", "info"),
        ("warn", "warn"),
        ("error", "error"),
        ("debug", "debug"),
    ] {
        console.set(name, console_fn(ctx.clone(), weak.clone(), level)?)?;
    }
    ctx.globals().set("console", console)?;
    let helpers: Object<'_> = ctx.eval(HELPERS_SRC)?;
    *inner.helpers.borrow_mut() = Some(Persistent::save(ctx, helpers));
    Ok(())
}

impl Runtime {
    /// Create a runtime with the limits applied and no host functions.
    /// Errors: `script` when QuickJS cannot be initialised.
    pub fn new(limits: Limits) -> Result<Runtime, OxaError> {
        let init =
            |e: rquickjs::Error| OxaError::new(codes::SCRIPT, format!("QuickJS cannot be initialised: {e}"));
        let rt = rquickjs::Runtime::new().map_err(init)?;
        rt.set_memory_limit(limits.memory_bytes);
        rt.set_max_stack_size(limits.stack_bytes);
        let ctx = rquickjs::Context::full(&rt).map_err(init)?;
        let interrupt = InterruptHandle::default();
        let inner = Rc::new(Inner {
            fns: RefCell::new(HashMap::new()),
            modules: RefCell::new(HashMap::new()),
            helpers: RefCell::new(None),
            loader: RefCell::new(None),
            console: RefCell::new(None),
            cur_ctx: RefCell::new(None),
            cur_module: RefCell::new(None),
            rt_ptr: Cell::new(std::ptr::null_mut()),
            deadline: Cell::new(None),
            deadline_ms: Cell::new(0),
            timed_out: Cell::new(false),
            depth: Cell::new(0),
            interrupt: interrupt.clone(),
            ctx,
            rt,
        });
        let weak = Rc::downgrade(&inner);
        let hook = weak.clone();
        inner
            .rt
            .set_interrupt_handler(Some(Box::new(move || match hook.upgrade() {
                Some(i) => i.check_interrupt(),
                None => true,
            })));
        inner
            .rt
            .set_loader(Bridge { weak: weak.clone() }, Bridge { weak });
        inner.ctx.with(|ctx| install(&ctx, &inner)).map_err(init)?;
        Ok(Runtime {
            limits,
            interrupt,
            inner: Some(Box::new(inner)),
            _not_send: PhantomData,
        })
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Change the per-call deadline (codemode's `timeout_ms`).
    pub fn set_deadline(&mut self, deadline_ms: Option<u64>) {
        self.limits.deadline_ms = deadline_ms;
    }

    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.interrupt.clone()
    }

    /// Install the module loader used by `import`. Without one every import fails.
    pub fn set_loader(&mut self, loader: Box<dyn ModuleLoader>) {
        if let Some(inner) = state_ref(self) {
            *inner.loader.borrow_mut() = Some(loader);
        }
    }

    /// Route `console.*`. Default: dropped.
    pub fn set_console(&mut self, sink: ConsoleFn) {
        if let Some(inner) = state_ref(self) {
            *inner.console.borrow_mut() = Some(sink);
        }
    }

    /// Register a synchronous host function at a dotted global path (`"oxa.exec"` creates the
    /// global object `oxa` if needed). Registering the same path again replaces it.
    /// A returned `Err` is thrown into the script (see the crate docs).
    pub fn register(&mut self, path: &str, f: HostFn) {
        let f = Rc::new(f);
        let _ = enter(self, |inner, ctx| {
            let func = host_function(ctx.clone(), Rc::downgrade(inner), f.clone())
                .map_err(|e| rq(&ctx, e, inner))?;
            set_path(&ctx, path, func.into_value()).map_err(|e| rq(&ctx, e, inner))
        });
    }

    /// Like [`Runtime::register`], but the JS function returns a `Promise` that is resolved
    /// with the `Ok` value or rejected with the `Err` (so `Promise.allSettled` works over host
    /// calls). The host function still runs synchronously, in call order.
    pub fn register_async(&mut self, path: &str, f: HostFn) {
        let f = Rc::new(f);
        let _ = enter(self, |inner, ctx| {
            let func = host_async_function(ctx.clone(), Rc::downgrade(inner), f.clone())
                .map_err(|e| rq(&ctx, e, inner))?;
            set_path(&ctx, path, func.into_value()).map_err(|e| rq(&ctx, e, inner))
        });
    }

    /// Set a global (dotted path) to a JSON value.
    pub fn set_global(&mut self, path: &str, value: &Value) -> Result<(), OxaError> {
        enter(self, |inner, ctx| {
            let v = from_json(&ctx, inner, value)?;
            set_path(&ctx, path, v).map_err(|e| rq(&ctx, e, inner))
        })
    }

    /// Evaluate a classic script; the completion value is bridged back. A returned promise is
    /// awaited.
    pub fn eval_script(&mut self, name: &str, source: &str) -> Result<Value, OxaError> {
        let _ = name;
        enter(self, |inner, ctx| {
            let v: rquickjs::Value<'_> = ctx.eval(source.as_bytes()).map_err(|e| rq(&ctx, e, inner))?;
            let v = await_value(v).map_err(|e| rq(&ctx, e, inner))?;
            to_json(&ctx, inner, &v, 0)
        })
    }

    /// Evaluate `source` as the body of an `async function` (top-level `await` and `return`
    /// work): codemode's script form. The resolved value is bridged back.
    pub fn eval_async_body(&mut self, name: &str, source: &str) -> Result<Value, OxaError> {
        let _ = name;
        let wrapped = format!("(async function () {{\n{source}\n}})()");
        enter(self, |inner, ctx| {
            let v: rquickjs::Value<'_> = ctx.eval(wrapped.as_bytes()).map_err(|e| rq(&ctx, e, inner))?;
            let v = await_value(v).map_err(|e| rq(&ctx, e, inner))?;
            to_json(&ctx, inner, &v, 0)
        })
    }

    /// Evaluate an ES module (imports go through the loader) and run its top level.
    pub fn eval_module(&mut self, name: &str, source: &str) -> Result<ModuleRef, OxaError> {
        enter(self, |inner, ctx| {
            let declared =
                rquickjs::Module::declare(ctx.clone(), name, source).map_err(|e| rq(&ctx, e, inner))?;
            let (evaluated, promise) = declared.eval().map_err(|e| rq(&ctx, e, inner))?;
            promise
                .finish::<rquickjs::Value<'_>>()
                .map_err(|e| rq(&ctx, e, inner))?;
            let ns = evaluated.namespace().map_err(|e| rq(&ctx, e, inner))?;
            let id = next_id();
            inner.modules.borrow_mut().insert(id, Persistent::save(&ctx, ns));
            Ok(ModuleRef {
                id,
                name: name.to_string(),
            })
        })
    }

    /// Names of the module's exports, sorted.
    pub fn exports(&mut self, m: &ModuleRef) -> Result<Vec<String>, OxaError> {
        enter(self, |inner, ctx| {
            let ns = get_module(&ctx, inner, m)?;
            let mut names = Vec::new();
            for k in ns.keys::<String>() {
                names.push(k.map_err(|e| rq(&ctx, e, inner))?);
            }
            names.sort();
            Ok(names)
        })
    }

    /// Call an exported function (`"default"` for the default export) and await its result.
    /// Errors: `not_found` when the export is missing, `invalid` when it is not a function.
    pub fn call_export(&mut self, m: &ModuleRef, export: &str, args: &[Value]) -> Result<Value, OxaError> {
        enter(self, |inner, ctx| {
            let ns = get_module(&ctx, inner, m)?;
            let v: rquickjs::Value<'_> = ns.get(export).map_err(|e| rq(&ctx, e, inner))?;
            if v.is_undefined() {
                return Err(OxaError::new(
                    codes::NOT_FOUND,
                    format!("export not found: {export}"),
                ));
            }
            let func = v.into_function().ok_or_else(|| {
                OxaError::new(codes::INVALID, format!("export is not a function: {export}"))
            })?;
            call_js(&ctx, inner, func, args)
        })
    }

    /// Call a script function captured earlier and await its result.
    pub fn call(&mut self, f: &FnRef, args: &[Value]) -> Result<Value, OxaError> {
        enter(self, |inner, ctx| {
            let func = {
                let fns = inner.fns.borrow();
                let p = fns
                    .get(&f.0)
                    .ok_or_else(|| OxaError::new(codes::SCRIPT, "stale function reference"))?;
                let v: rquickjs::Value<'_> = p.clone().restore(&ctx).map_err(|e| rq(&ctx, e, inner))?;
                v.into_function()
                    .ok_or_else(|| OxaError::new(codes::INVALID, "referenced value is not a function"))?
            };
            call_js(&ctx, inner, func, args)
        })
    }

    /// Forget a captured function so it can be collected. Unknown ids are ignored.
    pub fn release(&mut self, f: &FnRef) {
        if let Some(inner) = state_ref(self) {
            inner.fns.borrow_mut().remove(&f.0);
        }
    }

    /// Current heap use in bytes.
    pub fn memory_used(&self) -> usize {
        match state_ref(self) {
            Some(inner) => inner.rt.memory_usage().memory_used_size.max(0) as usize,
            None => 0,
        }
    }
}
