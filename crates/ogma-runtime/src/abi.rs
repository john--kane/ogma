use crate::generated::*;
use std::{
    collections::HashMap,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

// Handles contain only serialized responses. JSC contexts never cross threads or
// live across exports. Caller memory is borrowed only for the duration of a call.
struct Session {
    response: Vec<u8>,
    executed: bool,
}
static HANDLES: OnceLock<Mutex<HashMap<u64, Session>>> = OnceLock::new();
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
fn handles() -> &'static Mutex<HashMap<u64, Session>> {
    HANDLES.get_or_init(Default::default)
}
fn boundary(action: impl FnOnce() -> Status) -> Status {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)).unwrap_or(Status::Panic)
}

#[unsafe(no_mangle)]
pub extern "C" fn ogma_abi_version() -> u32 {
    ABI_VERSION
}

/// # Safety
/// `handle` must point to writable aligned u64 storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ogma_create(version: u32, handle: *mut u64) -> Status {
    boundary(|| {
        if handle.is_null() {
            return Status::InvalidArgument;
        }
        unsafe {
            *handle = 0;
        }
        if version != ABI_VERSION {
            return Status::Protocol;
        }
        let Ok(mut sessions) = handles().lock() else {
            return Status::Panic;
        };
        if sessions.len() >= 1024 {
            return Status::ResourceLimit;
        }
        let id = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            return Status::ResourceLimit;
        }
        sessions.insert(
            id,
            Session {
                response: Vec::new(),
                executed: false,
            },
        );
        unsafe {
            *handle = id;
        }
        Status::Ok
    })
}
/// # Safety
/// `request` must point to `length` readable bytes. Trusted callers only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ogma_execute(handle: u64, request: *mut u8, length: usize) -> Status {
    boundary(|| {
        if request.is_null() || length == 0 || length > MAX_FRAME_BYTES {
            return Status::InvalidArgument;
        }
        // Hold the registry lock for execution: destroy/response cannot race use,
        // and one request per handle prevents accidental replay.
        let Ok(mut sessions) = handles().lock() else {
            return Status::Panic;
        };
        let Some(session) = sessions.get_mut(&handle) else {
            return Status::InvalidHandle;
        };
        if session.executed {
            return Status::InvalidArgument;
        }
        session.executed = true;
        let bytes = unsafe { std::slice::from_raw_parts(request, length) };
        // Catch while the registry guard is alive so execution panics cannot
        // poison it. A failed handle stays consumed; nothing is replayed.
        let response = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::execute_bytes(bytes)
        })) {
            Ok(response) => response,
            Err(_) => return Status::Panic,
        };
        match serde_json::to_vec(&response) {
            Ok(bytes) if bytes.len() <= MAX_FRAME_BYTES => {
                session.response = bytes;
                Status::Ok
            }
            _ => Status::ResourceLimit,
        }
    })
}
/// # Safety
/// `written` must point to writable usize storage. A non-null `buffer` must
/// provide `capacity` writable bytes and must not overlap `written`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ogma_response(
    handle: u64,
    buffer: *mut u8,
    capacity: usize,
    written: *mut usize,
) -> Status {
    boundary(|| {
        if written.is_null() {
            return Status::InvalidArgument;
        }
        unsafe {
            *written = 0;
        }
        let Ok(sessions) = handles().lock() else {
            return Status::Panic;
        };
        let Some(session) = sessions.get(&handle) else {
            return Status::InvalidHandle;
        };
        if !session.executed {
            return Status::InvalidArgument;
        }
        unsafe {
            *written = session.response.len();
        }
        if buffer.is_null() || capacity < session.response.len() {
            return Status::BufferTooSmall;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                session.response.as_ptr(),
                buffer,
                session.response.len(),
            );
        }
        Status::Ok
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn ogma_destroy(handle: u64) -> Status {
    boundary(|| {
        let Ok(mut sessions) = handles().lock() else {
            return Status::Panic;
        };
        if sessions.remove(&handle).is_some() {
            Status::Ok
        } else {
            Status::InvalidHandle
        }
    })
}

// Type-check every export against the generated shared ABI.
const _: OgmaAbiVersionFn = ogma_abi_version;
const _: OgmaCreateFn = ogma_create;
const _: OgmaExecuteFn = ogma_execute;
const _: OgmaResponseFn = ogma_response;
const _: OgmaDestroyFn = ogma_destroy;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_bad_version_null_and_stale_handles() {
        let mut handle = 0;
        assert_eq!(unsafe { ogma_create(99, &mut handle) }, Status::Protocol);
        assert_eq!(handle, 0);
        assert_eq!(
            unsafe { ogma_create(ABI_VERSION, std::ptr::null_mut()) },
            Status::InvalidArgument
        );
        assert_eq!(unsafe { ogma_create(ABI_VERSION, &mut handle) }, Status::Ok);
        assert_eq!(ogma_destroy(handle), Status::Ok);
        assert_eq!(ogma_destroy(handle), Status::InvalidHandle);
    }
    #[test]
    fn embedded_nul_executes_through_trusted_boundary() {
        let request = Request {
            protocol_version: PROTOCOL_VERSION,
            id: "nul".into(),
            command: "eval".into(),
            source: "//\0\nconsole.log('a\0b');".into(),
            limits: Limits {
                timeout_ms: 1000,
                max_memory_mb: 64,
                max_output_bytes: 1000,
            },
            ..Default::default()
        };
        let mut bytes = serde_json::to_vec(&request).unwrap();
        let mut handle = 0;
        assert_eq!(unsafe { ogma_create(ABI_VERSION, &mut handle) }, Status::Ok);
        assert_eq!(
            unsafe { ogma_execute(handle, bytes.as_mut_ptr(), bytes.len()) },
            Status::Ok
        );
        assert_eq!(
            unsafe { ogma_execute(handle, bytes.as_mut_ptr(), bytes.len()) },
            Status::InvalidArgument
        );
        let mut length = 0;
        assert_eq!(
            unsafe { ogma_response(handle, std::ptr::null_mut(), 0, &mut length) },
            Status::BufferTooSmall
        );
        let mut response = vec![0; length];
        assert_eq!(
            unsafe { ogma_response(handle, response.as_mut_ptr(), length, &mut length) },
            Status::Ok
        );
        let response: Response = serde_json::from_slice(&response).unwrap();
        assert_eq!(response.status, "ok", "{:?}", response.diagnostics);
        assert_eq!(response.output[0].text, "a\0b");
        assert_eq!(ogma_destroy(handle), Status::Ok);
    }
    #[test]
    fn panic_is_contained() {
        assert_eq!(boundary(|| panic!("fault")), Status::Panic);
    }
    #[test]
    fn trusted_request_cannot_claim_isolation() {
        let request = Request {
            protocol_version: PROTOCOL_VERSION,
            id: "sandbox".into(),
            command: "eval".into(),
            source: "console.log('must not execute');".into(),
            sandbox: "linux".into(),
            limits: Limits {
                timeout_ms: 1000,
                max_memory_mb: 64,
                max_output_bytes: 1000,
            },
            ..Default::default()
        };
        let response = crate::execute_bytes(&serde_json::to_vec(&request).unwrap());
        assert_eq!(response.diagnostics[0].code, "OGMA_UNSUPPORTED");
        assert!(response.output.is_empty());
    }
}
