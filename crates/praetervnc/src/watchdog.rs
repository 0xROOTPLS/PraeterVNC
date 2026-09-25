//! Crash logging and stall detection.
//! No minidumps: suspended capture threads can stall the display driver.
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};
use windows::Win32::System::Diagnostics::Debug::*;
use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};

/// Exit codes.
pub const STALLED: u32 = 3;
const PANICKED: u32 = 4;
const CRASHED: u32 = 5;

static BASE: OnceLock<Instant> = OnceLock::new();
static BEATS: Mutex<Vec<Weak<Beat>>> = Mutex::new(Vec::new());
static SUPERVISED: AtomicBool = AtomicBool::new(false);
static RESPAWN: OnceLock<Vec<String>> = OnceLock::new();

/// Ends at once: no exit handlers, no Windows Error Reporting.
fn die(code: u32) -> ! {
    unsafe {
        let _ = TerminateProcess(GetCurrentProcess(), code);
    }
    loop {
        std::thread::park();
    }
}

/// Portable mode: relaunch with `args` on failure.
pub fn respawn_with(args: Vec<String>) {
    let _ = RESPAWN.set(args);
}

fn respawn() {
    if let Some(a) = RESPAWN.get() {
        crate::log!("relaunching");
        let _ = std::process::Command::new(crate::settings::exe_path()).args(a).env_remove("PRAETER_FAULT").spawn();
    }
}

fn now_ms() -> u64 {
    BASE.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// A loop or call that must make progress within `limit`.
pub struct Beat {
    name: String,
    limit: Duration,
    fatal: bool,
    last: AtomicU64,
    /// Checked only between `enter` and `leave`.
    call: bool,
    busy: AtomicBool,
}

impl Beat {
    pub fn tick(&self) {
        self.last.store(now_ms(), Ordering::Relaxed);
    }

    pub fn enter(&self) {
        self.tick();
        self.busy.store(true, Ordering::Release);
    }

    pub fn leave(&self) {
        self.busy.store(false, Ordering::Release);
    }
}

fn register(name: String, limit: Duration, fatal: bool, call: bool) -> Arc<Beat> {
    let b = Arc::new(Beat { name, limit, fatal, last: AtomicU64::new(now_ms()), call, busy: AtomicBool::new(false) });
    let mut v = BEATS.lock();
    v.retain(|w| w.strong_count() > 0);
    v.push(Arc::downgrade(&b));
    b
}

/// A loop that ticks at least every `limit`.
pub fn looped(name: impl Into<String>, limit: Duration) -> Arc<Beat> {
    register(name.into(), limit, true, false)
}

/// A call that must return within `limit`; `fatal` restarts the process on a stall.
pub fn call(name: impl Into<String>, limit: Duration, fatal: bool) -> Arc<Beat> {
    register(name.into(), limit, fatal, true)
}

/// Crash logging plus the watchdog thread. `supervised`: exit on a fatal stall.
pub fn install(supervised: bool) {
    SUPERVISED.store(supervised, Ordering::Relaxed);
    let _ = BASE.set(Instant::now());
    std::panic::set_hook(Box::new(|info| {
        crate::log!("panic: {info}\n{}", std::backtrace::Backtrace::force_capture());
        respawn();
        die(PANICKED);
    }));
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
        SetUnhandledExceptionFilter(Some(on_exception));
    }
    std::thread::Builder::new().name("watchdog".into()).spawn(watch).unwrap();
    // Test hook (debug builds): PRAETER_FAULT=stall|panic.
    #[cfg(debug_assertions)]
    match std::env::var("PRAETER_FAULT").as_deref() {
        Ok("stall") => std::mem::forget(looped("fault test", Duration::from_secs(2))),
        Ok("panic") => {
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(2));
                panic!("fault test");
            });
        }
        _ => {}
    }
}

unsafe extern "system" fn on_exception(ep: *const EXCEPTION_POINTERS) -> i32 {
    let rec = if ep.is_null() { std::ptr::null_mut() } else { (*ep).ExceptionRecord };
    if rec.is_null() {
        crate::log!("crash: unknown exception");
    } else {
        crate::log!("crash: exception {:#010x} at {:?}", (*rec).ExceptionCode.0 as u32, (*rec).ExceptionAddress);
    }
    respawn();
    die(CRASHED)
}

fn watch() {
    let mut reported: Vec<String> = Vec::new();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let now = now_ms();
        let beats: Vec<Arc<Beat>> = BEATS.lock().iter().filter_map(Weak::upgrade).collect();
        for b in beats {
            if b.call && !b.busy.load(Ordering::Acquire) {
                continue;
            }
            let idle = now.saturating_sub(b.last.load(Ordering::Relaxed));
            if idle < b.limit.as_millis() as u64 || reported.contains(&b.name) {
                continue;
            }
            crate::log!("stall: {} made no progress for {:.1} s", b.name, idle as f64 / 1e3);
            reported.push(b.name.clone());
            if b.fatal && (SUPERVISED.load(Ordering::Relaxed) || RESPAWN.get().is_some()) {
                crate::log!("restarting");
                respawn();
                die(STALLED);
            }
        }
    }
}

/// Log folder: next to the exe (admin-only for the service), else %LOCALAPPDATA%.
pub fn logs_dir(service: bool) -> PathBuf {
    let d = crate::settings::exe_path().with_file_name("logs");
    if service {
        let _ = std::fs::create_dir_all(&d);
        secure(&d);
        return d;
    }
    let probe = d.join(".w");
    if std::fs::create_dir_all(&d).is_ok() && std::fs::write(&probe, b"").is_ok() {
        let _ = std::fs::remove_file(probe);
        return d;
    }
    let d = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join("PraeterVNC").join("logs");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// SYSTEM and Administrators only.
fn secure(dir: &std::path::Path) {
    use windows::core::{w, HSTRING};
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::*;
    use windows::Win32::Security::*;
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(w!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"), 1, &mut sd, None).is_err() {
            return;
        }
        let (mut present, mut def) = (windows::core::BOOL(0), windows::core::BOOL(0));
        let mut acl: *mut ACL = std::ptr::null_mut();
        if GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut def).is_ok() {
            let _ = SetNamedSecurityInfoW(&HSTRING::from(dir.as_os_str()), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, None, None, Some(acl), None);
        }
        LocalFree(Some(HLOCAL(sd.0)));
    }
}
