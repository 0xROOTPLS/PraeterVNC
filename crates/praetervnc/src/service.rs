//! Windows service: runs a SYSTEM helper in the active console session and restarts it as needed.
use crate::server::Server;
use crate::settings::{Key, Store, KEY, KEY_SDDL};
use crate::Config;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use windows::core::{s, w, BOOL, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Security::*;
use windows::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Services::*;
use windows::Win32::System::Threading::*;

pub const NAME: windows::core::PCWSTR = w!("PraeterVNC");

static STATUS: AtomicUsize = AtomicUsize::new(0);
static STOP: AtomicUsize = AtomicUsize::new(0);
static WAKE: AtomicUsize = AtomicUsize::new(0);

fn h(a: &AtomicUsize) -> HANDLE {
    HANDLE(a.load(Ordering::Acquire) as *mut _)
}

fn inheritable() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: std::ptr::null_mut(), bInheritHandle: true.into() }
}

fn set_state(state: SERVICE_STATUS_CURRENT_STATE) {
    let st = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE } else { 0 },
        dwWaitHint: 5000,
        ..Default::default()
    };
    unsafe {
        let _ = SetServiceStatus(SERVICE_STATUS_HANDLE(STATUS.load(Ordering::Acquire) as *mut _), &st);
    }
}

/// Entry point when started by the SCM.
pub fn run() {
    let mut name: Vec<u16> = "PraeterVNC\0".encode_utf16().collect();
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(name.as_mut_ptr()), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe {
        if let Err(e) = StartServiceCtrlDispatcherW(table.as_ptr()) {
            eprintln!("--service is for the service manager: {e}");
        }
    }
}

