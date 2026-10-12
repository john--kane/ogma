use ogma_runtime::{error::RuntimeError, generated::Status, protocol};
use std::io::Read;

fn main() {
    // Panic diagnostics go to stderr; stdout is exclusively framed JSON.
    let result = std::panic::catch_unwind(worker);
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
        Err(_) => {
            eprintln!("OGMA_PANIC: worker panicked; request was not replayed");
            std::process::exit(3);
        }
    }
}
fn worker() -> Result<(), RuntimeError> {
    let input = std::io::stdin();
    let output = std::io::stdout();
    let mut input = input.lock();
    let mut output = output.lock();
    let bytes = protocol::read_frame(&mut input)?;
    let request = match protocol::parse_request(&bytes) {
        Ok(request) => request,
        Err(error) => {
            return protocol::write_response(
                &mut output,
                &error.response(String::new(), String::new()),
            );
        }
    };
    // Persistent sessions use a per-evaluation JSC watchdog and external memory
    // supervision; a process-lifetime CPU deadline would incorrectly expire idle
    // sessions. All workers retain core/descriptor/no_new_privs controls.
    apply_limits(
        if request.command == "repl" || request.command.starts_with("model-") {
            0
        } else {
            request.limits.timeout_ms
        },
    )?;
    ogma_runtime::sandbox::apply(&request)?;
    if request.command.starts_with("model-") {
        return model_worker(request, &mut input, &mut output);
    }
    if request.command != "repl" {
        return protocol::write_response(&mut output, &ogma_runtime::execute_request(request));
    }
    let mut session = ogma_runtime::engine::ReplSession::new(&request)?;
    let permissions = serde_json::to_value(&request.permissions).unwrap();
    let limits = serde_json::to_value(&request.limits).unwrap();
    let sandbox = request.sandbox.clone();
    let cache = request.cache_directory.clone();
    let args = request.args.clone();
    let tool = serde_json::to_value(&request.tool).unwrap();
    let model = serde_json::to_value(&request.model).unwrap();
    let mut current = request;
    loop {
        let response = session.evaluate(&current);
        protocol::write_response(&mut output, &response)?;
        if response.diagnostics.iter().any(|d| {
            matches!(
                d.code.as_str(),
                "OGMA_TIMEOUT" | "OGMA_RESOURCE_LIMIT" | "OGMA_PANIC"
            )
        }) {
            return Ok(());
        }
        // EOF at a frame boundary is normal session termination; truncated frames
        // remain protocol failures. Ordinary workers never enter this loop.
        use std::io::Read;
        let mut first = [0u8; 1];
        if input.read(&mut first)? == 0 {
            return Ok(());
        }
        let mut joined = first.as_slice().chain(&mut input);
        let bytes = protocol::read_frame(&mut joined)?;
        current = protocol::parse_request(&bytes)?;
        if current.command != "repl"
            || serde_json::to_value(&current.permissions).unwrap() != permissions
            || serde_json::to_value(&current.limits).unwrap() != limits
            || current.sandbox != sandbox
            || current.cache_directory != cache
            || current.args != args
            || serde_json::to_value(&current.tool).unwrap() != tool
            || serde_json::to_value(&current.model).unwrap() != model
        {
            return Err(RuntimeError::new(
                Status::Protocol,
                "REPL execution policy cannot change during a session",
            ));
        }
    }
}
fn model_worker(
    request: ogma_runtime::generated::Request,
    input: &mut impl std::io::Read,
    output: &mut impl std::io::Write,
) -> Result<(), RuntimeError> {
    let started = std::time::Instant::now();
    let mut session = match ogma_runtime::model::ModelSession::new(&request) {
        Ok(session) => session,
        Err(error) => {
            return protocol::write_response(output, &error.response(request.id, request.entry));
        }
    };
    if request.command == "model-ask" {
        return protocol::write_response(output, &session.evaluate(&request));
    }
    let mut policy = serde_json::to_value(&request).unwrap();
    for field in ["id", "source", "command"] {
        policy.as_object_mut().unwrap().remove(field);
    }
    let mut ready = session.ready(&request);
    if request.trace {
        ready.trace.push(ogma_runtime::generated::TraceEvent {
            name: "model-startup".into(),
            elapsed_micros: started.elapsed().as_micros() as u64,
            count: 1,
        });
    }
    protocol::write_response(output, &ready)?;
    loop {
        #[cfg(unix)]
        loop {
            session.check()?;
            let mut descriptor = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut descriptor, 1, 100) };
            if result > 0 {
                break;
            }
            if result < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        let mut first = [0u8; 1];
        if input.read(&mut first)? == 0 {
            return Ok(());
        }
        let mut joined = first.as_slice().chain(&mut *input);
        let current = match protocol::parse_request(&protocol::read_frame(&mut joined)?) {
            Ok(request) => request,
            Err(error) => {
                return protocol::write_response(
                    output,
                    &error.response(String::new(), String::new()),
                );
            }
        };
        let mut candidate = serde_json::to_value(&current).unwrap();
        for field in ["id", "source", "command"] {
            candidate.as_object_mut().unwrap().remove(field);
        }
        if candidate != policy
            || !(current.command == "model-status"
                || (request.command == "model-chat" && current.command == "model-chat"))
        {
            return protocol::write_response(
                output,
                &RuntimeError::new(
                    Status::Protocol,
                    "model execution policy cannot change during a session",
                )
                .response(current.id, current.entry),
            );
        }
        let response = session.evaluate(&current);
        let failed = response.status != "ok";
        protocol::write_response(output, &response)?;
        if failed {
            return Ok(());
        }
    }
}
fn apply_limits(timeout_ms: u32) -> Result<(), RuntimeError> {
    #[cfg(unix)]
    unsafe {
        // V8-style virtual-address caps are inappropriate for a JIT; supervisor
        // bounds resident memory instead. CPU and core limits are enforced by OS.
        let seconds = (timeout_ms as u64).div_ceil(1000) + 1;
        for (resource, limit) in [
            (libc::RLIMIT_CPU, seconds),
            (libc::RLIMIT_CORE, 0),
            (libc::RLIMIT_NOFILE, 256),
        ] {
            let value = libc::rlimit {
                rlim_cur: limit as libc::rlim_t,
                rlim_max: limit as libc::rlim_t,
            };
            if resource == libc::RLIMIT_CPU && timeout_ms == 0 {
                continue;
            }
            if libc::setrlimit(resource, &value) != 0 {
                return Err(RuntimeError::new(
                    Status::Io,
                    std::io::Error::last_os_error().to_string(),
                ));
            }
        }
    }
    #[cfg(target_os = "linux")]
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(RuntimeError::new(
                Status::Io,
                std::io::Error::last_os_error().to_string(),
            ));
        }
    }
    Ok(())
}
