use crate::{
    cache::Cache,
    error::RuntimeError,
    generated::{OutputEvent, PROTOCOL_VERSION, Request, Response, Status, TraceEvent},
    loader::Loader,
    permissions::PermissionSet,
};
use rust_jsc::{
    JSContext, JSError, JSFunction, JSModuleLoader, JSObject, JSPromise,
    JSPromiseResolvingFunctions, JSResult, JSString, JSStringProctected, JSValue, callback,
    module_evaluate, module_fetch, module_import_meta, module_resolve,
};
use rust_jsc_sys::{JSContextGroupRef, JSContextRef};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    path::Path,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

// Private JSC watchdog API is not exposed by rust_jsc_sys. Everything else uses
// the maintained binding. No C++ glue and no exception crosses this C interface.
unsafe extern "C" {
    fn JSContextGroupSetExecutionTimeLimit(
        group: JSContextGroupRef,
        limit: f64,
        callback: Option<unsafe extern "C" fn(JSContextRef, *mut std::ffi::c_void) -> bool>,
        data: *mut std::ffi::c_void,
    );
    fn JSContextGroupClearExecutionTimeLimit(group: JSContextGroupRef);
}
struct ContextOwner {
    context: JSContext,
    raw: rust_jsc_sys::JSGlobalContextRef,
    group: JSContextGroupRef,
}
impl ContextOwner {
    fn new(timeout_ms: u32) -> Self {
        unsafe {
            let group = rust_jsc_sys::JSContextGroupCreate();
            let raw = rust_jsc_sys::JSGlobalContextCreateInGroup(group, std::ptr::null_mut());
            let context = JSContext::from(raw);
            JSContextGroupSetExecutionTimeLimit(
                group,
                timeout_ms as f64 / 1000.0,
                None,
                std::ptr::null_mut(),
            );
            Self {
                context,
                raw,
                group,
            }
        }
    }
}
impl Drop for ContextOwner {
    fn drop(&mut self) {
        unsafe {
            if let Some(state) = self.context.get_shared_data::<Rc<Host>>() {
                state.pending.borrow_mut().clear();
            }
            drop(self.context.take_shared_data::<Rc<Host>>());
            JSContextGroupClearExecutionTimeLimit(self.group);
            rust_jsc_sys::JSGlobalContextRelease(self.raw);
            rust_jsc_sys::JSContextGroupRelease(self.group);
        }
    }
}
struct Pending {
    resolver: JSPromiseResolvingFunctions,
    task: tokio::task::JoinHandle<Result<Option<String>, RuntimeError>>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.task.abort();
        self.resolver.unprotect();
    }
}
struct Host {
    permissions: Arc<PermissionSet>,
    model: crate::generated::ModelOptions,
    allow_model: bool,
    timeout_ms: u32,
    loader: RefCell<Loader>,
    args: Vec<String>,
    output: RefCell<Vec<OutputEvent>>,
    output_bytes: RefCell<usize>,
    max_output: usize,
    pending: RefCell<VecDeque<Pending>>,
    error_modules: RefCell<HashMap<String, String>>,
    loader_failures: RefCell<Vec<RuntimeError>>,
    unhandled: RefCell<Option<RuntimeError>>,
    module_done: Cell<bool>,
    mcp_mode: String,
    mcp_result: RefCell<serde_json::Value>,
}
fn host(ctx: &JSContext) -> &Rc<Host> {
    ctx.get_shared_data::<Rc<Host>>()
        .expect("Ogma context state")
}
// The maintained binding's str conversions use CString and panic on NUL.
// Guest source, file contents and diagnostics are length-delimited Unicode.
fn raw_string(text: &str) -> rust_jsc_sys::JSStringRef {
    let utf16: Vec<u16> = text.encode_utf16().collect();
    unsafe { rust_jsc_sys::JSStringCreateWithCharacters(utf16.as_ptr(), utf16.len()) }
}
fn js_string(text: &str) -> JSString {
    JSString::from(raw_string(text))
}
fn loader_string(text: &str) -> JSStringProctected {
    JSStringProctected::from(raw_string(text))
}
fn js_error(ctx: &JSContext, error: RuntimeError) -> JSError {
    JSError::with_message(ctx, js_string(&error.to_string())).unwrap_or_else(|error| error)
}
fn argument(args: &[JSValue], index: usize) -> Result<String, RuntimeError> {
    args.get(index)
        .filter(|value| value.is_string())
        .ok_or_else(|| RuntimeError::new(Status::InvalidArgument, "expected a string"))?
        .as_string()
        .map(|s| s.to_string())
        .map_err(|e| RuntimeError::new(Status::Execution, format!("{e:?}")))
}
fn enqueue(
    ctx: &JSContext,
    task: tokio::task::JoinHandle<Result<Option<String>, RuntimeError>>,
) -> JSResult<JSValue> {
    if host(ctx).pending.borrow().len() >= 1024 {
        task.abort();
        return Err(js_error(
            ctx,
            RuntimeError::new(Status::ResourceLimit, "too many pending operations"),
        ));
    }
    let (promise, resolver) = JSPromise::new_pending(ctx)?;
    resolver.protect();
    host(ctx)
        .pending
        .borrow_mut()
        .push_back(Pending { resolver, task });
    Ok(promise.into())
}

