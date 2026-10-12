#[rustfmt::skip]
pub mod generated;
pub mod abi;
pub mod cache;
pub mod engine;
pub mod error;
pub mod loader;
pub mod mcp;
pub mod model;
pub mod model_context;
pub mod model_store;
pub mod npm;
pub mod permissions;
pub mod protocol;
pub mod sandbox;
pub mod tooling;

pub fn execute_bytes(bytes: &[u8]) -> generated::Response {
    match protocol::parse_request(bytes) {
        Ok(request)
            if !request.sandbox.is_empty()
                || request.command == "repl"
                || request.permissions.model
                || request.command.starts_with("model-") =>
        {
            error::RuntimeError::new(
                generated::Status::Unsupported,
                "sandbox and persistent sessions require an isolated worker",
            )
            .response(request.id, request.entry)
        }
        Ok(request) => execute_request(request),
        Err(error) => error.response(String::new(), String::new()),
    }
}

pub fn execute_request(request: generated::Request) -> generated::Response {
    use generated::{PROTOCOL_VERSION, Response, TraceEvent};
    let started = std::time::Instant::now();
    let result = match request.command.as_str() {
        "fmt" | "lint" | "bundle" | "check" => tooling::execute(&request),
        "mcp-init" => mcp::scaffold(&request).map(|output| Response {
            protocol_version: PROTOCOL_VERSION,
            id: request.id.clone(),
            status: "ok".into(),
            output,
            ..Default::default()
        }),
        "install" => npm::install(&request).map(|output| Response {
            protocol_version: PROTOCOL_VERSION,
            id: request.id.clone(),
            status: "ok".into(),
            output,
            ..Default::default()
        }),
        _ => return engine::execute(request),
    };
    let mut response =
        result.unwrap_or_else(|error| error.response(request.id.clone(), request.entry.clone()));
    if request.trace {
        response.trace.push(TraceEvent {
            name: format!("tool:{}", request.command),
            elapsed_micros: started.elapsed().as_micros() as u64,
            count: 0,
        });
    }
    response
}
