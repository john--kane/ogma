//! Optional Linux prototype: selective bubblewrap mounts + PID/network namespaces
//! installed by the frontend, and a thread-compatible seccomp filter here.
//! This is not a complete hard sandbox or a resource/cgroup boundary.
use crate::{
    error::RuntimeError,
    generated::{Request, Status},
};

fn unavailable(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        Status::Unsupported,
        format!("Linux sandbox unavailable: {}", message.into()),
    )
}

pub fn apply(request: &Request) -> Result<(), RuntimeError> {
    match request.sandbox.as_str() {
        "" => Ok(()),
        "linux" => {
            if !matches!(request.command.as_str(), "run" | "eval" | "test" | "repl") {
                return Err(unavailable(
                    "tooling and installation require separate processes",
                ));
            }
            apply_linux()
        }
        _ => Err(RuntimeError::new(
            Status::InvalidArgument,
            "unknown sandbox mode",
        )),
    }
}

#[cfg(not(target_os = "linux"))]
fn apply_linux() -> Result<(), RuntimeError> {
    Err(unavailable("only Linux is supported"))
}

#[cfg(target_os = "linux")]
fn apply_linux() -> Result<(), RuntimeError> {
    // --as-pid-1 and a private tmpfs root are observable kernel state. Never
    // accept an environment variable as evidence that the launcher isolated us.
    let mut root = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::getpid() } != 1
        || unsafe { libc::statfs(c"/".as_ptr(), root.as_mut_ptr()) } != 0
    {
        return Err(unavailable(
            "worker must be PID 1 inside the namespace launcher",
        ));
    }
    if unsafe { root.assume_init() }.f_type != libc::TMPFS_MAGIC {
        return Err(unavailable("worker requires a private tmpfs root"));
    }
    install_filter()
}