#[callback]
fn native_call(
    ctx: JSContext,
    _function: JSObject,
    _this: JSObject,
    arguments: &[JSValue],
) -> JSResult<JSValue> {
    // A panic must never unwind through a JSC callback into C++.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(&ctx, arguments))) {
        Ok(result) => result,
        Err(_) => Err(js_error(
            &ctx,
            RuntimeError::new(Status::Panic, "native operation panicked"),
        )),
    }
}
fn dispatch(ctx: &JSContext, args: &[JSValue]) -> JSResult<JSValue> {
    let op = argument(args, 0).map_err(|e| js_error(ctx, e))?;
    let state = host(ctx);
    match op.as_str() {
        "mcpValidateTool" => {
            crate::mcp::tool(&argument(args, 1).map_err(|e| js_error(ctx, e))?)
                .map_err(|e| js_error(ctx, e))?;
            Ok(JSValue::undefined(ctx))
        }
        "mcpValidateArguments" => {
            let tool = crate::mcp::tool(&argument(args, 1).map_err(|e| js_error(ctx, e))?)
                .map_err(|e| js_error(ctx, e))?;
            crate::mcp::arguments(&tool, &argument(args, 2).map_err(|e| js_error(ctx, e))?)
                .map_err(|e| js_error(ctx, e))?;
            Ok(JSValue::undefined(ctx))
        }
        "mcpMissingTool" => Err(js_error(
            ctx,
            RuntimeError::new(Status::InvalidArgument, "unknown MCP tool"),
        )),
        "mcpResult" => {
            let text = argument(args, 1).map_err(|e| js_error(ctx, e))?;
            if !matches!(state.mcp_mode.as_str(), "mcp-list" | "mcp-call")
                || !state.mcp_result.borrow().is_null()
            {
                return Err(js_error(
                    ctx,
                    RuntimeError::new(Status::Protocol, "unexpected MCP result"),
                ));
            }
            let mut bytes = state.output_bytes.borrow_mut();
            if *bytes + text.len() > state.max_output {
                return Err(js_error(
                    ctx,
                    RuntimeError::new(Status::ResourceLimit, "MCP result exceeds output limit"),
                ));
            }
            *bytes += text.len();
            let result: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| js_error(ctx, RuntimeError::new(Status::Protocol, e.to_string())))?;
            if (state.mcp_mode == "mcp-list" && !result.is_array())
                || (state.mcp_mode == "mcp-call" && !result.is_string())
            {
                return Err(js_error(
                    ctx,
                    RuntimeError::new(Status::Protocol, "invalid MCP result type"),
                ));
            }
            *state.mcp_result.borrow_mut() = result;
            Ok(JSValue::undefined(ctx))
        }
        "print" => {
            let stream = argument(args, 1).map_err(|e| js_error(ctx, e))?;
            let text = argument(args, 2).map_err(|e| js_error(ctx, e))?;
            let mut size = state.output_bytes.borrow_mut();
            // Include event overhead and cap event count to bound JSON expansion.
            if *size + text.len() + 64 > state.max_output || state.output.borrow().len() >= 8192 {
                return Err(js_error(
                    ctx,
                    RuntimeError::new(Status::ResourceLimit, "output limit exceeded"),
                ));
            }
            *size += text.len() + 64;
            state.output.borrow_mut().push(OutputEvent { stream, text });
            Ok(JSValue::undefined(ctx))
        }
        "args" => Ok(JSValue::from_json(
            ctx,
            serde_json::to_string(&state.args).unwrap(),
        )),
        "env" => {
            let name = argument(args, 1).map_err(|e| js_error(ctx, e))?;
            match state.permissions.env(&name).map_err(|e| js_error(ctx, e))? {
                Some(value) => Ok(JSValue::string(ctx, js_string(&value))),
                None => Ok(JSValue::undefined(ctx)),
            }
        }
        "readTextFile" => {
            let path = argument(args, 1).map_err(|e| js_error(ctx, e))?;
            let permissions = state.permissions.clone();
            let task = tokio::task::spawn_blocking(move || {
                permissions.read_text(Path::new(&path)).map(Some)
            });
            enqueue(ctx, task)
        }
        "writeTextFile" => {
            let path = argument(args, 1).map_err(|e| js_error(ctx, e))?;
            let text = argument(args, 2).map_err(|e| js_error(ctx, e))?;
            let permissions = state.permissions.clone();
            let task = tokio::task::spawn_blocking(move || {
                permissions
                    .write_text(Path::new(&path), &text)
                    .map(|_| None)
            });
            enqueue(ctx, task)
        }
        "modelAsk" | "modelEmbed" => {
            let text = argument(args, 1).map_err(|e| js_error(ctx, e))?;
            let context = if op == "modelAsk" {
                argument(args, 2).map_err(|e| js_error(ctx, e))?
            } else {
                String::new()
            };
            let options = state.model.clone();
            let allowed = state.allow_model;
            let timeout = state.timeout_ms;
            let embedding = op == "modelEmbed";
            enqueue(
                ctx,
                tokio::spawn(async move {
                    if embedding {
                        crate::model::embedding(allowed, options, timeout, text)
                            .await
                            .map(Some)
                    } else {
                        crate::model::ask(allowed, options, timeout, text, context)
                            .await
                            .map(Some)
                    }
                }),
            )
        }
        "fetchText" => {
            let url = argument(args, 1).map_err(|e| js_error(ctx, e))?;
            let permissions = state.permissions.clone();
            enqueue(
                ctx,
                tokio::spawn(async move { fetch_text(permissions, url).await.map(Some) }),
            )
        }
        "sleep" => {
            let value = args
                .get(1)
                .filter(|v| v.is_number())
                .ok_or_else(|| {
                    js_error(
                        ctx,
                        RuntimeError::new(Status::InvalidArgument, "expected milliseconds"),
                    )
                })?
                .as_number()?;
            if !value.is_finite() || !(0.0..=600_000.0).contains(&value) {
                return Err(js_error(
                    ctx,
                    RuntimeError::new(Status::InvalidArgument, "invalid sleep duration"),
                ));
            }
            enqueue(
                ctx,
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs_f64(value / 1000.0)).await;
                    Ok(None)
                }),
            )
        }
        _ => Err(js_error(
            ctx,
            RuntimeError::new(Status::Unsupported, "unsupported native operation"),
        )),
    }
}

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
async fn fetch_text(permissions: Arc<PermissionSet>, text: String) -> Result<String, RuntimeError> {
    let mut url = permissions.net_url(&text)?;
    // Environment proxy settings must never route guest requests outside their grants.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|_| RuntimeError::new(Status::Io, "HTTP client initialization failed"))?;
    for redirects in 0..=5 {
        // Recheck every destination, including relative and cross-host redirects.
        url = permissions.net_url(url.as_str())?;
        let mut response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| RuntimeError::new(Status::Io, "HTTP request failed"))?;
        let status = response.status();
        if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
            if redirects == 5 {
                return Err(RuntimeError::new(
                    Status::Io,
                    "HTTP redirect limit exceeded",
                ));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    RuntimeError::new(Status::Io, "HTTP redirect requires a valid Location")
                })?;
            crate::permissions::validate_net_url_text(location)?;
            url = url.join(location).map_err(|_| {
                RuntimeError::new(Status::Io, "HTTP redirect has an invalid Location")
            })?;
            continue;
        }
        if !status.is_success() {
            return Err(RuntimeError::new(
                Status::Io,
                format!("HTTP request returned status {}", status.as_u16()),
            ));
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            return Err(RuntimeError::new(
                Status::ResourceLimit,
                "HTTP response exceeds 16 MiB",
            ));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| RuntimeError::new(Status::Io, "HTTP response body failed"))?
        {
            if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
                return Err(RuntimeError::new(
                    Status::ResourceLimit,
                    "HTTP response exceeds 16 MiB",
                ));
            }
            body.extend_from_slice(&chunk);
        }
        return String::from_utf8(body)
            .map_err(|_| RuntimeError::new(Status::Io, "HTTP response is not UTF-8"));
    }
    Err(RuntimeError::new(
        Status::Io,
        "HTTP redirect limit exceeded",
    ))
}

