use crate::generated::{Diagnostic, PROTOCOL_VERSION, Response, Status};

#[derive(Debug, Clone)]
pub struct RuntimeError {
    pub status: Status,
    pub message: String,
    pub specifier: String,
    pub line: u32,
    pub column: u32,
}
impl RuntimeError {
    pub fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            specifier: String::new(),
            line: 0,
            column: 0,
        }
    }
    pub fn code(&self) -> &'static str {
        match self.status {
            Status::PermissionDenied => "OGMA_PERMISSION_DENIED",
            Status::Protocol => "OGMA_PROTOCOL",
            Status::InvalidArgument => "OGMA_INVALID_ARGUMENT",
            Status::Io => "OGMA_IO",
            Status::Panic => "OGMA_PANIC",
            Status::Timeout => "OGMA_TIMEOUT",
            Status::Cancelled => "OGMA_CANCELLED",
            Status::WorkerCrash => "OGMA_WORKER_CRASH",
            Status::Unsupported => "OGMA_UNSUPPORTED",
            Status::ResourceLimit => "OGMA_RESOURCE_LIMIT",
            _ => "OGMA_EXECUTION",
        }
    }
    pub fn response(&self, id: String, specifier: String) -> Response {
        Response {
            protocol_version: PROTOCOL_VERSION,
            id,
            status: "error".into(),
            diagnostics: vec![Diagnostic {
                code: self.code().into(),
                message: bounded_text(&self.message, 16384),
                specifier: if self.specifier.is_empty() {
                    bounded_text(&specifier, 32768)
                } else {
                    bounded_text(&self.specifier, 32768)
                },
                line: self.line,
                column: self.column,
            }],
            ..Default::default()
        }
    }
}
pub(crate) fn bounded_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [truncated]", &text[..end])
}
impl From<std::io::Error> for RuntimeError {
    fn from(error: std::io::Error) -> Self {
        Self::new(Status::Io, error.to_string())
    }
}
impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message)
    }
}
impl std::error::Error for RuntimeError {}