#[cfg(target_os = "linux")]
fn install_filter() -> Result<(), RuntimeError> {
    use libc::{sock_filter, sock_fprog};
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000003e;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc00000b7;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return Err(unavailable("unsupported seccomp architecture"));
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        // seccomp_data offsets are stable Linux UAPI: nr=0, arch=4, args[0]=16.
        let instruction = |code, jt, jf, k| sock_filter { code, jt, jf, k };
        const LOAD: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
        const EQ: u16 = 0x15;
        const SET: u16 = 0x45;
        const GE: u16 = 0x35;
        const RETURN: u16 = 0x06;
        const ALLOW: u32 = 0x7fff0000;
        const ERRNO: u32 = 0x00050000;
        const KILL: u32 = 0x80000000;
        let denied = ERRNO | libc::EPERM as u32;
        let mut filter = vec![
            instruction(LOAD, 0, 0, 4),
            instruction(EQ, 1, 0, ARCH),
            instruction(RETURN, 0, 0, KILL),
            instruction(LOAD, 0, 0, 0),
            // Reject the x32 syscall namespace as well as unknown large numbers.
            instruction(GE, 0, 1, 0x40000000),
            instruction(RETURN, 0, 0, denied),
            // JSC and Tokio need threads; process creation remains denied.
            instruction(EQ, 0, 4, libc::SYS_clone as u32),
            instruction(LOAD, 0, 0, 16),
            instruction(SET, 1, 0, libc::CLONE_THREAD as u32),
            instruction(RETURN, 0, 0, denied),
            instruction(RETURN, 0, 0, ALLOW),
            // glibc retries thread creation with clone when clone3 is ENOSYS.
            instruction(EQ, 0, 1, libc::SYS_clone3 as u32),
            instruction(RETURN, 0, 0, ERRNO | libc::ENOSYS as u32),
            // Tokio's signal driver uses anonymous Unix socket pairs. They
            // cannot connect to host endpoints; socket/connect remain denied.
            instruction(EQ, 0, 4, libc::SYS_socketpair as u32),
            instruction(LOAD, 0, 0, 16),
            instruction(EQ, 1, 0, libc::AF_UNIX as u32),
            instruction(RETURN, 0, 0, denied),
            instruction(RETURN, 0, 0, ALLOW),
        ];
        let mut denied_calls = vec![
            libc::SYS_socket,
            libc::SYS_connect,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_accept,
            libc::SYS_accept4,
            libc::SYS_execve,
            libc::SYS_execveat,
            libc::SYS_unshare,
            libc::SYS_setns,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_pivot_root,
            libc::SYS_chroot,
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_kill,
            libc::SYS_tkill,
            libc::SYS_bpf,
            libc::SYS_userfaultfd,
            libc::SYS_perf_event_open,
            libc::SYS_keyctl,
            libc::SYS_add_key,
            libc::SYS_request_key,
            libc::SYS_reboot,
            libc::SYS_kexec_load,
            libc::SYS_open_by_handle_at,
            libc::SYS_capset,
            libc::SYS_setuid,
            libc::SYS_setgid,
            libc::SYS_setresuid,
            libc::SYS_setresgid,
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ];
        #[cfg(target_arch = "x86_64")]
        denied_calls.extend([libc::SYS_fork, libc::SYS_vfork]);
        for syscall in denied_calls {
            filter.push(instruction(EQ, 0, 1, syscall as u32));
            filter.push(instruction(RETURN, 0, 0, denied));
        }
        // Permit watchdog thread signals within this worker, never another PID.
        for syscall in [libc::SYS_tgkill, libc::SYS_rt_tgsigqueueinfo] {
            filter.extend([
                instruction(EQ, 0, 4, syscall as u32),
                instruction(LOAD, 0, 0, 16),
                instruction(EQ, 1, 0, unsafe { libc::getpid() } as u32),
                instruction(RETURN, 0, 0, denied),
                instruction(RETURN, 0, 0, ALLOW),
            ]);
        }
        filter.push(instruction(RETURN, 0, 0, ALLOW));
        let program = sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_mut_ptr(),
        };
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
            || unsafe {
                libc::syscall(
                    libc::SYS_seccomp,
                    libc::SECCOMP_SET_MODE_FILTER,
                    libc::SECCOMP_FILTER_FLAG_TSYNC,
                    &program,
                )
            } != 0
        {
            return Err(unavailable(std::io::Error::last_os_error().to_string()));
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[test]
    fn namespace_missing_fails_closed() {
        if unsafe { libc::getpid() } != 1 {
            assert_eq!(apply_linux().unwrap_err().status, Status::Unsupported);
        }
    }
    #[test]
    fn filter_blocks_network_processes_and_exec_but_allows_threads() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sandbox::tests::filter_probe",
                "--ignored",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    #[test]
    #[ignore = "installs irreversible seccomp filter; exercised in a subprocess"]
    fn filter_probe() {
        install_filter().unwrap();
        let thread = std::thread::spawn(|| 42);
        assert_eq!(thread.join().unwrap(), 42);
        for syscall in [libc::SYS_socket, libc::SYS_execve, libc::SYS_unshare] {
            assert_eq!(unsafe { libc::syscall(syscall, 0, 0, 0, 0, 0, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
        }
        assert_eq!(
            unsafe { libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        let mut sockets = [0; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_INET, libc::SOCK_STREAM, 0, sockets.as_mut_ptr()) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sockets.as_mut_ptr()) },
            0
        );
        unsafe {
            libc::close(sockets[0]);
            libc::close(sockets[1]);
        }
        let response = crate::engine::execute(Request {
            protocol_version: crate::generated::PROTOCOL_VERSION,
            id: "seccomp".into(),
            command: "eval".into(),
            source: "await Ogma.sleep(1); console.log('JSC thread filter');".into(),
            limits: crate::generated::Limits {
                timeout_ms: 1000,
                max_memory_mb: 256,
                max_output_bytes: 1000,
            },
            ..Default::default()
        });
        assert_eq!(response.status, "ok", "{:?}", response.diagnostics);
        assert_eq!(response.output[0].text, "JSC thread filter");
    }
}