fn throwing_module(error: &RuntimeError) -> String {
    format!(
        "throw new Error({});",
        serde_json::to_string(&error.to_string()).unwrap()
    )
}
#[module_resolve]
fn resolve_module(
    ctx: JSContext,
    key: JSValue,
    referrer: JSValue,
    _fetcher: JSValue,
) -> JSStringProctected {
    // Include conversion and error-module registration inside the panic boundary.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let result = (|| {
            let key = key.as_string().map_err(engine_error)?.to_string();
            let base = if referrer.is_undefined() {
                key.clone()
            } else {
                referrer.as_string().map_err(engine_error)?.to_string()
            };
            host(&ctx).loader.borrow_mut().resolve_import(&key, &base)
        })();
        match result {
            Ok(key) => loader_string(&key),
            Err(error) => {
                let state = host(&ctx);
                if state.loader_failures.borrow().len() < 4096 {
                    state.loader_failures.borrow_mut().push(error.clone());
                }
                let mut modules = state.error_modules.borrow_mut();
                let key = format!("ogma:rejected/{}", modules.len());
                modules.insert(key.clone(), throwing_module(&error));
                loader_string(&key)
            }
        }
    }))
    .unwrap_or_else(|_| loader_string("ogma:loader-panic"))
}
#[module_fetch]
fn fetch_module(
    ctx: JSContext,
    key: JSValue,
    _attributes: JSValue,
    _fetcher: JSValue,
) -> JSStringProctected {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let result = (|| {
            let key = key.as_string().map_err(engine_error)?.to_string();
            if key == "ogma:loader-panic" {
                return Err(RuntimeError::new(
                    Status::Panic,
                    "module resolution panicked",
                ));
            }
            if let Some(source) = host(&ctx).error_modules.borrow().get(&key) {
                return Ok(source.clone());
            }
            host(&ctx).loader.borrow_mut().load(&key)
        })();
        loader_string(&match result {
            Ok(source) => source,
            Err(mut error) => {
                if let Ok(key) = key.as_string() {
                    error.specifier = key.to_string();
                }
                let state = host(&ctx);
                if state.loader_failures.borrow().len() < 4096 {
                    state.loader_failures.borrow_mut().push(error.clone());
                }
                throwing_module(&error)
            }
        })
    }))
    .unwrap_or_else(|_| loader_string("throw new Error('OGMA_PANIC: module loading panicked');"))
}
#[module_evaluate]
fn evaluate_synthetic(ctx: JSContext, _key: JSValue) -> JSValue {
    // No synthetic modules are registered. JSC nevertheless requires the
    // callback to activate its custom loader; real ESM uses its native evaluator.
    JSValue::undefined(&ctx)
}
#[callback]
fn completed(
    ctx: JSContext,
    _function: JSObject,
    _this: JSObject,
    _args: &[JSValue],
) -> JSResult<JSValue> {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        host(&ctx).module_done.set(true)
    }));
    Ok(JSValue::undefined(&ctx))
}
#[module_import_meta]
fn import_meta(ctx: JSContext, key: JSValue, _fetcher: JSValue) -> JSObject {
    let object = JSObject::new(&ctx);
    let _ = object.set_property("url", &key, Default::default());
    object
}
#[callback]
fn rejected(
    ctx: JSContext,
    _function: JSObject,
    _this: JSObject,
    args: &[JSValue],
) -> JSResult<JSValue> {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(value) = args.last() else {
            return;
        };
        let mut error = classify_js(
            value
                .as_string()
                .map(|s| s.to_string())
                .unwrap_or_else(|_| "unhandled rejection".into()),
        );
        if let Ok(object) = value.as_object() {
            let get_string = |key| {
                object
                    .get_property(key)
                    .ok()
                    .filter(|v| !v.is_undefined())
                    .and_then(|v| v.as_string().ok())
                    .map(|s| s.to_string())
            };
            let get_number = |key| {
                object
                    .get_property(key)
                    .ok()
                    .and_then(|v| v.as_number().ok())
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .unwrap_or(0.0) as u32
            };
            error.specifier = get_string("sourceURL").unwrap_or_default();
            error.line = get_number("line");
            error.column = get_number("column");
            if let Some(stack) = get_string("stack") {
                error.message = format!("{}\n{}", error.message, stack);
            }
            let state = host(&ctx);
            if let Some(map) = state.loader.borrow().maps.get(&error.specifier)
                && let Ok(map) = sourcemap::decode_slice(map.as_bytes())
                && let Some(token) =
                    map.lookup_token(error.line.saturating_sub(1), error.column.saturating_sub(1))
            {
                error.line = token.get_src_line() + 1;
                error.column = token.get_src_col() + 1;
                if let Some(source) = token.get_source() {
                    error.specifier = source.into();
                }
            }
        }
        *host(&ctx).unhandled.borrow_mut() = Some(error);
    }));
    Ok(JSValue::undefined(&ctx))
}
fn classify_js(message: String) -> RuntimeError {
    // Native diagnostics carry stable codes; ordinary guest exceptions stay execution errors.
    let status = if message.contains("OGMA_UNSUPPORTED:") {
        Status::Unsupported
    } else if message.contains("OGMA_IO:") {
        Status::Io
    } else if message.contains("OGMA_INVALID_ARGUMENT:") {
        Status::InvalidArgument
    } else if message.contains("OGMA_PANIC:") {
        Status::Panic
    } else if message.contains("OGMA_PERMISSION_DENIED:") {
        Status::PermissionDenied
    } else if message.contains("OGMA_RESOURCE_LIMIT:") {
        Status::ResourceLimit
    } else if message.contains("execution terminated") || message.contains("terminated execution") {
        Status::Timeout
    } else {
        Status::Execution
    };
    RuntimeError::new(status, message)
}
fn evaluate_at(owner: &ContextOwner, source: &str, url: &str) -> Result<JSValue, RuntimeError> {
    // rust_jsc's convenience evaluate_script omits sourceURL. JSC dynamic
    // import needs a file referrer; use its public C API from the maintained sys binding.
    let source: rust_jsc_sys::JSStringRef = raw_string(source);
    let url: rust_jsc_sys::JSStringRef = raw_string(url);
    let mut exception = std::ptr::null();
    let value = unsafe {
        rust_jsc_sys::JSEvaluateScript(
            owner.raw,
            source,
            std::ptr::null_mut(),
            url,
            1,
            &mut exception,
        )
    };
    unsafe {
        rust_jsc_sys::JSStringRelease(source);
        rust_jsc_sys::JSStringRelease(url);
    }
    if !exception.is_null() {
        return Err(engine_error(JSValue::new(exception, owner.raw).into()));
    }
    Ok(JSValue::new(value, owner.raw))
}
fn engine_at(stage: &str, error: JSError) -> RuntimeError {
    let mut error = engine_error(error);
    error.message = format!("{stage}: {}", error.message);
    error
}
fn engine_error(error: JSError) -> RuntimeError {
    let message = error
        .stack()
        .map(|s| s.to_string())
        .or_else(|_| error.message().map(|s| s.to_string()))
        .unwrap_or_else(|_| format!("{error:?}"));
    classify_js(message)
}

