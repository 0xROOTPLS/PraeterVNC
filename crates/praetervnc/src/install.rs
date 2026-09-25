//! Self-install as a service (portable exe -> Program Files) and uninstall.
use crate::settings::{exe_path, Key, Store, KEY};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use windows::core::{w, Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Com::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Services::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const RUN: PCWSTR = w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run");
const UNINSTALL: PCWSTR = w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\PraeterVNC");
const POLICY: PCWSTR = w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\System");
const NO_WINDOW: u32 = 0x0800_0000;

pub fn elevated() -> bool {
    unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_err() {
            return false;
        }
        let mut e = TOKEN_ELEVATION::default();
        let mut n = 0u32;
        let ok = GetTokenInformation(tok, TokenElevation, Some(&mut e as *mut _ as *mut _), std::mem::size_of::<TOKEN_ELEVATION>() as u32, &mut n).is_ok();
        let _ = CloseHandle(tok);
        ok && e.TokenIsElevated != 0
    }
}

/// Runs `exe args` elevated (UAC prompt) and waits; None if declined.
pub fn run_elevated(exe: &Path, args: &str) -> Option<u32> {
    unsafe {
        let (f, a) = (HSTRING::from(exe.as_os_str()), HSTRING::from(args));
        let mut ei = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: w!("runas"),
            lpFile: PCWSTR(f.as_ptr()),
            lpParameters: PCWSTR(a.as_ptr()),
            // SW_HIDE would apply to the child's first top-level window.
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        ShellExecuteExW(&mut ei).ok()?;
        if ei.hProcess.is_invalid() {
            return None;
        }
        WaitForSingleObject(ei.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(ei.hProcess, &mut code);
        let _ = CloseHandle(ei.hProcess);
        Some(code)
    }
}

pub fn install_dir() -> PathBuf {
    PathBuf::from(std::env::var("ProgramW6432").or_else(|_| std::env::var("ProgramFiles")).unwrap_or_else(|_| r"C:\Program Files".into())).join("PraeterVNC")
}

struct Sc(SC_HANDLE);

impl Drop for Sc {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

fn scm(access: u32) -> Result<Sc, String> {
    unsafe { OpenSCManagerW(None, None, access).map(Sc).map_err(|e| format!("service manager: {e}")) }
}

fn open_service(m: &Sc, access: u32) -> Option<Sc> {
    unsafe { OpenServiceW(m.0, crate::service::NAME, access).ok().map(Sc) }
}

/// Current service state, None if not installed.
pub fn service_state() -> Option<SERVICE_STATUS_CURRENT_STATE> {
    let m = scm(SC_MANAGER_CONNECT).ok()?;
    let s = open_service(&m, SERVICE_QUERY_STATUS)?;
    let mut st = SERVICE_STATUS::default();
    unsafe { QueryServiceStatus(s.0, &mut st).ok()? };
    Some(st.dwCurrentState)
}

fn self_installed() -> bool {
    Key::open(HKEY_LOCAL_MACHINE, KEY, KEY_READ).is_ok_and(|k| k.get_u32(w!("SelfInstalled")) == Some(1))
}

fn stop_service(s: &Sc) {
    unsafe {
        let mut st = SERVICE_STATUS::default();
        let _ = ControlService(s.0, SERVICE_CONTROL_STOP, &mut st);
        for _ in 0..100 {
            if QueryServiceStatus(s.0, &mut st).is_err() || st.dwCurrentState == SERVICE_STOPPED {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

fn netsh(args: &[&str]) {
    let _ = std::process::Command::new("netsh").args(["advfirewall", "firewall"]).args(args).creation_flags(NO_WINDOW).output();
}

pub fn install() -> Result<String, String> {
    let src = exe_path();
    let dir = install_dir();
    let dst = dir.join("praetervnc.exe");
    let m = scm(SC_MANAGER_ALL_ACCESS)?;
    let existing = open_service(&m, SERVICE_ALL_ACCESS);
    if existing.is_some() && !self_installed() {
        return Err("PraeterVNC is installed by its MSI package; manage it from Apps & features.".into());
    }
    if let Some(s) = &existing {
        stop_service(s);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !same_file(&src, &dst) {
        copy_retry(&src, &dst)?;
    }
    let fresh = Key::open(HKEY_LOCAL_MACHINE, KEY, KEY_READ).map_or(true, |k| k.get_u32(w!("Port")).is_none());
    let reg = Store::Registry;
    if fresh {
        let s = Store::portable().load();
        reg.save(&s, s.password.as_deref().map(Some)).map_err(|e| format!("settings: {e}"))?;
    } else {
        reg.save(&reg.load(), None).map_err(|e| format!("settings: {e}"))?;
    }
    let k = Key::open(HKEY_LOCAL_MACHINE, KEY, KEY_ALL_ACCESS).map_err(|e| format!("settings: {e}"))?;
    let _ = k.set_u32(w!("SelfInstalled"), 1);

    let bin = HSTRING::from(format!("\"{}\" --service", dst.display()));
    let s = match existing {
        Some(s) => {
            unsafe {
                ChangeServiceConfigW(s.0, SERVICE_WIN32_OWN_PROCESS, SERVICE_AUTO_START, SERVICE_ERROR_NORMAL, &bin, None, None, None, None, None, None)
                    .map_err(|e| format!("update service: {e}"))?;
            }
            s
        }
        None => unsafe {
            CreateServiceW(m.0, crate::service::NAME, w!("PraeterVNC Server"), SERVICE_ALL_ACCESS, SERVICE_WIN32_OWN_PROCESS, SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL, &bin, None, None, None, None, None)
                .map(Sc)
                .map_err(|e| format!("create service: {e}"))?
        },
    };
    configure_service(&s);

    let exe = dst.display().to_string();
    netsh(&["delete", "rule", "name=PraeterVNC"]);
    netsh(&["add", "rule", "name=PraeterVNC", "dir=in", "action=allow", &format!("program={exe}"), "enable=yes", "profile=any"]);

    if let Ok(p) = Key::create(HKEY_LOCAL_MACHINE, POLICY, None) {
        if p.get_u32(w!("SoftwareSASGeneration")).is_none_or(|v| v == 0) {
            let _ = p.set_u32(w!("SoftwareSASGeneration"), 1);
            let _ = k.set_u32(w!("SetSAS"), 1);
        }
    }
    if let Ok(r) = Key::open(HKEY_LOCAL_MACHINE, RUN, KEY_SET_VALUE) {
        let _ = r.set_str(w!("PraeterVNC"), &format!("\"{exe}\" --tray"));
    }
    if let Ok(u) = Key::create(HKEY_LOCAL_MACHINE, UNINSTALL, None) {
        let _ = u.set_str(w!("DisplayName"), "PraeterVNC");
        let _ = u.set_str(w!("DisplayVersion"), env!("CARGO_PKG_VERSION"));
        let _ = u.set_str(w!("Publisher"), "PraeterVNC");
        let _ = u.set_str(w!("DisplayIcon"), &exe);
        let _ = u.set_str(w!("InstallLocation"), &dir.display().to_string());
        let _ = u.set_str(w!("UninstallString"), &format!("\"{exe}\" --uninstall"));
        let _ = u.set_u32(w!("NoModify"), 1);
        let _ = u.set_u32(w!("NoRepair"), 1);
        let _ = u.set_u32(w!("EstimatedSize"), (std::fs::metadata(&dst).map_or(0, |m| m.len()) / 1024) as u32);
    }
    let _ = shortcut(&dst);
    unsafe {
        StartServiceW(s.0, None).map_err(|e| format!("start service: {e}"))?;
    }
    let pw = reg.load().password.is_some();
    Ok(format!("Installed to {} and started.{}", dir.display(), if pw { "" } else { " Set a password in Settings to accept connections." }))
}

fn configure_service(s: &Sc) {
    unsafe {
        let mut desc: Vec<u16> = "Low-latency VNC server.\0".encode_utf16().collect();
        let d = SERVICE_DESCRIPTIONW { lpDescription: windows::core::PWSTR(desc.as_mut_ptr()) };
        let _ = ChangeServiceConfig2W(s.0, SERVICE_CONFIG_DESCRIPTION, Some(&d as *const _ as *const _));
        let mut acts = [SC_ACTION { Type: SC_ACTION_RESTART, Delay: 2000 }, SC_ACTION { Type: SC_ACTION_RESTART, Delay: 5000 }, SC_ACTION { Type: SC_ACTION_RESTART, Delay: 30000 }];
        let fa = SERVICE_FAILURE_ACTIONSW { dwResetPeriod: 86400, cActions: acts.len() as u32, lpsaActions: acts.as_mut_ptr(), ..Default::default() };
        let _ = ChangeServiceConfig2W(s.0, SERVICE_CONFIG_FAILURE_ACTIONS, Some(&fa as *const _ as *const _));
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn copy_retry(src: &Path, dst: &Path) -> Result<(), String> {
    let old = dst.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    let mut last = String::new();
    for i in 0..30 {
        match std::fs::copy(src, dst) {
            Ok(_) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        // A running exe (tray, settings) can't be overwritten but can be renamed.
        if i == 0 && std::fs::rename(dst, &old).is_ok() {
            continue;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Err(format!("copy to {}: {last}", dst.display()))
}

fn shortcut_path() -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_CommonPrograms, KNOWN_FOLDER_FLAG(0), None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        Some(PathBuf::from(s?).join("PraeterVNC Settings.lnk"))
    }
}

fn shortcut(exe: &Path) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(exe.as_os_str()))?;
        link.SetArguments(w!("--settings"))?;
        link.SetDescription(w!("PraeterVNC settings"))?;
        link.SetIconLocation(&HSTRING::from(exe.as_os_str()), 0)?;
        let Some(p) = shortcut_path() else { return Ok(()) };
        link.cast::<IPersistFile>()?.Save(&HSTRING::from(p.as_os_str()), true)
    }
}

pub fn uninstall() -> Result<String, String> {
    let m = scm(SC_MANAGER_ALL_ACCESS)?;
    let svc = open_service(&m, SERVICE_ALL_ACCESS);
    if svc.is_some() && !self_installed() {
        return Err("PraeterVNC is installed by its MSI package; remove it from Apps & features.".into());
    }
    if let Some(s) = &svc {
        stop_service(s);
        unsafe {
            DeleteService(s.0).map_err(|e| format!("delete service: {e}"))?;
        }
    }
    netsh(&["delete", "rule", "name=PraeterVNC"]);
    if let Ok(r) = Key::open(HKEY_LOCAL_MACHINE, RUN, KEY_SET_VALUE) {
        r.delete(w!("PraeterVNC"));
    }
    let set_sas = Key::open(HKEY_LOCAL_MACHINE, KEY, KEY_READ).is_ok_and(|k| k.get_u32(w!("SetSAS")) == Some(1));
    if set_sas {
        if let Ok(p) = Key::open(HKEY_LOCAL_MACHINE, POLICY, KEY_SET_VALUE) {
            p.delete(w!("SoftwareSASGeneration"));
        }
    }
    unsafe {
        let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, KEY);
        let _ = RegDeleteKeyW(HKEY_LOCAL_MACHINE, KEY);
        let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, UNINSTALL);
        let _ = RegDeleteKeyW(HKEY_LOCAL_MACHINE, UNINSTALL);
    }
    if let Some(p) = shortcut_path() {
        let _ = std::fs::remove_file(p);
    }
    let dir = install_dir();
    if exe_path().starts_with(&dir) {
        let cmd = format!("ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display());
        let _ = std::process::Command::new("cmd").arg("/c").raw_arg(cmd).creation_flags(NO_WINDOW).spawn();
    } else {
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok("PraeterVNC was removed.".into())
}