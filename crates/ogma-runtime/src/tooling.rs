//! Pinned tooling is deliberately off the guest execution path.
use crate::{
    error::RuntimeError,
    generated::{MAX_FRAME_BYTES, Request, Response, Status},
};
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
};

fn tool_directory() -> Result<PathBuf, RuntimeError> {
    let executable = std::env::current_exe()?;
    let directory = executable
        .parent()
        .ok_or_else(|| RuntimeError::new(Status::Io, "worker has no directory"))?
        .join("tooling");
    if !directory.join("adapter.mjs").is_file() || !directory.join("node").is_file() {
        return Err(RuntimeError::new(
            Status::Unsupported,
            "pinned tool adapter missing; run tools/build.py",
        ));
    }
    Ok(directory)
}

fn biome(request: &Request, directory: &std::path::Path) -> Result<Response, RuntimeError> {
    let platform = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let binary = directory.join(format!("node_modules/@biomejs/cli-{platform}-{arch}/biome"));
    let mut command = Command::new(binary);
    command
        .env_clear()
        .arg(if request.command == "fmt" {
            "format"
        } else {
            "lint"
        })
        .args(["--reporter=json", "--colors=off", "--max-diagnostics=200"]);
    if request.tool.write {
        command.arg("--write");
    }
    let target = PathBuf::from(&request.entry);
    let target = if target.is_absolute() {
        target
    } else {
        std::env::current_dir()?.join(target)
    };
    command
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (bytes, stderr, status) = capture(command, None)?;
    let report: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
        RuntimeError::new(
            Status::Execution,
            String::from_utf8_lossy(&stderr).into_owned(),
        )
    })?;
    let mut response = Response {
        protocol_version: request.protocol_version,
        id: request.id.clone(),
        status: "ok".into(),
        ..Default::default()
    };
    if let Some(diagnostics) = report["diagnostics"].as_array() {
        for item in diagnostics.iter().take(200) {
            let message = format!(
                "{}: {}",
                item["category"].as_str().unwrap_or("biome"),
                item["message"].as_str().unwrap_or("diagnostic")
            );
            if matches!(item["severity"].as_str(), Some("error" | "fatal")) {
                response.status = "error".into();
                response.diagnostics.push(crate::generated::Diagnostic {
                    code: if request.command == "fmt" {
                        "OGMA_FORMAT"
                    } else {
                        "OGMA_LINT"
                    }
                    .into(),
                    message: crate::error::bounded_text(&message, 4096),
                    specifier: crate::error::bounded_text(
                        item["location"]["path"].as_str().unwrap_or(""),
                        32768,
                    ),
                    line: item["location"]["start"]["line"].as_u64().unwrap_or(0) as u32,
                    column: item["location"]["start"]["column"].as_u64().unwrap_or(0) as u32,
                });
            }
        }
    }
    if !status.success() && response.diagnostics.is_empty() {
        return Err(RuntimeError::new(
            Status::Execution,
            String::from_utf8_lossy(&stderr).into_owned(),
        ));
    }
    if response.status == "ok" {
        response.output.push(crate::generated::OutputEvent {
            stream: "stdout".into(),
            text: format!(
                "{}: {} changed, {} unchanged",
                request.command, report["summary"]["changed"], report["summary"]["unchanged"]
            ),
        });
    }
    Ok(response)
}

fn capture(
    mut command: Command,
    input: Option<Vec<u8>>,
) -> Result<(Vec<u8>, Vec<u8>, std::process::ExitStatus), RuntimeError> {
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(MAX_FRAME_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let err = std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut stream = stderr;
        let mut buffer = [0; 4096];
        loop {
            let n = stream.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            if kept.len() < 16384 {
                kept.extend_from_slice(&buffer[..n.min(16384 - kept.len())]);
            }
        }
        Ok::<_, std::io::Error>(kept)
    });
    let write = match input {
        Some(bytes) => child.stdin.take().unwrap().write_all(&bytes),
        None => Ok(()),
    };
    if write.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let bytes = out
        .join()
        .map_err(|_| RuntimeError::new(Status::Panic, "tool output reader panicked"))??;
    let stderr = err
        .join()
        .map_err(|_| RuntimeError::new(Status::Panic, "tool error reader panicked"))??;
    write?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(RuntimeError::new(
            Status::ResourceLimit,
            "tool response exceeds frame limit",
        ));
    }
    Ok((bytes, stderr, status))
}

pub fn execute(request: &Request) -> Result<Response, RuntimeError> {
    let directory = tool_directory()?;
    if matches!(request.command.as_str(), "fmt" | "lint") {
        return biome(request, &directory);
    }
    let mut command = Command::new(directory.join("node"));
    command
        .arg(format!(
            "--max-old-space-size={}",
            request.limits.max_memory_mb * 3 / 4
        ))
        .arg(directory.join("adapter.mjs"))
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let input = serde_json::to_vec(request)
        .map_err(|e| RuntimeError::new(Status::Protocol, e.to_string()))?;
    let (bytes, stderr, status) = capture(command, Some(input))?;
    if !status.success() {
        return Err(RuntimeError::new(
            Status::Execution,
            format!(
                "tool helper exited {status}: {}",
                String::from_utf8_lossy(&stderr)
            ),
        ));
    }
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(RuntimeError::new(
            Status::ResourceLimit,
            "tool response exceeds frame limit",
        ));
    }
    let response: Response = serde_json::from_slice(&bytes)
        .map_err(|e| RuntimeError::new(Status::Protocol, format!("invalid tool response: {e}")))?;
    if response.id != request.id
        || response.protocol_version != request.protocol_version
        || !matches!(response.status.as_str(), "ok" | "error")
    {
        return Err(RuntimeError::new(
            Status::Protocol,
            "tool response identity mismatch",
        ));
    }
    Ok(response)
}