pub fn execute(request: Request) -> Response {
    let started = Instant::now();
    let id = request.id.clone();
    let entry = request.entry.clone();
    let result = execute_inner(&request);
    match result {
        Ok(mut response) => {
            if request.trace {
                response.trace.push(TraceEvent {
                    name: "execution".into(),
                    elapsed_micros: started.elapsed().as_micros() as u64,
                    count: 0,
                });
            }
            response
        }
        Err(error) => error.response(id, entry),
    }
}
fn execute_inner(request: &Request) -> Result<Response, RuntimeError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let cache = Cache::new(&request.cache_directory);
        let mut permissions = PermissionSet::new(&request.permissions)?;
        permissions.reserve_cache(cache.root());
        if request.permissions.model {
            permissions.reserve_cache(Some(&crate::model_store::store_root(request)?));
        }
        let permissions = Arc::new(permissions);
        let entry = if request.command == "eval" {
            std::env::current_dir()?.join("__ogma_eval__.ts")
        } else {
            let path = Path::new(&request.entry);
            if path.is_absolute() {
                path.to_owned()
            } else {
                std::env::current_dir()?.join(path)
            }
        };
        let entry = url::Url::from_file_path(&entry)
            .map_err(|_| RuntimeError::new(Status::InvalidArgument, "invalid entry path"))?
            .to_string();
        let eval = if request.command == "eval" {
            Some((entry.clone(), request.source.clone()))
        } else {
            None
        };
        let state = Rc::new(Host {
            permissions: permissions.clone(),
            model: request.model.clone(),
            allow_model: request.permissions.model,
            timeout_ms: request.limits.timeout_ms,
            loader: RefCell::new(Loader::new(permissions, cache, eval)),
            args: request.args.clone(),
            output: Default::default(),
            output_bytes: Default::default(),
            max_output: request.limits.max_output_bytes as usize,
            pending: Default::default(),
            error_modules: Default::default(),
            loader_failures: Default::default(),
            unhandled: Default::default(),
            module_done: Cell::new(false),
            mcp_mode: request.command.clone(),
            mcp_result: Default::default(),
        });
        let mut initialization_micros = 0;
        let result: Result<(), RuntimeError> = async {
            let initialization = Instant::now();
            let owner = ContextOwner::new(request.limits.timeout_ms);
            initialize_context(&owner, &state)?;
            initialization_micros = initialization.elapsed().as_micros() as u64;
            let ctx = &owner.context;
            // Use ECMAScript dynamic import's Promise. The fork's convenience
            // JSLoadAndEvaluateModule stores a stack exception pointer across async
            // evaluation; using import() avoids that lifetime hazard entirely.
            let expression = format!("import({})", serde_json::to_string(&entry).unwrap());
            observe(ctx, &state, evaluate_at(&owner, &expression, &entry)?)?;
            finish_phase(ctx, &state, request.limits.timeout_ms, "module evaluation").await?;
            if matches!(request.command.as_str(), "mcp-list" | "mcp-call") {
                let expression = format!(
                    "globalThis.__ogmaRunMcp({}, {});",
                    serde_json::to_string(&request.command).unwrap(),
                    serde_json::to_string(&request.mcp).unwrap()
                );
                let promise = evaluate_at(&owner, &expression, &entry)?;
                observe(ctx, &state, promise)?;
                finish_phase(ctx, &state, request.limits.timeout_ms, "MCP tool execution").await?;
            }
            if request.command == "test" {
                let promise = ctx
                    .evaluate_script("globalThis.__ogmaRunTests();", None)
                    .map_err(engine_error)?;
                observe(ctx, &state, promise)?;
                finish_phase(ctx, &state, request.limits.timeout_ms, "test runner").await?;
            }
            Ok(())
        }
        .await;
        let output = state.output.borrow().clone();
        let mut response = match result {
            Ok(()) => Response {
                protocol_version: PROTOCOL_VERSION,
                id: request.id.clone(),
                status: "ok".into(),
                ..Default::default()
            },
            Err(error) => error.response(request.id.clone(), request.entry.clone()),
        };
        response.output = output;
        if response.status == "ok" && matches!(request.command.as_str(), "mcp-list" | "mcp-call") {
            response.result = state.mcp_result.borrow().clone();
        }
        add_loader_diagnostics(&state, &mut response);
        if request.trace {
            let loader = state.loader.borrow();
            for (name, count) in [
                ("modules", loader.module_count as u64),
                ("transformCacheHits", loader.transform_hits),
                ("graphCacheHits", loader.graph_hits),
            ] {
                response.trace.push(TraceEvent {
                    name: name.into(),
                    elapsed_micros: 0,
                    count,
                });
            }
            for (name, elapsed_micros) in [
                ("contextInitialization", initialization_micros),
                ("moduleLoading", loader.load_micros),
                ("typescriptTransform", loader.transform_micros),
            ] {
                response.trace.push(TraceEvent {
                    name: name.into(),
                    elapsed_micros,
                    count: 0,
                });
            }
        }
        Ok(response)
    })
}
fn observe(ctx: &JSContext, state: &Rc<Host>, value: JSValue) -> Result<(), RuntimeError> {
    state.module_done.set(false);
    let promise = value.as_object().map_err(engine_error)?;
    promise.protect();
    let complete = JSFunction::callback(ctx, Some("ogmaComplete"), Some(completed));
    let reject = JSFunction::callback(ctx, Some("ogmaRejected"), Some(rejected));
    let observed = promise
        .get_property("then")
        .and_then(|v| v.as_object())
        .and_then(|then| then.call(Some(&promise), &[complete.into(), reject.into()]));
    promise.unprotect();
    observed.map_err(engine_error)?;
    Ok(())
}

