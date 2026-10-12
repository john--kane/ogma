//! MCP metadata/input validation runs in the isolated worker. Schema references
//! are resolved offline: neither guest grants nor ambient access can fetch them.
use crate::{
    error::RuntimeError,
    generated::{McpTool, Status},
};
use serde_json::Value;

pub fn scaffold(
    request: &crate::generated::Request,
) -> Result<Vec<crate::generated::OutputEvent>, RuntimeError> {
    use std::{fs, io::Write, path::Path};
    let path = Path::new(&request.entry);
    let mut directory = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(path)?; // Existing directories and symlinks are never adopted.
    let files = [
        ("server.ts", include_str!("mcp_scaffold.ts")),
        ("ogma.d.ts", include_str!("../../../types/ogma.d.ts")),
        (
            "protocol.d.ts",
            include_str!("../../../types/protocol.d.ts"),
        ),
        ("protocol.js", include_str!("../../../types/protocol.js")),
        (
            "README.md",
            "# Ogma MCP server\n\nFrom this directory, run:\n\n```sh\nogma mcp serve server.ts --allow-read=.\n```\n\nRegister tools with Ogma.mcpTool. Handlers return text or Promise<string>.\nInput schemas are checked offline. Each discovery/call loads the module in a fresh isolated worker; keep top-level code limited to registration.\nClient arguments cannot elevate filesystem, environment or network grants.\nStdio carries newline-delimited JSON-RPC only; guest logs go to stderr.\nUse an absolute CLI/entry path and explicit read root in your MCP client config.\n",
        ),
    ];
    let mut created = Vec::new();
    let result: Result<(), std::io::Error> = (|| {
        for (name, content) in files {
            let target = path.join(name);
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&target)?;
            created.push(target);
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        for target in created {
            let _ = fs::remove_file(target);
        }
        let _ = fs::remove_dir(path);
        return Err(error.into());
    }
    Ok(vec![crate::generated::OutputEvent {
        stream: "stdout".into(),
        text: format!("Created MCP server at {}", path.display()),
    }])
}

pub fn tool(text: &str) -> Result<McpTool, RuntimeError> {
    if text.len() > 1024 * 1024 {
        return Err(RuntimeError::new(
            Status::ResourceLimit,
            "MCP definition exceeds 1 MiB",
        ));
    }
    let tool: McpTool = serde_json::from_str(text)
        .map_err(|e| RuntimeError::new(Status::InvalidArgument, e.to_string()))?;
    if tool.name.is_empty()
        || tool.name.len() > 128
        || !tool
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        || tool.description.len() > 16384
        || !tool.input_schema.is_object()
        || tool.input_schema.get("type").and_then(Value::as_str) != Some("object")
    {
        return Err(RuntimeError::new(
            Status::InvalidArgument,
            "invalid MCP tool name, description or object input schema",
        ));
    }
    jsonschema::options()
        .offline()
        .build(&tool.input_schema)
        .map_err(|e| {
            RuntimeError::new(
                Status::InvalidArgument,
                format!("invalid offline MCP input schema: {e}"),
            )
        })?;
    Ok(tool)
}

pub fn arguments(tool: &McpTool, text: &str) -> Result<(), RuntimeError> {
    if text.len() > 1024 * 1024 {
        return Err(RuntimeError::new(
            Status::ResourceLimit,
            "MCP arguments exceed 1 MiB",
        ));
    }
    let instance: Value = serde_json::from_str(text)
        .map_err(|e| RuntimeError::new(Status::InvalidArgument, e.to_string()))?;
    let validator = jsonschema::options()
        .offline()
        .build(&tool.input_schema)
        .map_err(|e| RuntimeError::new(Status::InvalidArgument, e.to_string()))?;
    validator.validate(&instance).map_err(|e| {
        RuntimeError::new(
            Status::InvalidArgument,
            format!("MCP arguments violate inputSchema: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schemas_are_validated_without_external_retrieval() {
        let valid = tool(r##"{"name":"echo","description":"Echo","inputSchema":{"type":"object","$defs":{"text":{"type":"string"}},"properties":{"text":{"$ref":"#/$defs/text"}},"required":["text"],"additionalProperties":false}}"##).unwrap();
        assert!(arguments(&valid, r#"{"text":"ok"}"#).is_ok());
        assert!(arguments(&valid, r#"{"text":42}"#).is_err());
        assert!(arguments(&valid, "{}").is_err());
        for reference in ["https://example.com/schema.json", "file:///etc/passwd"] {
            let value = serde_json::json!({"name":"bad","description":"", "inputSchema":{"type":"object", "$ref":reference}});
            assert!(tool(&value.to_string()).is_err());
        }
    }
}
