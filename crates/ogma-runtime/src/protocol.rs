use crate::{
    error::RuntimeError,
    generated::{MAX_FRAME_BYTES, PROTOCOL_VERSION, Request, Response, Status},
};
use std::io::{Read, Write};

pub fn read_frame(reader: &mut impl Read) -> Result<Vec<u8>, RuntimeError> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let size = u32::from_le_bytes(header) as usize;
    if size == 0 || size > MAX_FRAME_BYTES {
        return Err(RuntimeError::new(Status::Protocol, "invalid frame length"));
    }
    let mut payload = vec![0; size];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

pub fn parse_request(bytes: &[u8]) -> Result<Request, RuntimeError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(RuntimeError::new(
            Status::Protocol,
            "request exceeds frame limit",
        ));
    }
    let request: Request = serde_json::from_slice(bytes)
        .map_err(|e| RuntimeError::new(Status::Protocol, e.to_string()))?;
    if request.protocol_version != PROTOCOL_VERSION {
        return Err(RuntimeError::new(
            Status::Protocol,
            "unsupported protocol version",
        ));
    }
    if !matches!(
        request.command.as_str(),
        "run"
            | "mcp-init"
            | "mcp-list"
            | "mcp-call"
            | "eval"
            | "test"
            | "repl"
            | "fmt"
            | "lint"
            | "bundle"
            | "check"
            | "install"
            | "model-run"
            | "model-chat"
            | "model-ask"
            | "model-status"
    ) {
        return Err(RuntimeError::new(
            Status::Unsupported,
            "unsupported worker command",
        ));
    }
    if request.id.is_empty() || request.id.len() > 128 || request.entry.len() > 32768 {
        return Err(RuntimeError::new(
            Status::InvalidArgument,
            "invalid request identity or entry",
        ));
    }
    if !matches!(request.sandbox.as_str(), "" | "linux") {
        return Err(RuntimeError::new(
            Status::Unsupported,
            "unsupported sandbox",
        ));
    }
    if !request.sandbox.is_empty()
        && matches!(
            request.command.as_str(),
            "fmt" | "lint" | "bundle" | "check" | "install" | "mcp-init"
        )
    {
        return Err(RuntimeError::new(
            Status::Unsupported,
            "sandbox is restricted to guest execution",
        ));
    }
    if request.command.starts_with("model-") || request.permissions.model {
        crate::model::authorize(request.permissions.model)?;
        let model = &request.model;
        if !(1..=1_048_576).contains(&model.ctx)
            || !(1..=1024).contains(&model.parallel)
            || model
                .ctx
                .checked_mul(model.parallel)
                .is_none_or(|n| n > 1_048_576)
            || model.gpu_layers < -1
            || !(1..=65535).contains(&model.port)
            || !matches!(model.caveman.as_str(), "" | "compact" | "ultra")
            || [&model.model, &model.server, &model.store, &model.context]
                .iter()
                .any(|s| s.len() > 32768 || s.contains('\0'))
        {
            return Err(RuntimeError::new(
                Status::InvalidArgument,
                "model options are out of range",
            ));
        }
        if !request.sandbox.is_empty() {
            return Err(RuntimeError::new(
                Status::Unsupported,
                "model hosting requires an isolated worker outside the guest sandbox prototype",
            ));
        }
        if request.command == "model-run" && model.model.is_empty() {
            return Err(RuntimeError::new(
                Status::InvalidArgument,
                "model run requires a model",
            ));
        }
    }
    if request.command == "mcp-call"
        && (request.mcp.name.is_empty()
            || request.mcp.name.len() > 128
            || !request.mcp.arguments.is_object())
    {
        return Err(RuntimeError::new(
            Status::InvalidArgument,
            "invalid MCP invocation",
        ));
    }
    let limits = &request.limits;
    if !(1..=600_000).contains(&limits.timeout_ms)
        || !(16..=if request.permissions.model {
            65536
        } else {
            4096
        })
            .contains(&limits.max_memory_mb)
        || !(1..=1_048_576).contains(&limits.max_output_bytes)
    {
        return Err(RuntimeError::new(
            Status::InvalidArgument,
            "resource limits are out of range",
        ));
    }
    if request.args.len() > 1024 || request.args.iter().any(|s| s.len() > 32768) {
        return Err(RuntimeError::new(
            Status::InvalidArgument,
            "arguments exceed limit",
        ));
    }
    Ok(request)
}

pub fn write_response(writer: &mut impl Write, response: &Response) -> Result<(), RuntimeError> {
    let mut bytes = serde_json::to_vec(response)
        .map_err(|e| RuntimeError::new(Status::Protocol, e.to_string()))?;
    if bytes.len() > MAX_FRAME_BYTES {
        let failure = RuntimeError::new(Status::ResourceLimit, "response exceeds frame limit")
            .response(response.id.clone(), String::new());
        bytes = serde_json::to_vec(&failure)
            .map_err(|e| RuntimeError::new(Status::Protocol, e.to_string()))?;
    }
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_oversize_without_allocating_payload() {
        let header = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes();
        assert_eq!(
            read_frame(&mut header.as_slice()).unwrap_err().status,
            Status::Protocol
        );
    }
    #[test]
    fn truncated_frame_fails() {
        assert!(read_frame(&mut [10, 0, 0, 0, 1].as_slice()).is_err());
    }
    #[test]
    fn oversized_reply_returns_resource_diagnostic_instead_of_truncated_frame() {
        let response = Response {
            protocol_version: PROTOCOL_VERSION,
            id: "large".into(),
            status: "ok".into(),
            output: vec![crate::generated::OutputEvent {
                stream: "stdout".into(),
                text: "x".repeat(MAX_FRAME_BYTES),
            }],
            ..Default::default()
        };
        let mut bytes = Vec::new();
        write_response(&mut bytes, &response).unwrap();
        let payload = read_frame(&mut bytes.as_slice()).unwrap();
        let returned: Response = serde_json::from_slice(&payload).unwrap();
        assert_eq!(returned.id, "large");
        assert_eq!(returned.diagnostics[0].code, "OGMA_RESOURCE_LIMIT");
        assert!(returned.output.is_empty());
    }
    #[test]
    fn strict_contract_rejects_unknown_fields_and_versions() {
        let mut request = Request {
            protocol_version: PROTOCOL_VERSION,
            id: "x".into(),
            command: "run".into(),
            limits: crate::generated::Limits {
                timeout_ms: 1000,
                max_memory_mb: 64,
                max_output_bytes: 1000,
            },
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&request).unwrap();
        assert!(parse_request(&bytes).is_ok());
        let mut value = serde_json::to_value(&request).unwrap();
        value["allowAll"] = true.into();
        assert_eq!(
            parse_request(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .status,
            Status::Protocol
        );
        request.protocol_version = 9;
        assert_eq!(
            parse_request(&serde_json::to_vec(&request).unwrap())
                .unwrap_err()
                .status,
            Status::Protocol
        );
    }
}