fn add_loader_diagnostics(state: &Host, response: &mut Response) {
    // The binding returns source strings, not a rejected fetch. Named static
    // imports can fail linking before an error module throws. Report the real
    // loading failures alongside JSC's link error, without guessing causality.
    if response
        .diagnostics
        .iter()
        .any(|d| d.code == "OGMA_EXECUTION" && d.message.contains("Importing binding name"))
    {
        let mut diagnostics = state
            .loader_failures
            .borrow()
            .iter()
            .take(32)
            .flat_map(|error| {
                error
                    .response(response.id.clone(), String::new())
                    .diagnostics
            })
            .collect::<Vec<_>>();
        diagnostics.append(&mut response.diagnostics);
        response.diagnostics = diagnostics;
    }
}
async fn finish_phase(
    ctx: &JSContext,
    state: &Rc<Host>,
    timeout_ms: u32,
    name: &str,
) -> Result<(), RuntimeError> {
    drain(ctx, state, timeout_ms).await?;
    if let Some(error) = state.unhandled.borrow_mut().take() {
        return Err(error);
    }
    if !state.module_done.get() {
        return Err(RuntimeError::new(
            Status::Execution,
            format!("{name} has an unresolved promise"),
        ));
    }
    Ok(())
}
async fn drain(ctx: &JSContext, state: &Rc<Host>, timeout_ms: u32) -> Result<(), RuntimeError> {
    tokio::time::timeout(Duration::from_millis(timeout_ms as u64), async {
        loop {
            if state.pending.borrow().is_empty() {
                break;
            }
            // Poll every task; waiting on queue order would incorrectly make a
            // slow earlier I/O operation win Promise.race over a ready later one.
            let (pending, result) = std::future::poll_fn(|cx| {
                use std::{future::Future, pin::Pin, task::Poll};
                let mut queue = state.pending.borrow_mut();
                for index in 0..queue.len() {
                    if let Poll::Ready(result) = Pin::new(&mut queue[index].task).poll(cx) {
                        return Poll::Ready((queue.remove(index).unwrap(), result));
                    }
                }
                Poll::Pending
            })
            .await;
            let result = result.map_err(|e| RuntimeError::new(Status::Execution, e.to_string()))?;
            match result {
                Ok(Some(text)) => {
                    pending
                        .resolver
                        .resolve(None, &[JSValue::string(ctx, js_string(&text))])
                        .map_err(engine_error)?;
                }
                Ok(None) => {
                    pending
                        .resolver
                        .resolve(None, &[JSValue::undefined(ctx)])
                        .map_err(engine_error)?;
                }
                Err(error) => {
                    pending
                        .resolver
                        .reject(None, &[js_error(ctx, error).into()])
                        .map_err(engine_error)?;
                }
            }
            // Returning through the C API drains JSC's promise jobs. Also pump jobs
            // after settling native operations, including newly scheduled operations.
            ctx.evaluate_script("void 0", None).map_err(engine_error)?;
        }
        Ok(())
    })
    .await
    .map_err(|_| RuntimeError::new(Status::Timeout, "async deadline exceeded"))?
}

