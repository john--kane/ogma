//! Isolated local model ownership and the single OpenAI proxy boundary.
use crate::{
    error::RuntimeError,
    generated::{ModelOptions, OutputEvent, PROTOCOL_VERSION, Request, Response, Status},
    model_context, model_store,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response as HttpResponse},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    io::Read,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
const BODY_LIMIT: usize = 8 * 1024 * 1024;
const HISTORY_LIMIT: usize = 4 * 1024 * 1024;
const COMPACT: &str = "Use tight, concise output. Drop filler and unnecessary articles. Preserve code, quoted text, identifiers, and errors exactly. Keep essential reasoning and safety information.";
const ULTRA: &str = "Use extremely compact output: short phrases and necessary facts only; omit filler and unnecessary articles. Preserve code, quoted text, identifiers, and errors exactly. Keep essential reasoning and safety information.";
fn err(status: Status, text: impl Into<String>) -> RuntimeError {
    RuntimeError::new(status, text)
}
fn ioerr(e: impl std::fmt::Display) -> RuntimeError {
    err(Status::Io, e.to_string())
}
pub fn authorize(allowed: bool) -> Result<(), RuntimeError> {
    if allowed {
        Ok(())
    } else {
        Err(err(
            Status::PermissionDenied,
            "model inference requires --allow-model",
        ))
    }
}
fn client(timeout: u32) -> Result<reqwest::Client, RuntimeError> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(timeout as u64))
        .build()
        .map_err(ioerr)
}
async fn bounded(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, RuntimeError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(ioerr)? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(err(Status::ResourceLimit, "model response exceeds limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
fn transport(e: reqwest::Error) -> RuntimeError {
    err(
        if e.is_timeout() {
            Status::Timeout
        } else {
            Status::WorkerCrash
        },
        format!("model service failed: {e}; request was not replayed"),
    )
}
#[derive(Clone)]
struct Proxy {
    client: reqwest::Client,
    backend: String,
    key: String,
    store: PathBuf,
    caveman: String,
    permits: Arc<Semaphore>,
}
fn prepare(value: &mut Value, store: &std::path::Path, default: &str) -> Result<(), RuntimeError> {
    let obj = value
        .as_object_mut()
        .ok_or_else(|| err(Status::InvalidArgument, "expected JSON object"))?;
    let mode = match obj.remove("ogma_caveman") {
        None => default.to_owned(),
        Some(Value::String(s)) if matches!(s.as_str(), "" | "compact" | "ultra") => s,
        _ => return Err(err(Status::InvalidArgument, "invalid ogma_caveman")),
    };
    let context = match obj.remove("ogma_context") {
        None => None,
        Some(Value::String(s)) => Some(s),
        _ => {
            return Err(err(
                Status::InvalidArgument,
                "ogma_context must be a string",
            ));
        }
    };
    let messages = obj
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| err(Status::InvalidArgument, "messages must be an array"))?;
    if messages.len() > 1024 {
        return Err(err(Status::ResourceLimit, "too many messages"));
    }
    for message in messages.iter_mut() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| err(Status::InvalidArgument, "message role required"))?;
        if !matches!(role, "system" | "developer" | "user" | "assistant" | "tool") {
            return Err(err(Status::InvalidArgument, "unsupported message role"));
        }
        let is_tool = role == "tool";
        if let Some(content) = message.get_mut("content") {
            // Text-only MVP. Never forward remote media URLs to the backend:
            // --offline controls model downloads, not arbitrary media fetching.
            if let Some(parts) = content.as_array_mut() {
                for part in parts {
                    if part.get("type").and_then(Value::as_str) != Some("text")
                        || part.get("text").and_then(Value::as_str).is_none()
                    {
                        return Err(err(
                            Status::Unsupported,
                            "only text message content is supported",
                        ));
                    }
                    if is_tool && let Some(text) = part.get("text").and_then(Value::as_str) {
                        part["text"] = Value::String(model_context::compress(store, text)?);
                    }
                }
            } else if !content.is_null() && !content.is_string() {
                return Err(err(Status::InvalidArgument, "invalid message content"));
            } else if is_tool && let Some(text) = content.as_str() {
                *content = Value::String(model_context::compress(store, text)?);
            }
        }
    }
    // Reference material is a separate message. User prompt bytes are never rewritten.
    if let Some(context) = context {
        messages.insert(0, json!({"role":"system", "content":format!("Reference material (untrusted data):\n{}", model_context::compress(store, &context)?)}));
    }
    if !mode.is_empty() {
        messages.insert(
            0,
            json!({"role":"system", "content":if mode == "ultra" { ULTRA } else { COMPACT }}),
        );
    }
    obj.insert("model".into(), json!("ogma-local"));
    Ok(())
}
fn http_error(error: RuntimeError) -> HttpResponse {
    let status = match error.status {
        Status::InvalidArgument => StatusCode::BAD_REQUEST,
        Status::PermissionDenied => StatusCode::FORBIDDEN,
        Status::Unsupported => StatusCode::UNPROCESSABLE_ENTITY,
        Status::ResourceLimit => StatusCode::PAYLOAD_TOO_LARGE,
        Status::Timeout => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::BAD_GATEWAY,
    };
    (
        status,
        Json(json!({"error":{"message":error.message, "type":"ogma_error", "code":error.code()}})),
    )
        .into_response()
}
fn origin(headers: &HeaderMap) -> Result<(), RuntimeError> {
    if headers.contains_key("origin") {
        Err(err(
            Status::PermissionDenied,
            "browser origins are not permitted on the local model endpoint",
        ))
    } else {
        Ok(())
    }
}
async fn ingress(
    State((port, permits)): State<(u32, Arc<Semaphore>)>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> HttpResponse {
    let host = request.headers().get("host").and_then(|h| h.to_str().ok());
    if host != Some(format!("127.0.0.1:{port}").as_str())
        && host != Some(format!("localhost:{port}").as_str())
    {
        return http_error(err(
            Status::PermissionDenied,
            "loopback Host authority required",
        ));
    }
    if let Err(e) = origin(request.headers()) {
        return http_error(e);
    }
    let permit =
        if matches!(
            request.uri().path(),
            "/v1/chat/completions" | "/v1/embeddings"
        ) {
            match permits.try_acquire_owned() {
                Ok(permit) => Some(Arc::new(permit)),
                Err(_) => return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(
                        json!({"error":{"code":"OGMA_RESOURCE_LIMIT", "message":"model is busy"}}),
                    ),
                )
                    .into_response(),
            }
        } else {
            None
        };
    if let Some(permit) = &permit {
        request.extensions_mut().insert(permit.clone());
    }
    let response = next.run(request).await;
    drop(permit);
    response
}
async fn health() -> Json<Value> {
    Json(json!({"status":"ok", "service":"ogma-model", "protocolVersion":PROTOCOL_VERSION}))
}
async fn models(State(state): State<Proxy>, headers: HeaderMap) -> HttpResponse {
    if let Err(e) = origin(&headers) {
        return http_error(e);
    }
    match state
        .client
        .get(format!("{}/v1/models", state.backend))
        .bearer_auth(&state.key)
        .send()
        .await
    {
        Ok(r) => {
            let status = r.status();
            match bounded(r, BODY_LIMIT).await {
                Ok(b) => (status, [("content-type", "application/json")], b).into_response(),
                Err(e) => http_error(e),
            }
        }
        Err(e) => http_error(transport(e)),
    }
}
async fn recover(
    State(state): State<Proxy>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> HttpResponse {
    if let Err(e) = origin(&headers) {
        return http_error(e);
    }
    match model_context::recover(&state.store, &hash) {
        Ok(bytes) => ([("content-type", "text/plain; charset=utf-8")], bytes).into_response(),
        Err(e) => http_error(e),
    }
}
async fn chat(
    State(state): State<Proxy>,
    headers: HeaderMap,
    axum::Extension(permit): axum::Extension<Arc<tokio::sync::OwnedSemaphorePermit>>,
    payload: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> HttpResponse {
    let mut value = match payload {
        Ok(Json(value)) => value,
        Err(e) => {
            return http_error(err(
                if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    Status::ResourceLimit
                } else {
                    Status::InvalidArgument
                },
                e.body_text(),
            ));
        }
    };
    if let Err(e) = origin(&headers).and_then(|_| prepare(&mut value, &state.store, &state.caveman))
    {
        return http_error(e);
    }
    forward(state, "/v1/chat/completions", value, permit).await
}
async fn embed(
    State(state): State<Proxy>,
    headers: HeaderMap,
    axum::Extension(permit): axum::Extension<Arc<tokio::sync::OwnedSemaphorePermit>>,
    payload: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> HttpResponse {
    let mut value = match payload {
        Ok(Json(value)) => value,
        Err(e) => {
            return http_error(err(
                if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    Status::ResourceLimit
                } else {
                    Status::InvalidArgument
                },
                e.body_text(),
            ));
        }
    };
    if let Err(e) = origin(&headers) {
        return http_error(e);
    }
    let Some(obj) = value.as_object_mut() else {
        return http_error(err(Status::InvalidArgument, "expected JSON object"));
    };
    obj.insert("model".into(), json!("ogma-local"));
    forward(state, "/v1/embeddings", value, permit).await
}
async fn forward(
    state: Proxy,
    path: &str,
    value: Value,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> HttpResponse {
    let response = match state
        .client
        .post(format!("{}{path}", state.backend))
        .bearer_auth(&state.key)
        .json(&value)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return http_error(transport(e)),
    };
    let status = response.status();
    let stream = value.get("stream").and_then(Value::as_bool) == Some(true) && status.is_success();
    if stream {
        let chunks = futures_util::stream::unfold(
            (response, 0usize, permit),
            |(mut response, size, permit)| async move {
                if size > BODY_LIMIT {
                    return None;
                }
                match response.chunk().await {
                    Ok(Some(chunk)) if size.saturating_add(chunk.len()) <= BODY_LIMIT => {
                        let next = size + chunk.len();
                        Some((Ok::<_, std::io::Error>(chunk), (response, next, permit)))
                    }
                    Ok(Some(_)) => Some((
                        Err(std::io::Error::other("model stream exceeds limit")),
                        (response, BODY_LIMIT + 1, permit),
                    )),
                    Ok(None) => None,
                    Err(e) => Some((
                        Err(std::io::Error::other(e)),
                        (response, BODY_LIMIT + 1, permit),
                    )),
                }
            },
        );
        (
            status,
            [
                ("content-type", "text/event-stream"),
                ("cache-control", "no-cache"),
            ],
            Body::from_stream(chunks),
        )
            .into_response()
    } else {
        let result = bounded(response, BODY_LIMIT).await;
        drop(permit);
        match result {
            Ok(b) => (status, [("content-type", "application/json")], b).into_response(),
            Err(e) => http_error(e),
        }
    }
}
struct Process {
    child: Child,
    log: Arc<Mutex<String>>,
}
impl Process {
    fn check(&mut self) -> Result<(), RuntimeError> {
        if let Some(status) = self.child.try_wait()? {
            return Err(err(
                Status::WorkerCrash,
                format!(
                    "llama-server exited ({status}); request was not replayed. {}",
                    self.log.lock().unwrap()
                ),
            ));
        }
        Ok(())
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn drain(mut input: impl Read + Send + 'static, log: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        let mut buffer = [0; 2048];
        while let Ok(n) = input.read(&mut buffer) {
            if n == 0 {
                break;
            }
            let mut text = log.lock().unwrap();
            text.push_str(&String::from_utf8_lossy(&buffer[..n]));
            if text.len() > 16384 {
                let mut cut = text.len() - 16384;
                while !text.is_char_boundary(cut) {
                    cut += 1;
                }
                text.drain(..cut);
            }
        }
    });
}
fn spawn(request: &Request, path: PathBuf, port: u16, key: &str) -> Result<Process, RuntimeError> {
    let exe = if request.model.server.is_empty() {
        std::env::current_exe()?
            .parent()
            .unwrap()
            .join("model/llama-server")
    } else {
        PathBuf::from(&request.model.server)
    };
    let mut command = Command::new(exe);
    command
        .env_clear()
        .args(["--model"])
        .arg(path)
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--alias",
            "ogma-local",
            "--api-key",
            key,
            "--offline",
            "--ctx-size",
            &request
                .model
                .ctx
                .saturating_mul(request.model.parallel)
                .to_string(),
            "--n-gpu-layers",
            &request.model.gpu_layers.to_string(),
            "--parallel",
            &request.model.parallel.to_string(),
            "--embedding",
            "--pooling",
            "mean",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                {
                    let parent = libc::getppid();
                    if parent == 1
                        || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
                        || libc::getppid() != parent
                    {
                        return Err(std::io::Error::other("model parent unavailable"));
                    }
                }
                Ok(())
            });
        }
    }
    let mut child = command.spawn()?;
    let log = Arc::new(Mutex::new(String::new()));
    drain(child.stdout.take().unwrap(), log.clone());
    drain(child.stderr.take().unwrap(), log.clone());
    Ok(Process { child, log })
}
pub struct ModelSession {
    runtime: tokio::runtime::Runtime,
    process: Option<Process>,
    proxy: Option<tokio::task::JoinHandle<()>>,
    options: ModelOptions,
    allowed: bool,
    timeout: u32,
    max_output: usize,
    history: Vec<Value>,
}
impl Drop for ModelSession {
    fn drop(&mut self) {
        if let Some(task) = self.proxy.take() {
            task.abort();
        }
        self.process.take();
    }
}
impl ModelSession {
    pub fn new(request: &Request) -> Result<Self, RuntimeError> {
        authorize(request.permissions.model)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let mut session = Self {
            runtime,
            process: None,
            proxy: None,
            options: request.model.clone(),
            allowed: true,
            timeout: request.limits.timeout_ms,
            max_output: request.limits.max_output_bytes as usize,
            history: Vec::new(),
        };
        if request.model.model.is_empty() {
            session
                .runtime
                .block_on(check_service(&session.options, session.timeout))?;
            return Ok(session);
        }
        let started = Instant::now();
        let public = std::net::TcpListener::bind(("127.0.0.1", request.model.port as u16))?;
        public.set_nonblocking(true)?;
        let path = session
            .runtime
            .block_on(model_store::resolve_model(request))?;
        let reservation = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let port = reservation.local_addr()?.port();
        let mut random = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        let key = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        drop(reservation);
        let process = spawn(request, path, port, &key)?;
        session.process = Some(process);
        let http = client(session.timeout)?;
        let backend = format!("http://127.0.0.1:{port}");
        loop {
            session.check()?;
            if started.elapsed() >= Duration::from_millis(session.timeout as u64) {
                return Err(err(Status::Timeout, "model startup timed out"));
            }
            let ready = session.runtime.block_on(async {
                http.get(format!("{backend}/health"))
                    .bearer_auth(&key)
                    .timeout(Duration::from_millis(500))
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success())
            });
            if ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let state = Proxy {
            client: http,
            backend,
            key,
            store: model_store::store_root(request)?,
            caveman: request.model.caveman.clone(),
            permits: Arc::new(Semaphore::new(request.model.parallel as usize)),
        };
        let _entered = session.runtime.enter();
        let listener = tokio::net::TcpListener::from_std(public)?;
        let app = Router::new()
            .route("/health", get(health))
            .route("/v1/models", get(models))
            .route("/v1/chat/completions", post(chat))
            .route("/v1/embeddings", post(embed))
            .route("/ogma/context/{hash}", get(recover))
            .layer(DefaultBodyLimit::max(BODY_LIMIT))
            .layer(axum::middleware::from_fn_with_state(
                (request.model.port, state.permits.clone()),
                ingress,
            ))
            .with_state(state);
        session.proxy = Some(tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        }));
        drop(_entered);
        Ok(session)
    }
    pub fn check(&mut self) -> Result<(), RuntimeError> {
        if let Some(p) = &mut self.process {
            p.check()?;
        }
        if self.proxy.as_ref().is_some_and(|task| task.is_finished()) {
            return Err(err(
                Status::WorkerCrash,
                "model proxy stopped; request was not replayed",
            ));
        }
        Ok(())
    }
    pub fn ready(&self, request: &Request) -> Response {
        success(
            request,
            format!("http://127.0.0.1:{}/v1", self.options.port),
        )
    }
    pub fn evaluate(&mut self, request: &Request) -> Response {
        let started = Instant::now();
        let result = self.check().and_then(|_| {
            if request.command == "model-status" {
                return Ok(String::new());
            }
            let mut messages = self.history.clone();
            messages.push(json!({"role":"user", "content":request.source}));
            if serde_json::to_vec(&messages).map_err(ioerr)?.len() > HISTORY_LIMIT
                || messages.len() > 512
            {
                return Err(err(
                    Status::ResourceLimit,
                    "conversation limit reached; start a new chat",
                ));
            }
            let context = if request.model.context.is_empty() {
                String::new()
            } else {
                crate::permissions::PermissionSet::new(&request.permissions)?
                    .read_text(std::path::Path::new(&request.model.context))?
            };
            let text = self.runtime.block_on(ask_messages(
                self.allowed,
                &self.options,
                self.timeout,
                messages.clone(),
                context,
            ))?;
            if text.len() > self.max_output {
                return Err(err(Status::ResourceLimit, "model output exceeds limit"));
            }
            if request.command == "model-chat" {
                messages.push(json!({"role":"assistant", "content":text}));
                self.history = messages;
            }
            Ok(text)
        });
        let mut response = match result {
            Ok(text) => success(request, text),
            Err(e) => e.response(request.id.clone(), request.entry.clone()),
        };
        if request.trace {
            response.trace.push(crate::generated::TraceEvent {
                name: request.command.clone(),
                elapsed_micros: started.elapsed().as_micros() as u64,
                count: 1,
            });
        }
        response
    }
}
fn success(request: &Request, text: String) -> Response {
    Response {
        protocol_version: PROTOCOL_VERSION,
        id: request.id.clone(),
        status: "ok".into(),
        output: if text.is_empty() {
            Vec::new()
        } else {
            vec![OutputEvent {
                stream: "stdout".into(),
                text,
            }]
        },
        ..Default::default()
    }
}
async fn check_service(options: &ModelOptions, timeout: u32) -> Result<(), RuntimeError> {
    let response = client(timeout)?
        .get(format!("http://127.0.0.1:{}/health", options.port))
        .send()
        .await
        .map_err(transport)?;
    if !response.status().is_success() {
        return Err(err(Status::WorkerCrash, "local model service unavailable"));
    }
    let value: Value = serde_json::from_slice(&bounded(response, 4096).await?).map_err(ioerr)?;
    if value.get("service").and_then(Value::as_str) != Some("ogma-model")
        || value.get("protocolVersion").and_then(Value::as_u64) != Some(PROTOCOL_VERSION as u64)
    {
        return Err(err(
            Status::Protocol,
            "endpoint is not a compatible Ogma model service",
        ));
    }
    Ok(())
}
async fn infer(
    allowed: bool,
    options: &ModelOptions,
    timeout: u32,
    path: &str,
    value: Value,
) -> Result<Value, RuntimeError> {
    authorize(allowed)?;
    if options.port == 0 || options.port > 65535 {
        return Err(err(Status::InvalidArgument, "invalid model port"));
    }
    if serde_json::to_vec(&value).map_err(ioerr)?.len() > BODY_LIMIT {
        return Err(err(Status::ResourceLimit, "model request exceeds limit"));
    }
    let response = client(timeout)?
        .post(format!("http://127.0.0.1:{}{path}", options.port))
        .json(&value)
        .send()
        .await
        .map_err(transport)?;
    let status = response.status();
    let bytes = bounded(response, BODY_LIMIT).await?;
    if !status.is_success() {
        return Err(err(
            match status {
                StatusCode::GATEWAY_TIMEOUT => Status::Timeout,
                StatusCode::BAD_GATEWAY => Status::WorkerCrash,
                StatusCode::TOO_MANY_REQUESTS | StatusCode::PAYLOAD_TOO_LARGE => {
                    Status::ResourceLimit
                }
                StatusCode::NOT_IMPLEMENTED | StatusCode::UNPROCESSABLE_ENTITY => {
                    Status::Unsupported
                }
                StatusCode::FORBIDDEN => Status::PermissionDenied,
                _ => Status::Execution,
            },
            format!(
                "model endpoint {status}: {}",
                crate::error::bounded_text(&String::from_utf8_lossy(&bytes), 4096)
            ),
        ));
    }
    serde_json::from_slice(&bytes).map_err(ioerr)
}
async fn ask_messages(
    allowed: bool,
    options: &ModelOptions,
    timeout: u32,
    messages: Vec<Value>,
    context: String,
) -> Result<String, RuntimeError> {
    let mut request = json!({"model":"ogma-local", "messages":messages, "stream":false});
    if !options.caveman.is_empty() {
        request["ogma_caveman"] = json!(options.caveman);
    }
    if !context.is_empty() {
        request["ogma_context"] = json!(context);
    }
    let value = infer(allowed, options, timeout, "/v1/chat/completions", request).await?;
    value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| err(Status::Execution, "model returned no assistant text"))
}
pub async fn ask(
    allowed: bool,
    options: ModelOptions,
    timeout: u32,
    prompt: String,
    context: String,
) -> Result<String, RuntimeError> {
    authorize(allowed)?;
    check_service(&options, timeout).await?;
    ask_messages(
        allowed,
        &options,
        timeout,
        vec![json!({"role":"user", "content":prompt})],
        context,
    )
    .await
}
pub async fn embedding(
    allowed: bool,
    options: ModelOptions,
    timeout: u32,
    text: String,
) -> Result<String, RuntimeError> {
    authorize(allowed)?;
    check_service(&options, timeout).await?;
    let value = infer(
        allowed,
        &options,
        timeout,
        "/v1/embeddings",
        json!({"model":"ogma-local", "input":text}),
    )
    .await?;
    let data = value
        .pointer("/data/0/embedding")
        .and_then(Value::as_array)
        .ok_or_else(|| err(Status::Execution, "model returned no embedding"))?;
    if data.is_empty()
        || data.len() > 1_048_576
        || data
            .iter()
            .any(|n| n.as_f64().is_none_or(|f| !f.is_finite()))
    {
        return Err(err(Status::Execution, "invalid model embedding"));
    }
    serde_json::to_string(data).map_err(ioerr)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prompts_stay_exact_and_tool_context_recovers() {
        let store =
            std::env::temp_dir().join(format!("ogma-model-prompt-{}", crate::cache::unique_id()));
        let prompt = "the original user's prompt\n<code>$foo</code>";
        let tool = "error: exact\n".repeat(4000);
        let mut value = json!({"messages":[{"role":"user","content":prompt},{"role":"tool","content":tool}],"ogma_caveman":"ultra"});
        prepare(&mut value, &store, "").unwrap();
        assert_eq!(value["messages"][1]["content"], prompt);
        assert_eq!(value["messages"][0]["content"], ULTRA);
        assert!(value["messages"][2]["content"].as_str().unwrap().len() < tool.len());
        assert_eq!(
            model_context::recover(&store, &crate::cache::digest(tool.as_bytes())).unwrap(),
            tool.as_bytes()
        );
        std::fs::remove_dir_all(store).unwrap();
    }
    #[tokio::test]
    async fn denied_before_any_socket() {
        let e = ask(
            false,
            ModelOptions::default(),
            10,
            "prompt".into(),
            "".into(),
        )
        .await
        .unwrap_err();
        assert_eq!(e.status, Status::PermissionDenied);
    }
}