unsafe extern "system" fn handler(ctrl: u32, _ev: u32, _data: *mut core::ffi::c_void, _ctx: *mut core::ffi::c_void) -> u32 {
    match ctrl {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            set_state(SERVICE_STOP_PENDING);
            let _ = SetEvent(h(&STOP));
            NO_ERROR.0
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            let _ = SetEvent(h(&WAKE));
            NO_ERROR.0
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let Ok(st) = RegisterServiceCtrlHandlerExW(NAME, Some(handler), None) else { return };
    STATUS.store(st.0 as usize, Ordering::Release);
    let log = open_log(true);
    crate::watchdog::install(true);
    let stop = CreateEventW(None, true, false, None).unwrap_or_default();
    let wake = CreateEventW(None, false, false, None).unwrap_or_default();
    STOP.store(stop.0 as usize, Ordering::Release);
    WAKE.store(wake.0 as usize, Ordering::Release);
    set_state(SERVICE_RUNNING);
    crate::log!("service started");
    supervise(stop, wake, log.as_ref().map(|f| HANDLE(f.as_raw_handle())));
    crate::log!("service stopped");
    set_state(SERVICE_STOPPED);
}

/// `logs\praetervnc.log`, rotated at 2 MB; becomes our stderr and is inherited by children.
pub fn open_log(service: bool) -> Option<std::fs::File> {
    let dir = crate::watchdog::logs_dir(service);
    let p = dir.join("praetervnc.log");
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > 2 << 20) {
        let _ = std::fs::rename(&p, dir.join("praetervnc.1.log"));
    }
    let f = std::fs::OpenOptions::new().append(true).create(true).open(p).ok()?;
    unsafe {
        let hf = HANDLE(f.as_raw_handle());
        let _ = SetHandleInformation(hf, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
        let _ = SetStdHandle(STD_ERROR_HANDLE, hf);
        let _ = SetStdHandle(STD_OUTPUT_HANDLE, hf);
    }
    Some(f)
}

struct Helper {
    proc: HANDLE,
    session: u32,
    started: Instant,
}

fn supervise(stop: HANDLE, wake: HANDLE, log: Option<HANDLE>) {
    let sa = inheritable();
    let (hstop, sas) = unsafe {
        (CreateEventW(Some(&sa), true, false, None).unwrap_or_default(), CreateEventW(Some(&sa), false, false, None).unwrap_or_default())
    };
    let hung = unsafe { CreateEventW(None, false, false, None).unwrap_or_default() };
    let hv = hung.0 as usize;
    std::thread::Builder::new().name("health".into()).spawn(move || health(hv)).unwrap();
    let mut helper: Option<Helper> = None;
    let mut backoff = Duration::from_secs(1);
    let mut next = Instant::now();
    let beat = crate::watchdog::looped("service supervisor", Duration::from_secs(30));
    loop {
        beat.tick();
        HELPER_UP.store(helper.as_ref().map_or(0, |hp| hp.started.elapsed().as_secs() + 1), Ordering::Relaxed);
        let target = unsafe { WTSGetActiveConsoleSessionId() };
        if let Some(hp) = helper.as_ref().filter(|hp| hp.session != target) {
            crate::log!("console session {} -> {target}; restarting helper", hp.session);
            end(hp, hstop);
            helper = None;
            next = Instant::now();
        }
        if helper.is_none() && target != u32::MAX && Instant::now() >= next {
            match launch(target, hstop, sas, log) {
                Ok(p) => {
                    crate::log!("helper started in session {target}");
                    helper = Some(Helper { proc: p, session: target, started: Instant::now() });
                }
                Err(e) => {
                    crate::log!("helper launch in session {target}: {e}");
                    next = Instant::now() + backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
        let mut hs = vec![stop, wake, sas, hung];
        if let Some(hp) = &helper {
            hs.push(hp.proc);
        }
        let r = unsafe { WaitForMultipleObjects(&hs, false, 1000) };
        match r.0.wrapping_sub(WAIT_OBJECT_0.0) {
            0 => {
                if let Some(hp) = &helper {
                    end(hp, hstop);
                }
                return;
            }
            2 => {
                if let Some(hp) = &helper {
                    send_sas(hp.session);
                }
            }
            3 => {
                if let Some(hp) = helper.take() {
                    crate::log!("helper not responding; restarting it");
                    end(&hp, hstop);
                    next = Instant::now();
                }
            }
            4 => {
                let hp = helper.take().unwrap();
                let mut code = 0u32;
                unsafe {
                    let _ = GetExitCodeProcess(hp.proc, &mut code);
                    let _ = CloseHandle(hp.proc);
                }
                crate::log!("helper exited with {code:#x} after {:.0} s", hp.started.elapsed().as_secs_f64());
                if hp.started.elapsed() < Duration::from_secs(10) {
                    next = Instant::now() + backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                } else {
                    backoff = Duration::from_secs(1);
                }
            }
            _ => {}
        }
    }
}

/// Seconds the current helper has been running (+1), 0 if none.
static HELPER_UP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Signals `hung` after three missed status replies from a helper older than 20 s.
fn health(hung: usize) {
    let mut misses = 0;
    loop {
        std::thread::sleep(Duration::from_secs(5));
        if HELPER_UP.load(Ordering::Relaxed) < 20 {
            misses = 0;
            continue;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::ipc::request("status").is_some());
        });
        if rx.recv_timeout(Duration::from_secs(3)).unwrap_or(false) {
            misses = 0;
            continue;
        }
        misses += 1;
        if misses >= 3 {
            misses = 0;
            unsafe {
                let _ = SetEvent(HANDLE(hung as *mut _));
            }
        }
    }
}

fn end(hp: &Helper, hstop: HANDLE) {
    unsafe {
        let _ = SetEvent(hstop);
        if WaitForSingleObject(hp.proc, 3000) != WAIT_OBJECT_0 {
            let _ = TerminateProcess(hp.proc, 1);
            let _ = WaitForSingleObject(hp.proc, 3000);
        }
        let _ = ResetEvent(hstop);
        let _ = CloseHandle(hp.proc);
    }
}

fn session_token(session: u32, ty: TOKEN_TYPE) -> windows::core::Result<HANDLE> {
    unsafe {
        let mut tok = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut tok)?;
        let mut dup = HANDLE::default();
        let r = DuplicateTokenEx(tok, TOKEN_ALL_ACCESS, None, SecurityImpersonation, ty, &mut dup);
        let _ = CloseHandle(tok);
        r?;
        if let Err(e) = SetTokenInformation(dup, TokenSessionId, &session as *const u32 as *const _, 4) {
            let _ = CloseHandle(dup);
            return Err(e);
        }
        Ok(dup)
    }
}

fn launch(session: u32, stop: HANDLE, sas: HANDLE, log: Option<HANDLE>) -> windows::core::Result<HANDLE> {
    unsafe {
        let tok = session_token(session, TokenPrimary)?;
        let mut inherit = vec![stop, sas];
        inherit.extend(log);
        let mut size = 0usize;
        let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
        let mut buf = vec![0u8; size];
        let attrs = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr() as *mut _);
        InitializeProcThreadAttributeList(Some(attrs), 1, None, &mut size)?;
        UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize, Some(inherit.as_ptr() as *const _), inherit.len() * std::mem::size_of::<HANDLE>(), None, None)?;
        let mut desk: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.lpDesktop = PWSTR(desk.as_mut_ptr());
        si.lpAttributeList = attrs;
        if let Some(l) = log {
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            si.StartupInfo.hStdOutput = l;
            si.StartupInfo.hStdError = l;
        }
        let exe = crate::settings::exe_path();
        let mut cmd: Vec<u16> = format!("\"{}\" --helper {} {}\0", exe.display(), stop.0 as usize, sas.0 as usize).encode_utf16().collect();
        let mut pi = PROCESS_INFORMATION::default();
        let r = CreateProcessAsUserW(Some(tok), None, Some(PWSTR(cmd.as_mut_ptr())), None, None, true,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT, None, None, &si.StartupInfo, &mut pi);
        DeleteProcThreadAttributeList(attrs);
        let _ = CloseHandle(tok);
        r?;
        let _ = CloseHandle(pi.hThread);
        Ok(pi.hProcess)
    }
}