fn initialize_context(owner: &ContextOwner, state: &Rc<Host>) -> Result<(), RuntimeError> {
    let ctx = &owner.context;
    ctx.set_shared_data(state.clone());
    ctx.set_module_loader(JSModuleLoader {
        disableBuiltinFileSystemLoader: true,
        moduleLoaderResolve: Some(resolve_module),
        moduleLoaderEvaluate: Some(evaluate_synthetic),
        moduleLoaderFetch: Some(fetch_module),
        moduleLoaderCreateImportMetaProperties: Some(import_meta),
    });
    let native = JSFunction::callback(ctx, Some("ogmaHost"), Some(native_call));
    ctx.global_object()
        .set_property("__ogmaHost", &native, Default::default())
        .map_err(|e| engine_at("native callback", e))?;
    let reject = JSFunction::callback(ctx, Some("ogmaRejected"), Some(rejected));
    ctx.set_unhandled_rejection_callback(reject.as_object().map_err(engine_error)?)
        .map_err(|e| engine_at("rejection callback", e))?;
    ctx.evaluate_script(include_str!("bootstrap.js"), None)
        .map_err(|e| engine_at("bootstrap", e))?;
    Ok(())
}

/// An explicitly persistent, isolated REPL context. Never recreated after a
/// failed worker. Guest exceptions leave the same context available to callers.
pub struct ReplSession {
    owner: ContextOwner,
    state: Rc<Host>,
    runtime: tokio::runtime::Runtime,
    entry: url::Url,
    sequence: u64,
}
impl ReplSession {
    pub fn new(request: &Request) -> Result<Self, RuntimeError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let cache = Cache::new(&request.cache_directory);
        let mut permissions = PermissionSet::new(&request.permissions)?;
        permissions.reserve_cache(cache.root());
        if request.permissions.model {
            permissions.reserve_cache(Some(&crate::model_store::store_root(request)?));
        }
        let permissions = Arc::new(permissions);
        let state = Rc::new(Host {
            permissions: permissions.clone(),
            model: request.model.clone(),
            allow_model: request.permissions.model,
            timeout_ms: request.limits.timeout_ms,
            loader: RefCell::new(Loader::new(permissions, cache, None)),
            args: request.args.clone(),
            output: Default::default(),
            output_bytes: Default::default(),
            max_output: request.limits.max_output_bytes as usize,
            pending: Default::default(),
            error_modules: Default::default(),
            loader_failures: Default::default(),
            unhandled: Default::default(),
            module_done: Cell::new(false),
            mcp_mode: request.command.clone(),
            mcp_result: Default::default(),
        });
        let owner = ContextOwner::new(request.limits.timeout_ms);
        initialize_context(&owner, &state)?;
        unsafe {
            JSContextGroupClearExecutionTimeLimit(owner.group);
        }
        let entry = url::Url::from_file_path(std::env::current_dir()?.join("__ogma_repl__.ts"))
            .map_err(|_| RuntimeError::new(Status::InvalidArgument, "invalid REPL directory"))?;
        Ok(Self {
            owner,
            state,
            runtime,
            entry,
            sequence: 0,
        })
    }
    pub fn evaluate(&mut self, request: &Request) -> Response {
        self.sequence += 1;
        self.state.output.borrow_mut().clear();
        *self.state.output_bytes.borrow_mut() = 0;
        self.state.unhandled.borrow_mut().take();
        self.state.loader_failures.borrow_mut().clear();
        self.state.module_done.set(false);
        let started = Instant::now();
        unsafe {
            JSContextGroupSetExecutionTimeLimit(
                self.owner.group,
                request.limits.timeout_ms as f64 / 1000.0,
                None,
                std::ptr::null_mut(),
            );
        }
        let result = (|| {
            let source = repl_source(&self.entry, &request.source)?;
            self.runtime.block_on(async {
                let value = evaluate_at(&self.owner, &source, self.entry.as_str())?;
                observe(&self.owner.context, &self.state, value)?;
                finish_phase(
                    &self.owner.context,
                    &self.state,
                    request.limits.timeout_ms,
                    "REPL evaluation",
                )
                .await
            })
        })();
        // No asynchronous task can survive across evaluation boundaries. A
        // cancellation kills this worker externally and never replays its state.
        self.state.pending.borrow_mut().clear();
        unsafe {
            JSContextGroupClearExecutionTimeLimit(self.owner.group);
        }
        let mut response = match result {
            Ok(()) => Response {
                protocol_version: PROTOCOL_VERSION,
                id: request.id.clone(),
                status: "ok".into(),
                ..Default::default()
            },
            Err(error) => error.response(request.id.clone(), self.entry.to_string()),
        };
        response.output = self.state.output.borrow().clone();
        add_loader_diagnostics(&self.state, &mut response);
        if request.trace {
            response.trace.push(TraceEvent {
                name: "repl".into(),
                elapsed_micros: started.elapsed().as_micros() as u64,
                count: self.sequence,
            });
        }
        response
    }
}

