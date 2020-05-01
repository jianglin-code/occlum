use super::*;
use exception::*;
use fs::HostStdioFds;
use process::pid_t;
use std::ffi::{CStr, CString, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use util::log::LevelFilter;
use util::mem_util::from_untrusted::*;
use util::sgx::allow_debug as sgx_allow_debug;
use sgx_tse::*;

pub static mut INSTANCE_DIR: String = String::new();
static mut ENCLAVE_PATH: String = String::new();

lazy_static! {
    static ref INIT_ONCE: Once = Once::new();
    static ref HAS_INIT: AtomicBool = AtomicBool::new(false);
}

macro_rules! ecall_errno {
    ($errno:expr) => {{
        let errno: Errno = $errno;
        -(errno as i32)
    }};
}

#[no_mangle]
pub extern "C" fn occlum_ecall_init(log_level: *const c_char, instance_dir: *const c_char) -> i32 {
    if HAS_INIT.load(Ordering::SeqCst) == true {
        return ecall_errno!(EEXIST);
    }

    assert!(!instance_dir.is_null());

    let log_level = {
        let input_log_level = match parse_log_level(log_level) {
            Err(e) => {
                eprintln!("invalid log level: {}", e.backtrace());
                return ecall_errno!(EINVAL);
            }
            Ok(log_level) => log_level,
        };
        // Use the input log level if and only if the enclave allows debug
        if sgx_allow_debug() {
            input_log_level
        } else {
            LevelFilter::Off
        }
    };

    INIT_ONCE.call_once(|| {
        // Init the log infrastructure first so that log messages will be printed afterwards
        util::log::init(log_level);
 
        // Init MPX for SFI if MPX is available
        let report = rsgx_self_report();
        if (report.body.attributes.xfrm & SGX_XFRM_MPX != 0) {
            util::mpx_util::mpx_enable();
        }

        // Register exception handlers (support cpuid & rdtsc for now)
        register_exception_handlers();
        unsafe {
            let dir_str: &str = CStr::from_ptr(instance_dir).to_str().unwrap();
            INSTANCE_DIR.push_str(dir_str);
            ENCLAVE_PATH.push_str(&INSTANCE_DIR);
            ENCLAVE_PATH.push_str("/build/lib/libocclum-libos.signed.so");
        }

        HAS_INIT.store(true, Ordering::SeqCst);
    });

    0
}

#[no_mangle]
pub extern "C" fn occlum_ecall_new_process(
    path_buf: *const c_char,
    argv: *const *const c_char,
    host_stdio_fds: *const HostStdioFds,
) -> i32 {
    if HAS_INIT.load(Ordering::SeqCst) == false {
        return ecall_errno!(EAGAIN);
    }

    let (path, args, host_stdio_fds) = match parse_arguments(path_buf, argv, host_stdio_fds) {
        Ok(path_and_args_and_host_stdio_fds) => path_and_args_and_host_stdio_fds,
        Err(e) => {
            eprintln!("invalid arguments for LibOS: {}", e.backtrace());
            return ecall_errno!(e.errno());
        }
    };

    let _ = unsafe { backtrace::enable_backtrace(&ENCLAVE_PATH, PrintFormat::Short) };
    panic::catch_unwind(|| {
        backtrace::__rust_begin_short_backtrace(|| {
            match do_new_process(&path, &args, &host_stdio_fds) {
                Ok(pid_t) => pid_t as i32,
                Err(e) => {
                    eprintln!("failed to boot up LibOS: {}", e.backtrace());
                    ecall_errno!(e.errno())
                }
            }
        })
    })
    .unwrap_or(ecall_errno!(EFAULT))
}

#[no_mangle]
pub extern "C" fn occlum_ecall_exec_thread(libos_pid: i32, host_tid: i32) -> i32 {
    if HAS_INIT.load(Ordering::SeqCst) == false {
        return ecall_errno!(EAGAIN);
    }

    let _ = unsafe { backtrace::enable_backtrace(&ENCLAVE_PATH, PrintFormat::Short) };
    panic::catch_unwind(|| {
        backtrace::__rust_begin_short_backtrace(|| {
            match do_exec_thread(libos_pid as pid_t, host_tid as pid_t) {
                Ok(exit_status) => exit_status,
                Err(e) => {
                    eprintln!("failed to execute a process: {}", e.backtrace());
                    ecall_errno!(e.errno())
                }
            }
        })
    })
    .unwrap_or(ecall_errno!(EFAULT))
}

fn parse_log_level(level_chars: *const c_char) -> Result<LevelFilter> {
    const DEFAULT_LEVEL: LevelFilter = LevelFilter::Off;

    if level_chars.is_null() {
        return Ok(DEFAULT_LEVEL);
    }

    let level_string = {
        // level_chars has been guaranteed to be inside enclave
        // and null terminated by ECall
        let level_cstring = CString::from(unsafe { CStr::from_ptr(level_chars) });
        level_cstring
            .into_string()
            .map_err(|e| errno!(EINVAL, "log_level contains valid utf-8 data"))?
            .to_lowercase()
    };
    Ok(match level_string.as_str() {
        "off" => LevelFilter::Off,
        "panic" | "fatal" | "error" => LevelFilter::Error,
        "warning" | "warn" => LevelFilter::Warn, // Panic, fatal and warning are log levels defined in OCI (Open Container Initiative)
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => DEFAULT_LEVEL, // Default
    })
}

fn parse_arguments(
    path_ptr: *const c_char,
    argv: *const *const c_char,
    host_stdio_fds: *const HostStdioFds,
) -> Result<(PathBuf, Vec<CString>, HostStdioFds)> {
    let path_buf = {
        if path_ptr.is_null() {
            return_errno!(EINVAL, "empty path");
        }
        // path_ptr has been guaranteed to be inside enclave
        // and null terminated by ECall
        let path_cstring = CString::from(unsafe { CStr::from_ptr(path_ptr) });

        let path_string = path_cstring
            .into_string()
            .map_err(|e| errno!(EINVAL, "path contains valid utf-8 data"))?;
        Path::new(&path_string).to_path_buf()
    };
    let program_cstring = {
        let program_osstr = path_buf
            .file_name()
            .ok_or_else(|| errno!(EINVAL, "invalid path"))?;
        let program_str = program_osstr
            .to_str()
            .ok_or_else(|| errno!(EINVAL, "invalid path"))?;
        CString::new(program_str).map_err(|e| errno!(e))?
    };

    let mut args = clone_cstrings_safely(argv)?;
    args.insert(0, program_cstring);

    let host_stdio_fds = HostStdioFds::from_user(host_stdio_fds)?;

    Ok((path_buf, args, host_stdio_fds))
}

fn do_new_process(
    program_path: &PathBuf,
    argv: &Vec<CString>,
    host_stdio_fds: &HostStdioFds,
) -> Result<pid_t> {
    validate_program_path(program_path)?;

    let envp = &config::LIBOS_CONFIG.env;
    let file_actions = Vec::new();
    let current = &process::IDLE;
    let program_path_str = program_path.to_str().unwrap();
    let new_tid = process::do_spawn_without_exec(
        &program_path_str,
        argv,
        envp,
        &file_actions,
        host_stdio_fds,
        current,
    )?;
    Ok(new_tid)
}

fn do_exec_thread(libos_tid: pid_t, host_tid: pid_t) -> Result<i32> {
    let status = process::task::exec(libos_tid, host_tid)?;

    // sync file system
    // TODO: only sync when all processes exit
    use rcore_fs::vfs::FileSystem;
    crate::fs::ROOT_INODE.fs().sync()?;

    // Not to be confused with the return value of a main function.
    // The exact meaning of status is described in wait(2) man page.
    Ok(status)
}

fn validate_program_path(target_path: &PathBuf) -> Result<()> {
    if !target_path.is_absolute() {
        return_errno!(EINVAL, "program path must be absolute");
    }

    // Forbid paths like /bin/../root, which may circument our prefix-based path matching
    let has_parent_component = {
        target_path
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    };
    if has_parent_component {
        return_errno!(
            EINVAL,
            "program path cannot contain any parent component (i.e., \"..\")"
        );
    }

    // Check whether the prefix of the program path matches one of the entry points
    let is_valid_entry_point = &config::LIBOS_CONFIG
        .entry_points
        .iter()
        .any(|valid_path_prefix| target_path.starts_with(valid_path_prefix));
    if !is_valid_entry_point {
        return_errno!(EACCES, "program path is NOT a valid entry point");
    }
    Ok(())
}