/// Ctrl+Alt+Del in `session`: SendSAS as a service impersonating a token of that session.
fn send_sas(session: u32) {
    unsafe {
        let Ok(lib) = LoadLibraryW(w!("sas.dll")) else { return crate::log!("sas.dll not available") };
        let Some(f) = GetProcAddress(lib, s!("SendSAS")) else { return };
        let f: unsafe extern "system" fn(BOOL) = std::mem::transmute(f);
        let Ok(tok) = session_token(session, TokenImpersonation) else { return };
        if ImpersonateLoggedOnUser(tok).is_ok() {
            f(false.into());
            let _ = RevertToSelf();
            crate::log!("sent Ctrl+Alt+Del to session {session}");
        }
        let _ = CloseHandle(tok);
    }
}

static SAS_EVENT: AtomicUsize = AtomicUsize::new(0);

/// Helper process: the server itself, as SYSTEM in the console session.
pub fn helper(args: &[String]) {
    let hv = |i: usize| HANDLE(args.get(i).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0) as *mut _);
    let (stop, sas) = (hv(0), hv(1));
    crate::watchdog::install(true);
    crate::desktop::FOLLOW.store(true, Ordering::Relaxed);
    SAS_EVENT.store(sas.0 as usize, Ordering::Release);
    let _ = crate::input::SAS.set(|| unsafe {
        let _ = SetEvent(HANDLE(SAS_EVENT.load(Ordering::Acquire) as *mut _));
    });
    crate::log!("helper running");
    let srv = Server::start(Config { service: true, ..Default::default() }, Store::Registry.load());
    let s2 = srv.clone();
    std::thread::Builder::new().name("ipc".into()).spawn(move || crate::ipc::serve(s2)).unwrap();
    let s2 = srv.clone();
    std::thread::Builder::new().name("settings-watch".into()).spawn(move || watch_settings(s2)).unwrap();
    unsafe {
        WaitForSingleObject(stop, INFINITE);
    }
    crate::log!("helper stopping");
    std::process::exit(0);
}

/// Applies HKLM settings changes live.
fn watch_settings(srv: std::sync::Arc<Server>) {
    let Ok(k) = Key::create(HKEY_LOCAL_MACHINE, KEY, Some(KEY_SDDL)) else { return crate::log!("settings key unavailable") };
    let Ok(ev) = (unsafe { CreateEventW(None, false, false, None) }) else { return };
    loop {
        unsafe {
            if RegNotifyChangeKeyValue(k.0, false, REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_CHANGE_NAME, Some(ev), true).is_err() {
                return;
            }
            WaitForSingleObject(ev, INFINITE);
        }
        std::thread::sleep(Duration::from_millis(200));
        let s = Store::Registry.load();
        if s != *srv.settings() {
            crate::log!("settings changed: port {} remote {} view-only {} monitor {:?} password {}", s.port, s.remote, s.view_only, s.monitor, if s.password.is_some() { "set" } else { "none" });
            srv.apply(s);
        }
    }
}