// REPL bindings are mutable properties on the persistent global object. Parsing
// statement spans (rather than replacing text heuristically) keeps strings,
// comments, nested declarations and destructuring intact. The async wrapper
// supports await/import while top-level declarations survive the wrapper scope.
fn repl_source(url: &url::Url, source: &str) -> Result<String, RuntimeError> {
    use deno_ast::swc::{ast::*, common::Spanned};
    let parsed = deno_ast::parse_module(deno_ast::ParseParams {
        specifier: url.clone(),
        text: source.into(),
        media_type: deno_ast::MediaType::TypeScript,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(|e| RuntimeError::new(Status::Execution, e.to_string()))?;
    let emitted = parsed
        .transpile(
            &deno_ast::TranspileOptions {
                var_decl_imports: true,
                ..Default::default()
            },
            &Default::default(),
            &deno_ast::EmitOptions {
                source_map: deno_ast::SourceMapOption::None,
                ..Default::default()
            },
        )
        .map_err(|e| RuntimeError::new(Status::Execution, e.to_string()))?
        .into_source()
        .text;
    let js = deno_ast::parse_module(deno_ast::ParseParams {
        specifier: url.clone(),
        text: emitted.clone().into(),
        media_type: deno_ast::MediaType::JavaScript,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(|e| RuntimeError::new(Status::Execution, e.to_string()))?;
    let program = js.program();
    let Program::Module(module) = &*program else {
        unreachable!()
    };
    let start = deno_ast::StartSourcePos::START_SOURCE_POS.as_byte_pos().0;
    let snippet = |span: deno_ast::swc::common::Span| -> &str {
        &emitted[(span.lo.0 - start) as usize..(span.hi.0 - start) as usize]
    };
    let mut body = String::new();
    for (index, item) in module.body.iter().enumerate() {
        match item {
            ModuleItem::Stmt(Stmt::Decl(Decl::Var(decl))) => {
                for binding in &decl.decls {
                    let name = snippet(binding.name.span());
                    let value = binding
                        .init
                        .as_ref()
                        .map(|v| snippet(v.span()))
                        .unwrap_or("undefined");
                    if matches!(binding.name, Pat::Ident(_)) {
                        body.push_str(&format!("globalThis.{name} = ({value});\n"));
                    } else {
                        body.push_str(&format!("({name} = ({value}));\n"));
                    }
                }
            }
            ModuleItem::Stmt(Stmt::Decl(Decl::Fn(decl))) => {
                body.push_str(&format!(
                    "globalThis.{} = ({});\n",
                    decl.ident.sym,
                    snippet(item.span())
                ));
            }
            ModuleItem::Stmt(Stmt::Decl(Decl::Class(decl))) => {
                body.push_str(&format!(
                    "globalThis.{} = ({});\n",
                    decl.ident.sym,
                    snippet(item.span())
                ));
            }
            ModuleItem::Stmt(Stmt::Expr(expr)) if index + 1 == module.body.len() => {
                body.push_str(&format!("return ({});\n", snippet(expr.expr.span())));
            }
            ModuleItem::ModuleDecl(_) => {
                return Err(RuntimeError::new(
                    Status::Unsupported,
                    "REPL exports are unsupported; use run for exported modules",
                ));
            }
            _ => {
                body.push_str(snippet(item.span()));
                body.push('\n');
            }
        }
    }
    Ok(format!(
        "(async function(){{\n{body}\n}})().then(value => {{ if (value !== undefined) console.log(value); }})"
    ))
}
