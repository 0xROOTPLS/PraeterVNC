//! Tray icon and settings window.
use crate::server::{Server, Status};
use crate::settings::{Settings, Store};
use parking_lot::Mutex;
use std::cell::{Cell, RefCell};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::System::Services::{SERVICE_RUNNING, SERVICE_STATUS_CURRENT_STATE, SERVICE_STOPPED};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{EM_LIMITTEXT, EM_SETCUEBANNER};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const WM_CHANGED: u32 = WM_APP + 2;
const WM_OPEN: u32 = WM_APP + 3;
const TRAY_CLASS: PCWSTR = w!("PraeterVNCTray");
const FORM_CLASS: PCWSTR = w!("PraeterVNCSettings");

const ID_PORT: i32 = 101;
const ID_PW: i32 = 102;
const ID_PW2: i32 = 103;
const ID_ATTEMPTS: i32 = 104;
const ID_BLOCK: i32 = 105;
const ID_MONITOR: i32 = 106;
const ID_REMOTE: i32 = 107;
const ID_VIEWONLY: i32 = 108;
const ID_INSTALL: i32 = 109;
const ID_INFO: i32 = 110;
const ID_HEAD: i32 = 111;
const ID_HINT: i32 = 112;
const ID_ADDR: i32 = 113;
const ID_COPY: i32 = 114;

const T_COPIED: usize = 1;
const T_ERROR: usize = 2;
/// Listener errors shorter than this aren't announced.
const ERROR_GRACE: Duration = Duration::from_secs(3);

/// Form geometry in DIPs.
const FORM_W: i32 = 460;
const HEAD_H: i32 = 72;
const NOTICE_H: i32 = 40;
const BODY_H: i32 = 438;
const FOOT_H: i32 = 56;
/// Offsets from the first section heading.
const DIVIDERS: [i32; 2] = [130, 306];

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}
const BRAND: [COLORREF; 2] = [rgb(0x3B, 0x5B, 0xDB), rgb(0x15, 0xAA, 0xBF)];
const ON_BRAND: COLORREF = rgb(0xFF, 0xFF, 0xFF);
const ON_BRAND_DIM: COLORREF = rgb(0xDB, 0xE4, 0xFF);
const WARN_BG: COLORREF = rgb(0xFF, 0xF4, 0xCE);
const TEXT: COLORREF = rgb(0x1B, 0x1B, 0x1B);
const ACCENT: COLORREF = rgb(0x2B, 0x4A, 0xC0);
const HINT: COLORREF = rgb(0x6B, 0x6B, 0x6B);
const FOOTER_BG: COLORREF = rgb(0xF3, 0xF3, 0xF3);
const LINE: COLORREF = rgb(0xE0, 0xE0, 0xE0);

const M_SETTINGS: usize = 1;
const M_DISCONNECT: usize = 2;
const M_INSTALL: usize = 3;
const M_EXIT: usize = 4;
const M_RELEASE: usize = 5;
const M_COPY: usize = 6;
const M_START: usize = 7;
const M_STOP: usize = 8;

#[derive(Default)]
struct Polled {
    done: bool,
    status: Option<Status>,
    state: Option<SERVICE_STATUS_CURRENT_STATE>,
}

enum Mode {
    App(Arc<Server>),
    Service(Arc<Mutex<Polled>>),
}

struct Tray {
    hwnd: HWND,
    mode: Mode,
    status: Status,
    reachable: bool,
    state: Option<SERVICE_STATUS_CURRENT_STATE>,
    polled: bool,
    icon: u16,
    taskbar: u32,
    /// Last announced listener error.
    error: Option<String>,
    /// Listener error not yet announced, and when it was first seen.
    pending: Option<(String, Instant)>,
}

#[derive(Clone, Copy, Default)]
struct Palette {
    hc: bool,
    text: COLORREF,
    accent: COLORREF,
    hint: COLORREF,
    window: COLORREF,
    warn: COLORREF,
    footer: COLORREF,
    line: COLORREF,
}

struct Form {
    hwnd: HWND,
    store: Store,
    srv: Option<Arc<Server>>,
    has_pw: bool,
    /// Saved port when opened.
    port: u16,
    notice: Option<String>,
    host: String,
    /// Address row shows a reachable address.
    addr_ok: Cell<bool>,
    dpi: u32,
    pal: Palette,
    font: HFONT,
    strong: HFONT,
    head: HFONT,
    title: HFONT,
    icon: HICON,
    warn_icon: HICON,
    bg: HBRUSH,
    warn: HBRUSH,
    footer: HBRUSH,
    line: HBRUSH,
    ctrls: Vec<(HWND, (i32, i32, i32, i32))>,
    standalone: bool,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
    static FORM: RefCell<Option<Form>> = const { RefCell::new(None) };
}

fn hinst() -> HINSTANCE {
    unsafe { GetModuleHandleW(None).unwrap_or_default().into() }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn copy_to(dst: &mut [u16], s: &str) {
    let v: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..v.len()].copy_from_slice(&v);
    dst[v.len()] = 0;
}

pub fn message(text: &str, error: bool) {
    unsafe {
        let f = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
        MessageBoxW(None, &HSTRING::from(text), w!("PraeterVNC"), f | MB_OK | MB_SETFOREGROUND);
    }
}

/// MAKEINTRESOURCE.
fn res(id: u16) -> PCWSTR {
    PCWSTR(id as usize as *const u16)
}

fn load_icon(id: u16, dpi: u32) -> HICON {
    unsafe {
        let cx = GetSystemMetricsForDpi(SM_CXSMICON, dpi);
        LoadImageW(Some(hinst()), res(id), IMAGE_ICON, cx, cx, LR_DEFAULTCOLOR)
            .map(|h| HICON(h.0))
            .or_else(|_| LoadIconW(None, IDI_APPLICATION))
            .unwrap_or_default()
    }
}

/// This computer as viewers on the network see it.
fn host_name() -> String {
    crate::net::lan_ip().map(|i| i.to_string()).or_else(|| std::env::var("COMPUTERNAME").ok()).unwrap_or_else(|| "this computer".into())
}

/// Address viewers connect to for a listener on `listen`; None if loopback only.
fn connect_addr(listen: &str) -> Option<String> {
    let a: SocketAddr = listen.parse().ok()?;
    if a.ip().is_loopback() {
        return None;
    }
    let host = if a.ip().is_unspecified() { host_name() } else { a.ip().to_string() };
    Some(format!("{host}:{}", a.port()))
}

/// Signals an existing tray in this session to open settings; true if one exists.
fn existing() -> bool {
    unsafe {
        match FindWindowW(TRAY_CLASS, None) {
            Ok(h) => {
                let _ = PostMessageW(Some(h), WM_OPEN, WPARAM(0), LPARAM(0));
                true
            }
            Err(_) => false,
        }
    }
}

/// Portable mode: server in this process plus tray. `restarted`: relaunched by the watchdog.
pub fn app(restarted: bool) {
    if restarted {
        for _ in 0..50 {
            if unsafe { FindWindowW(TRAY_CLASS, None) }.is_err() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    if existing() {
        return;
    }
    let log = crate::service::open_log(false);
    std::mem::forget(log);
    crate::watchdog::install(false);
    crate::watchdog::respawn_with(vec!["--restarted".into()]);
    let store = Store::portable();
    let first = matches!(&store, Store::Ini(p) if !p.exists());
    let srv = Server::start(crate::Config::default(), store.load());
    run_tray(Mode::App(srv), first);
}

/// Tray for the installed service (status over the control pipe).
pub fn service_tray() {
    if existing() {
        return;
    }
    run_tray(Mode::Service(Arc::default()), false);
}

const SERVICE_TITLE: PCWSTR = w!("PraeterVNC Service Settings");

/// Service settings (elevated), no tray. Raises an open one instead of opening another.
pub fn settings_standalone() {
    unsafe {
        if let Ok(h) = FindWindowW(FORM_CLASS, SERVICE_TITLE) {
            let _ = ShowWindow(h, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(h);
            return;
        }
    }
    register();
    open_form(Store::Registry, None, true);
    pump();
}

/// Debug builds: settings form only, no tray or server.
#[cfg(debug_assertions)]
pub fn preview(store: Store) {
    register();
    open_form(store, None, true);
    pump();
}

/// Debug builds: tray alongside any other; portable with a server if `store`, else service.
#[cfg(debug_assertions)]
pub fn preview_tray(store: Option<Store>) {
    match store {
        Some(s) => run_tray(Mode::App(Server::start(crate::Config::default(), s.load())), false),
        None => run_tray(Mode::Service(Arc::default()), false),
    }
}

fn register() {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let icon = LoadImageW(Some(hinst()), res(1), IMAGE_ICON, 0, 0, LR_DEFAULTSIZE).map(|h| HICON(h.0)).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(tray_proc),
            hInstance: hinst(),
            lpszClassName: TRAY_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(form_proc),
            hInstance: hinst(),
            lpszClassName: FORM_CLASS,
            hIcon: icon,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

fn pump() {
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let form = FORM.with(|f| f.borrow().as_ref().map(|f| f.hwnd));
            if form.is_some_and(|h| IsDialogMessageW(h, &msg).as_bool()) {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn run_tray(mode: Mode, open_settings: bool) {
    register();
    unsafe {
        let Ok(hwnd) = CreateWindowExW(Default::default(), TRAY_CLASS, w!("PraeterVNC"), WINDOW_STYLE(0), 0, 0, 0, 0, None, None, Some(hinst()), None) else { return };
        let h = hwnd.0 as usize;
        let post = move || PostMessageW(Some(HWND(h as *mut _)), WM_CHANGED, WPARAM(0), LPARAM(0)).is_ok();
        match &mode {
            Mode::App(srv) => srv.on_change(move || {
                post();
            }),
            Mode::Service(p) => {
                let p = p.clone();
                std::thread::spawn(move || loop {
                    let status = crate::ipc::request("status").map(|s| Status::decode(&s));
                    *p.lock() = Polled { done: true, status, state: crate::install::service_state() };
                    if !post() {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(2));
                });
            }
        }
        TRAY.with(|t| {
            *t.borrow_mut() = Some(Tray {
                hwnd, mode, status: Status::default(), reachable: false, state: None, polled: false, icon: 0,
                taskbar: RegisterWindowMessageW(w!("TaskbarCreated")), error: None, pending: None,
            })
        });
        add_icon();
        refresh();
        if open_settings {
            show_settings();
        }
    }
    pump();
}

fn add_icon() {
    TRAY.with(|t| {
        let mut t = t.borrow_mut();
        let Some(t) = t.as_mut() else { return };
        t.icon = 0;
        let mut nid = nid(t.hwnd);
        nid.uFlags = NIF_MESSAGE;
        nid.uCallbackMessage = WM_TRAY;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
    });
}

fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() }
}

fn ip_of(peer: &str) -> &str {
    peer.rsplit_once(':').map_or(peer, |p| p.0)
}

fn fmt_dur(s: u64) -> String {
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min", s / 60),
        _ => format!("{} h {} min", s / 3600, s / 60 % 60),
    }
}

/// Status line for tooltip and menu.
fn headline(t: &Tray) -> String {
    if matches!(t.mode, Mode::Service(_)) && !t.reachable {
        return match t.state {
            Some(s) if s == SERVICE_RUNNING => "Service starting…".into(),
            Some(_) => "Service is stopped".into(),
            None => "Service not installed".into(),
        };
    }
    match (&t.status.listen, &t.status.error) {
        (_, Some(e)) if e == "no password set" => "No password set: not accepting connections".into(),
        (_, Some(e)) => e.clone(),
        (Some(l), _) => match connect_addr(l) {
            Some(a) => format!("Listening on {a}"),
            None => format!("Listening on {} (this computer only)", l.replace("127.0.0.1", "localhost")),
        },
        (None, None) => "Starting…".into(),
    }
}

/// Re-reads status, updates the icon and tooltip, and announces changes; false once the service is gone.
fn refresh() -> bool {
    TRAY.with(|t| {
        let mut t = t.borrow_mut();
        let Some(t) = t.as_mut() else { return true };
        let (st, ok) = match &t.mode {
            Mode::App(srv) => (srv.status(), true),
            Mode::Service(p) => {
                let p = p.lock();
                if p.done && p.state.is_none() {
                    return false;
                }
                t.state = p.state;
                p.status.clone().map_or((Status::default(), false), |s| (s, true))
            }
        };
        let seen = t.polled && t.reachable;
        let viewer = st.clients.iter().find(|c| seen && !t.status.clients.iter().any(|o| o.0 == c.0)).map(|c| ip_of(&c.0).to_string());
        let blocked = st.blocked.iter().find(|b| seen && !t.status.blocked.iter().any(|o| o.0 == b.0)).map(|b| b.0.clone());
        let error = st.error.clone().filter(|e| ok && e != "no password set");
        let new_error = match error {
            None => {
                (t.error, t.pending) = (None, None);
                None
            }
            Some(e) if t.error.as_ref() == Some(&e) => None,
            Some(e) => match &t.pending {
                Some((p, since)) if *p == e && since.elapsed() >= ERROR_GRACE => {
                    (t.error, t.pending) = (Some(e.clone()), None);
                    Some(e)
                }
                Some((p, _)) if *p == e => None,
                _ => {
                    t.pending = Some((e, Instant::now()));
                    unsafe { SetTimer(Some(t.hwnd), T_ERROR, ERROR_GRACE.as_millis() as u32 + 100, None) };
                    None
                }
            },
        };
        t.polled = true;
        t.status = st;
        t.reachable = ok;
        let icon = if !ok || t.status.listen.is_none() { 3 } else if t.status.clients.is_empty() { 1 } else { 2 };
        let n = t.status.clients.len();
        let tip = if ok && n > 0 { format!("PraeterVNC\n{n} viewer{} connected", if n == 1 { "" } else { "s" }) } else { format!("PraeterVNC\n{}", headline(t)) };
        let mut nid = nid(t.hwnd);
        nid.uFlags = NIF_TIP | NIF_SHOWTIP;
        copy_to(&mut nid.szTip, &tip);
        if icon != t.icon {
            t.icon = icon;
            nid.uFlags |= NIF_ICON;
            nid.hIcon = load_icon(icon, unsafe { GetDpiForSystem() });
        }
        let note = match (new_error, blocked, viewer) {
            (Some(e), _, _) => Some((NIIF_WARNING, "Not accepting connections", e)),
            (_, Some(ip), _) => Some((NIIF_WARNING, "Address blocked", format!("{ip} made too many failed login attempts."))),
            (_, _, Some(ip)) => Some((NIIF_INFO, "Viewer connected", format!("{ip} is viewing this computer."))),
            _ => None,
        };
        if let Some((flags, title, text)) = note {
            nid.uFlags |= NIF_INFO;
            nid.dwInfoFlags = flags;
            copy_to(&mut nid.szInfoTitle, title);
            copy_to(&mut nid.szInfo, &text);
        }
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
        true
    })
}

struct Menu {
    hwnd: HWND,
    lines: Vec<String>,
    clients: Vec<String>,
    blocked: Vec<String>,
    addr: Option<String>,
    app: bool,
    /// Service mode: Some(running).
    service: Option<bool>,
}

fn show_menu() {
    refresh();
    let Some(mi) = TRAY.with(|t| {
        let t = t.borrow();
        let t = t.as_ref()?;
        let mut lines = vec![headline(t)];
        if t.reachable && t.status.listen.is_some() && t.status.error.is_some() {
            lines.push(t.status.error.clone().unwrap_or_default());
        }
        Some(Menu {
            hwnd: t.hwnd,
            lines,
            clients: t.status.clients.iter().map(|(p, s)| format!("{}  ·  {}", ip_of(p), fmt_dur(*s))).collect(),
            blocked: t.status.blocked.iter().map(|(ip, s)| format!("{ip}  ·  blocked for {}", fmt_dur(*s))).collect(),
            addr: t.status.listen.as_deref().filter(|_| t.reachable).and_then(connect_addr),
            app: matches!(t.mode, Mode::App(_)),
            service: t.state.filter(|_| matches!(t.mode, Mode::Service(_))).map(|s| s != SERVICE_STOPPED),
        })
    }) else { return };
    let (hwnd, app) = (mi.hwnd, mi.app);
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        for l in &mi.lines {
            let _ = AppendMenuW(m, MF_STRING | MF_GRAYED, 0, &HSTRING::from(l.as_str()));
        }
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        if mi.clients.is_empty() {
            let _ = AppendMenuW(m, MF_STRING | MF_GRAYED, 0, w!("No viewers connected"));
        }
        for c in mi.clients.iter().chain(&mi.blocked) {
            let _ = AppendMenuW(m, MF_STRING | MF_GRAYED, 0, &HSTRING::from(c.as_str()));
        }
        let dis = if mi.clients.is_empty() { MF_GRAYED } else { MF_STRING };
        let _ = AppendMenuW(m, MF_STRING | dis, M_DISCONNECT, w!("Disconnect all"));
        let _ = AppendMenuW(m, MF_STRING, M_RELEASE, w!("Release stuck keys"));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        if mi.addr.is_some() {
            let _ = AppendMenuW(m, MF_STRING, M_COPY, w!("Copy address"));
        }
        let _ = AppendMenuW(m, MF_STRING, M_SETTINGS, w!("Settings…"));
        if app {
            let _ = AppendMenuW(m, MF_STRING, M_INSTALL, w!("Install as service…"));
        }
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        match mi.service {
            Some(true) => {
                let _ = AppendMenuW(m, MF_STRING, M_STOP, w!("Stop service"));
            }
            Some(false) => {
                let _ = AppendMenuW(m, MF_STRING, M_START, w!("Start service"));
            }
            None => {}
        }
        let _ = AppendMenuW(m, MF_STRING, M_EXIT, if app { w!("Exit") } else { w!("Hide icon") });
        let _ = SetMenuDefaultItem(m, M_SETTINGS as u32, 0);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenuEx(m, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, pt.x, pt.y, hwnd, None).0 as usize;
        let _ = DestroyMenu(m);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        match cmd {
            M_SETTINGS => show_settings(),
            M_DISCONNECT => disconnect(),
            M_RELEASE if app => {
                crate::input::release_stuck();
            }
            M_RELEASE => {
                std::thread::spawn(|| crate::ipc::request("release"));
            }
            M_COPY => {
                if let Some(a) = &mi.addr {
                    crate::clipboard::set_text(a, 0);
                }
            }
            M_START => elevate("--start"),
            M_STOP if confirm_cutoff(hwnd, mi.clients.len(), "Stop the PraeterVNC service? Nobody can connect until it is started again.") => elevate("--stop"),
            M_INSTALL => install_from_app(None),
            M_EXIT if !app || confirm_cutoff(hwnd, mi.clients.len(), "Exit PraeterVNC?") => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

/// Asks before disconnecting `n` viewers; true to go ahead.
fn confirm_cutoff(owner: HWND, n: usize, question: &str) -> bool {
    if n == 0 {
        return true;
    }
    let text = format!("{question}\n\n{n} connected viewer{} will be disconnected.", if n == 1 { "" } else { "s" });
    unsafe { MessageBoxW(Some(owner), &HSTRING::from(text), w!("PraeterVNC"), MB_ICONWARNING | MB_YESNO | MB_DEFBUTTON2 | MB_SETFOREGROUND) == IDYES }
}

/// Runs this exe with `arg` elevated, off the UI thread. One UAC prompt at a time.
fn elevate(arg: &'static str) {
    static BUSY: AtomicBool = AtomicBool::new(false);
    if BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let exe = crate::settings::exe_path();
    std::thread::spawn(move || {
        crate::install::run_elevated(&exe, arg);
        BUSY.store(false, Ordering::Release);
    });
}

fn disconnect() {
    let srv = TRAY.with(|t| t.borrow().as_ref().and_then(|t| if let Mode::App(s) = &t.mode { Some(s.clone()) } else { None }));
    match srv {
        Some(s) => s.disconnect_all(),
        None => {
            std::thread::spawn(|| crate::ipc::request("disconnect"));
        }
    }
    refresh();
}

fn show_settings() {
    if let Some(h) = FORM.with(|f| f.borrow().as_ref().map(|f| f.hwnd)) {
        unsafe {
            let _ = ShowWindow(h, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(h);
        }
        return;
    }
    let srv = TRAY.with(|t| t.borrow().as_ref().and_then(|t| if let Mode::App(s) = &t.mode { Some(s.clone()) } else { None }));
    match srv {
        Some(s) => open_form(Store::portable(), Some(s), false),
        None => {
            if let Ok(h) = unsafe { FindWindowW(FORM_CLASS, SERVICE_TITLE) } {
                if unsafe { IsWindowVisible(h) }.as_bool() {
                    unsafe {
                        let _ = SetForegroundWindow(h);
                    }
                    return;
                }
            }
            elevate("--settings");
        }
    }
}

/// Installs the service from the portable app, then hands over to the installed tray.
fn install_from_app(owner: Option<HWND>) {
    let exe = crate::settings::exe_path();
    let srv = TRAY.with(|t| t.borrow().as_ref().and_then(|t| if let Mode::App(s) = &t.mode { Some(s.clone()) } else { None }));
    // The service needs the port.
    if let Some(s) = &srv {
        s.pause(true);
    }
    let r = crate::install::run_elevated(&exe, "--install");
    if r != Some(0) {
        if let Some(s) = &srv {
            s.pause(false);
        }
    }
    match r {
        Some(0) => {
            if let Some(s) = srv {
                s.disconnect_all();
            }
            // The new tray exits while this window exists.
            unsafe {
                if let Some(o) = owner {
                    let _ = DestroyWindow(o);
                }
                if let Some(h) = TRAY.with(|t| t.borrow().as_ref().map(|t| t.hwnd)) {
                    let _ = DestroyWindow(h);
                }
            }
            let tray = crate::install::install_dir().join("praetervnc.exe");
            let _ = std::process::Command::new(tray).arg("--tray").spawn();
        }
        Some(_) => {}
        None => message("Installation was cancelled.", false),
    }
}

fn is_service() -> bool {
    TRAY.with(|t| t.try_borrow().ok().is_some_and(|t| t.as_ref().is_some_and(|t| matches!(t.mode, Mode::Service(_)))))
}

unsafe extern "system" fn tray_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let taskbar = TRAY.with(|t| t.try_borrow().ok().and_then(|t| t.as_ref().map(|t| t.taskbar))).unwrap_or(u32::MAX);
    match m {
        WM_TRAY => {
            match (lp.0 & 0xFFFF) as u32 {
                WM_CONTEXTMENU => show_menu(),
                // Service settings need UAC.
                NIN_SELECT | 1025 if is_service() => show_menu(),
                NIN_SELECT | 1025 => show_settings(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_CHANGED => {
            if !refresh() {
                let _ = DestroyWindow(h);
            }
            LRESULT(0)
        }
        WM_OPEN => {
            show_settings();
            LRESULT(0)
        }
        WM_TIMER if wp.0 == T_ERROR => {
            let _ = KillTimer(Some(h), T_ERROR);
            if !refresh() {
                let _ = DestroyWindow(h);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let mut nid = nid(h);
            nid.uFlags = NOTIFY_ICON_DATA_FLAGS(0);
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ if m == taskbar => {
            add_icon();
            refresh();
            LRESULT(0)
        }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

fn scale(v: i32, dpi: u32) -> i32 {
    (v * dpi as i32 + 48) / 96
}

fn high_contrast() -> bool {
    #[repr(C)]
    struct HighContrast {
        size: u32,
        flags: u32,
        scheme: *mut u16,
    }
    let mut hc = HighContrast { size: std::mem::size_of::<HighContrast>() as u32, flags: 0, scheme: std::ptr::null_mut() };
    unsafe { SystemParametersInfoW(SPI_GETHIGHCONTRAST, hc.size, Some(&mut hc as *mut _ as *mut _), Default::default()).is_ok() && hc.flags & 1 != 0 }
}

fn palette() -> Palette {
    let sys = |i| COLORREF(unsafe { GetSysColor(i) });
    let window = sys(COLOR_WINDOW);
    if high_contrast() {
        let text = sys(COLOR_WINDOWTEXT);
        return Palette { hc: true, text, accent: text, hint: sys(COLOR_GRAYTEXT), window, warn: window, footer: sys(COLOR_BTNFACE), line: text };
    }
    Palette { hc: false, text: TEXT, accent: ACCENT, hint: HINT, window, warn: WARN_BG, footer: FOOTER_BG, line: LINE }
}

/// Service status; None after `timeout`.
fn service_status(timeout: Duration) -> Option<Status> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::ipc::request("status"));
    });
    rx.recv_timeout(timeout).ok().flatten().map(|s| Status::decode(&s))
}

fn open_form(store: Store, srv: Option<Arc<Server>>, standalone: bool) {
    let s = srv.as_ref().map_or_else(|| store.load(), |s| (*s.settings()).clone());
    let service = matches!(store, Store::Registry);
    let has_pw = s.password.is_some();
    let notice = if !has_pw {
        Some("No password set! Praeter will not allow inbound connections.".into())
    } else if service && crate::install::service_state() != Some(SERVICE_RUNNING) {
        Some("The service is stopped. Settings apply when it starts.".into())
    } else if service {
        service_status(Duration::from_secs(1)).and_then(|s| s.error)
    } else {
        srv.as_ref().and_then(|s| s.status().error)
    };
    unsafe {
        let dpi = GetDpiForSystem();
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let h = HEAD_H + if notice.is_some() { NOTICE_H } else { 0 } + BODY_H + FOOT_H;
        let mut r = RECT { left: 0, top: 0, right: scale(FORM_W, dpi), bottom: scale(h, dpi) };
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, WS_EX_CONTROLPARENT, dpi);
        let title = if service { SERVICE_TITLE } else { w!("PraeterVNC Settings") };
        let Ok(hwnd) = CreateWindowExW(WS_EX_CONTROLPARENT, FORM_CLASS, title, style, CW_USEDEFAULT, CW_USEDEFAULT, r.right - r.left, r.bottom - r.top, None, None, Some(hinst()), None) else { return };
        let mut f = Form {
            hwnd, store, srv, has_pw, port: s.port, notice, host: host_name(), addr_ok: Cell::new(false), dpi: GetDpiForWindow(hwnd), pal: Palette::default(),
            font: HFONT::default(), strong: HFONT::default(), head: HFONT::default(), title: HFONT::default(),
            icon: HICON::default(), warn_icon: HICON::default(),
            bg: HBRUSH::default(), warn: HBRUSH::default(), footer: HBRUSH::default(), line: HBRUSH::default(),
            ctrls: Vec::new(), standalone,
        };
        f.build(&s, service);
        FORM.with(|x| *x.borrow_mut() = Some(f));
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
        if let Ok(e) = GetDlgItem(Some(hwnd), if has_pw { ID_PORT } else { ID_PW }) {
            let _ = SetFocus(Some(e));
        }
    }
}

/// Draws single-line `t` in `r`.
unsafe fn draw(hdc: HDC, t: &str, mut r: RECT, f: DRAW_TEXT_FORMAT) {
    let mut v: Vec<u16> = t.encode_utf16().collect();
    DrawTextW(hdc, &mut v, &mut r, f | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER);
}

unsafe fn line_height(hdc: HDC, f: HFONT) -> i32 {
    SelectObject(hdc, f.into());
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);
    tm.tmHeight
}

unsafe fn gradient(hdc: HDC, r: &RECT) {
    let v = |x: i32, y: i32, c: COLORREF| TRIVERTEX { x, y, Red: ((c.0 & 0xFF) << 8) as u16, Green: (c.0 & 0xFF00) as u16, Blue: (c.0 >> 8 & 0xFF00) as u16, Alpha: 0 };
    let vs = [v(r.left, r.top, BRAND[0]), v(r.right, r.bottom, BRAND[1])];
    let m = GRADIENT_RECT { UpperLeft: 0, LowerRight: 1 };
    let _ = GradientFill(hdc, &vs, &m as *const _ as *const _, 1, GRADIENT_FILL_RECT_H);
}

impl Form {
    fn add(&mut self, class: PCWSTR, text: &str, style: i32, ex: WINDOW_EX_STYLE, id: i32, r: (i32, i32, i32, i32)) -> HWND {
        unsafe {
            let h = CreateWindowExW(ex, class, &HSTRING::from(text), WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style as u32), 0, 0, 0, 0, Some(self.hwnd), Some(HMENU(id as usize as *mut _)), Some(hinst()), None)
                .unwrap_or_default();
            self.ctrls.push((h, r));
            h
        }
    }

    fn top(&self) -> i32 {
        HEAD_H + if self.notice.is_some() { NOTICE_H } else { 0 }
    }

    fn build(&mut self, s: &Settings, service: bool) {
        let tab = WS_TABSTOP.0 as i32;
        let edit = WS_EX_CLIENTEDGE;
        let none = WINDOW_EX_STYLE(0);
        let (lx, fx, fw) = (20, 200, 240);
        let rw = FORM_W - 2 * lx;
        if let Some(n) = self.notice.clone() {
            self.add(w!("STATIC"), &n, 0x200, none, ID_INFO, (48, HEAD_H, FORM_W - 68, NOTICE_H));
        }
        let y = self.top() + 16;
        let head = |f: &mut Form, t: &str, dy: i32| {
            f.add(w!("STATIC"), t, 0, none, ID_HEAD, (lx, y + dy, rw, 22));
        };
        let label = |f: &mut Form, t: &str, dy: i32| {
            f.add(w!("STATIC"), t, 0, none, -1, (lx, y + dy + 4, fx - lx - 8, 20));
        };

        head(self, "Connection", 0);
        label(self, "Port", 28);
        let port = self.add(w!("EDIT"), &s.port.to_string(), tab | ES_NUMBER | ES_AUTOHSCROLL, edit, ID_PORT, (fx, y + 28, 80, 24));
        label(self, "Address", 60);
        self.add(w!("EDIT"), "", tab | ES_READONLY | ES_AUTOHSCROLL, none, ID_ADDR, (fx, y + 64, fw - 76, 20));
        self.add(w!("BUTTON"), "Copy", tab | BS_PUSHBUTTON, none, ID_COPY, (fx + fw - 68, y + 59, 68, 26));
        let remote = self.add(w!("BUTTON"), "Allow connections from other computers", tab | BS_AUTOCHECKBOX, none, ID_REMOTE, (lx, y + 94, rw, 22));

        head(self, "Security", 146);
        label(self, "Password", 174);
        let pw = self.add(w!("EDIT"), "", tab | ES_PASSWORD | ES_AUTOHSCROLL, edit, ID_PW, (fx, y + 174, fw, 24));
        label(self, "Confirm password", 206);
        let pw2 = self.add(w!("EDIT"), "", tab | ES_PASSWORD | ES_AUTOHSCROLL, edit, ID_PW2, (fx, y + 206, fw, 24));
        label(self, "Failed logins before block", 238);
        self.add(w!("EDIT"), &s.max_attempts.to_string(), tab | ES_NUMBER, edit, ID_ATTEMPTS, (fx, y + 238, 60, 24));
        self.add(w!("STATIC"), "0 = no limit", 0, none, ID_HINT, (fx + 70, y + 242, fw - 70, 20));
        label(self, "Block time", 270);
        self.add(w!("EDIT"), &s.block_secs.to_string(), tab | ES_NUMBER, edit, ID_BLOCK, (fx, y + 270, 60, 24));
        self.add(w!("STATIC"), "seconds", 0, none, ID_HINT, (fx + 70, y + 274, fw - 70, 20));

        head(self, "Display and input", 322);
        label(self, "Monitor", 350);
        let mon = self.add(w!("COMBOBOX"), "", tab | CBS_DROPDOWNLIST | WS_VSCROLL.0 as i32, none, ID_MONITOR, (fx, y + 350, fw, 200));
        let vo = self.add(w!("BUTTON"), "View only (ignore viewer mouse and keyboard)", tab | BS_AUTOCHECKBOX, none, ID_VIEWONLY, (lx, y + 384, rw, 22));

        let by = self.top() + BODY_H + 14;
        if !service {
            self.add(w!("BUTTON"), "Install as service…", tab | BS_PUSHBUTTON, none, ID_INSTALL, (lx, by, 150, 28));
        }
        self.add(w!("BUTTON"), "Save", tab | BS_DEFPUSHBUTTON, none, IDOK.0, (FORM_W - lx - 88 - 8 - 88, by, 88, 28));
        self.add(w!("BUTTON"), "Cancel", tab | BS_PUSHBUTTON, none, IDCANCEL.0, (FORM_W - lx - 88, by, 88, 28));
        unsafe {
            SendMessageW(port, EM_LIMITTEXT, Some(WPARAM(5)), None);
            for (h, cue) in [(pw, if self.has_pw { "Leave empty to keep current" } else { "Up to 8 characters" }), (pw2, "")] {
                let c = wide(cue);
                SendMessageW(h, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(c.as_ptr() as isize)));
                SendMessageW(h, EM_LIMITTEXT, Some(WPARAM(8)), None);
            }
            let add = |t: &str, data: usize| {
                let v = wide(t);
                let i = SendMessageW(mon, CB_ADDSTRING, None, Some(LPARAM(v.as_ptr() as isize)));
                SendMessageW(mon, CB_SETITEMDATA, Some(WPARAM(i.0 as usize)), Some(LPARAM(data as isize)));
                if s.monitor.map_or(0, |m| m + 1) == data {
                    SendMessageW(mon, CB_SETCURSEL, Some(WPARAM(i.0 as usize)), None);
                }
            };
            add("All monitors", 0);
            let outs = crate::capture::enum_outputs();
            let primary = outs.iter().find(|o| o.rect.left == 0 && o.rect.top == 0).map(|o| o.rect);
            for (i, o) in outs.iter().enumerate() {
                let r = o.rect;
                let at = match primary {
                    Some(p) if p == r => ", primary",
                    Some(p) if r.left >= p.right => ", right",
                    Some(p) if r.right <= p.left => ", left",
                    Some(p) if r.top >= p.bottom => ", below",
                    Some(p) if r.bottom <= p.top => ", above",
                    _ => "",
                };
                add(&format!("Monitor {}: {}×{}{at}", i + 1, r.right - r.left, r.bottom - r.top), i + 1);
            }
            if let Some(m) = s.monitor.filter(|&m| m >= outs.len()) {
                add(&format!("Monitor {}: not connected", m + 1), m + 1);
            }
            SendMessageW(remote, BM_SETCHECK, Some(WPARAM(s.remote as usize)), None);
            SendMessageW(vo, BM_SETCHECK, Some(WPARAM(s.view_only as usize)), None);
        }
        self.layout();
        self.update_addr();
    }

    /// Fonts, icons, colours and control positions for the current DPI.
    fn layout(&mut self) {
        unsafe {
            let d = self.dpi;
            let mut ncm = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
            let _ = SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, ncm.cbSize, Some(&mut ncm as *mut _ as *mut _), 0, d);
            let base = ncm.lfMessageFont;
            let font = |weight: i32, size: i32| {
                let mut lf = base;
                lf.lfWeight = weight;
                lf.lfHeight = lf.lfHeight * size / 10;
                CreateFontIndirectW(&lf)
            };
            let old = [self.font, self.strong, self.head, self.title];
            self.font = CreateFontIndirectW(&base);
            self.strong = font(600, 10);
            self.head = font(600, 11);
            self.title = font(600, 17);
            for o in old.into_iter().filter(|o| !o.is_invalid()) {
                let _ = DeleteObject(o.into());
            }
            if !self.icon.is_invalid() {
                let _ = DestroyIcon(self.icon);
            }
            let ic = scale(32, d);
            self.icon = LoadImageW(Some(hinst()), res(1), IMAGE_ICON, ic, ic, LR_DEFAULTCOLOR).map(|h| HICON(h.0)).unwrap_or_default();
            let cx = GetSystemMetricsForDpi(SM_CXSMICON, d);
            // System icons load only as shared; never destroyed.
            self.warn_icon = LoadImageW(None, res(32515), IMAGE_ICON, cx, cx, LR_SHARED).map(|h| HICON(h.0)).unwrap_or_default();
            self.pal = palette();
            for b in [self.bg, self.warn, self.footer, self.line].into_iter().filter(|b| !b.is_invalid()) {
                let _ = DeleteObject(b.into());
            }
            let p = self.pal;
            (self.bg, self.warn, self.footer, self.line) = (CreateSolidBrush(p.window), CreateSolidBrush(p.warn), CreateSolidBrush(p.footer), CreateSolidBrush(p.line));
            for &(h, (x, y, w, hh)) in &self.ctrls {
                let _ = SetWindowPos(h, None, scale(x, d), scale(y, d), scale(w, d), scale(hh, d), SWP_NOZORDER | SWP_NOACTIVATE);
                let f = match GetDlgCtrlID(h) {
                    ID_HEAD => self.head,
                    ID_ADDR if self.addr_ok.get() => self.strong,
                    _ => self.font,
                };
                SendMessageW(h, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));
            }
            let _ = InvalidateRect(Some(self.hwnd), None, true);
        }
    }

    fn paint(&self, hdc: HDC) {
        unsafe {
            let s = |v: i32| scale(v, self.dpi);
            let p = self.pal;
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);
            let head = RECT { left: 0, top: 0, right: rc.right, bottom: s(HEAD_H) };
            if p.hc {
                FillRect(hdc, &head, self.bg);
            } else {
                gradient(hdc, &head);
            }
            let ic = s(32);
            let _ = DrawIconEx(hdc, s(20), (head.bottom - ic) / 2, self.icon, ic, ic, 0, None, DI_NORMAL);
            let old = SelectObject(hdc, self.font.into());
            let (th, sh) = (line_height(hdc, self.title), line_height(hdc, self.font));
            let (x, y, right) = (s(20) + ic + s(14), (head.bottom - th - sh) / 2, rc.right - s(20));
            SetBkMode(hdc, TRANSPARENT);
            SelectObject(hdc, self.title.into());
            SetTextColor(hdc, if p.hc { p.text } else { ON_BRAND });
            draw(hdc, "PraeterVNC", RECT { left: x, top: y, right, bottom: y + th }, DT_LEFT);
            SelectObject(hdc, self.font.into());
            SetTextColor(hdc, if p.hc { p.text } else { ON_BRAND_DIM });
            let sub = if matches!(self.store, Store::Registry) { "Windows service" } else { "Portable" };
            let r = RECT { left: x, top: y + th, right, bottom: y + th + sh };
            draw(hdc, sub, r, DT_LEFT);
            draw(hdc, concat!("v", env!("CARGO_PKG_VERSION")), r, DT_RIGHT);
            SelectObject(hdc, old);
            if self.notice.is_some() {
                FillRect(hdc, &RECT { left: 0, top: s(HEAD_H), right: rc.right, bottom: s(HEAD_H + NOTICE_H) }, self.warn);
                let cx = GetSystemMetricsForDpi(SM_CXSMICON, self.dpi);
                let _ = DrawIconEx(hdc, s(20), s(HEAD_H) + (s(NOTICE_H) - cx) / 2, self.warn_icon, cx, cx, 0, None, DI_NORMAL);
            }
            let px = s(1).max(1);
            let y0 = self.top() + 16;
            for d in DIVIDERS {
                FillRect(hdc, &RECT { left: s(20), top: s(y0 + d), right: rc.right - s(20), bottom: s(y0 + d) + px }, self.line);
            }
            let fy = s(self.top() + BODY_H);
            FillRect(hdc, &RECT { left: 0, top: fy, right: rc.right, bottom: rc.bottom }, self.footer);
            FillRect(hdc, &RECT { left: 0, top: fy, right: rc.right, bottom: fy + px }, self.line);
        }
    }

    fn text(&self, id: i32) -> String {
        unsafe {
            let Ok(h) = GetDlgItem(Some(self.hwnd), id) else { return String::new() };
            let mut b = [0u16; 256];
            let n = GetWindowTextW(h, &mut b);
            String::from_utf16_lossy(&b[..n.max(0) as usize])
        }
    }

    fn checked(&self, id: i32) -> bool {
        unsafe { GetDlgItem(Some(self.hwnd), id).is_ok_and(|h| SendMessageW(h, BM_GETCHECK, None, None).0 == 1) }
    }

    /// Where viewers would connect with the current field values.
    fn update_addr(&self) {
        let port = self.text(ID_PORT).trim().parse::<u16>().ok().filter(|&p| p > 0);
        let pw = self.has_pw || !self.text(ID_PW).is_empty();
        let service = matches!(self.store, Store::Registry);
        let (text, ok) = match port {
            None => ("Invalid port".to_string(), false),
            Some(_) if !pw && service => ("Needs a password".into(), false),
            Some(p) if pw && self.checked(ID_REMOTE) => (format!("{}:{p}", self.host), true),
            Some(p) => (format!("localhost:{p} only"), false),
        };
        self.addr_ok.set(ok);
        unsafe {
            if let Ok(a) = GetDlgItem(Some(self.hwnd), ID_ADDR) {
                let f = if ok { self.strong } else { self.font };
                SendMessageW(a, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(0)));
                let _ = SetWindowTextW(a, &HSTRING::from(text));
            }
            if let Ok(b) = GetDlgItem(Some(self.hwnd), ID_COPY) {
                let _ = ShowWindow(b, if ok { SW_SHOWNA } else { SW_HIDE });
            }
        }
    }

    fn port_conflict(&self) -> Option<String> {
        let port = self.text(ID_PORT).trim().parse::<u16>().ok().filter(|&p| p > 0 && p != self.port)?;
        let owner = crate::net::port_owner(port)?;
        Some(format!("Port {port} is in use by {owner}. Save anyway?"))
    }

    /// Validates and saves; Err is shown to the user.
    fn save(&self) -> Result<(), String> {
        let port: u16 = self.text(ID_PORT).trim().parse().ok().filter(|&p| p > 0).ok_or("Port must be between 1 and 65535.")?;
        let (pw, pw2) = (self.text(ID_PW), self.text(ID_PW2));
        if pw != pw2 {
            return Err("The passwords don't match.".into());
        }
        if !pw.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
            return Err("Passwords can only use A-Z, 0-9, spaces and symbols.".into());
        }
        let service = matches!(self.store, Store::Registry);
        if service && pw.is_empty() && !self.has_pw {
            return Err("The service needs a password.".into());
        }
        let max_attempts = self.text(ID_ATTEMPTS).trim().parse().map_err(|_| "Enter a number of failed logins (0 for no limit).")?;
        let block_secs = self.text(ID_BLOCK).trim().parse().map_err(|_| "Enter a block time in seconds.")?;
        let monitor = unsafe {
            GetDlgItem(Some(self.hwnd), ID_MONITOR).map_or(0, |h| {
                let sel = SendMessageW(h, CB_GETCURSEL, None, None).0;
                if sel < 0 { 0 } else { SendMessageW(h, CB_GETITEMDATA, Some(WPARAM(sel as usize)), None).0 as usize }
            })
        };
        let s = Settings {
            port,
            password: None,
            remote: self.checked(ID_REMOTE),
            view_only: self.checked(ID_VIEWONLY),
            monitor: monitor.checked_sub(1),
            max_attempts,
            block_secs,
        };
        let pw = (!pw.is_empty()).then_some(pw);
        self.store.save(&s, pw.as_deref().map(Some)).map_err(|e| format!("Couldn't save settings: {e}"))?;
        if let Some(srv) = &self.srv {
            srv.apply(self.store.load());
        }
        Ok(())
    }
}

/// Confirms a port conflict, then saves; errors are shown. True if saved.
unsafe fn submit(h: HWND) -> bool {
    if let Some(q) = FORM.with(|f| f.borrow().as_ref().and_then(|f| f.port_conflict())) {
        if MessageBoxW(Some(h), &HSTRING::from(q), w!("PraeterVNC"), MB_ICONWARNING | MB_YESNO | MB_DEFBUTTON2) != IDYES {
            return false;
        }
    }
    match FORM.with(|f| f.borrow().as_ref().map(|f| f.save())) {
        Some(Ok(())) => true,
        Some(Err(e)) => {
            MessageBoxW(Some(h), &HSTRING::from(e), w!("PraeterVNC"), MB_ICONWARNING | MB_OK);
            false
        }
        None => false,
    }
}

unsafe extern "system" fn form_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match m {
        WM_COMMAND => {
            let (id, code) = ((wp.0 & 0xFFFF) as i32, (wp.0 >> 16 & 0xFFFF) as u32);
            match id {
                x if x == IDOK.0 => {
                    if submit(h) {
                        let _ = DestroyWindow(h);
                    }
                }
                x if x == IDCANCEL.0 => {
                    let _ = DestroyWindow(h);
                }
                ID_INSTALL => {
                    if submit(h) {
                        install_from_app(Some(h));
                    }
                }
                ID_COPY => {
                    let addr = FORM.with(|f| f.try_borrow().ok().and_then(|f| f.as_ref().map(|f| f.text(ID_ADDR))));
                    if let Some(a) = addr {
                        crate::clipboard::set_text(&a, 0);
                        let _ = SetDlgItemTextW(h, ID_COPY, w!("Copied"));
                        SetTimer(Some(h), T_COPIED, 1500, None);
                    }
                }
                ID_PORT | ID_PW if code == EN_CHANGE => FORM.with(|f| {
                    if let Some(f) = f.try_borrow().ok().as_deref().and_then(Option::as_ref) {
                        f.update_addr();
                    }
                }),
                ID_REMOTE => FORM.with(|f| {
                    if let Some(f) = f.try_borrow().ok().as_deref().and_then(Option::as_ref) {
                        f.update_addr();
                    }
                }),
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == T_COPIED => {
            let _ = KillTimer(Some(h), T_COPIED);
            let _ = SetDlgItemTextW(h, ID_COPY, w!("Copy"));
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(h, &mut ps);
            FORM.with(|f| {
                if let Ok(f) = f.try_borrow() {
                    if let Some(f) = f.as_ref() {
                        f.paint(hdc);
                    }
                }
            });
            let _ = EndPaint(h, &ps);
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let Some((p, bg, warn, footer, addr_ok)) = FORM.with(|f| f.try_borrow().ok().and_then(|f| f.as_ref().map(|f| (f.pal, f.bg, f.warn, f.footer, f.addr_ok.get())))) else {
                return DefWindowProcW(h, m, wp, lp);
            };
            let hdc = HDC(wp.0 as *mut _);
            let id = GetDlgCtrlID(HWND(lp.0 as *mut _));
            SetTextColor(hdc, match id {
                ID_HEAD => p.accent,
                ID_HINT => p.hint,
                ID_ADDR if !addr_ok => p.hint,
                _ => p.text,
            });
            let (c, brush) = match id {
                ID_INFO => (p.warn, warn),
                ID_COPY => (p.window, bg),
                _ if m == WM_CTLCOLORBTN => (p.footer, footer),
                _ => (p.window, bg),
            };
            SetBkColor(hdc, c);
            LRESULT(brush.0 as isize)
        }
        WM_SYSCOLORCHANGE | WM_SETTINGCHANGE => {
            FORM.with(|f| {
                if let Some(f) = f.try_borrow_mut().ok().as_deref_mut().and_then(Option::as_mut) {
                    f.layout();
                }
            });
            DefWindowProcW(h, m, wp, lp)
        }
        WM_DPICHANGED => {
            let r = &*(lp.0 as *const RECT);
            FORM.with(|f| {
                if let Some(f) = f.borrow_mut().as_mut() {
                    f.dpi = (wp.0 & 0xFFFF) as u32;
                    f.layout();
                }
            });
            let _ = SetWindowPos(h, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(h);
            LRESULT(0)
        }
        WM_DESTROY => {
            let f = FORM.with(|f| f.borrow_mut().take());
            if let Some(f) = f {
                for o in [f.font, f.strong, f.head, f.title].into_iter().filter(|o| !o.is_invalid()) {
                    let _ = DeleteObject(o.into());
                }
                for b in [f.bg, f.warn, f.footer, f.line] {
                    let _ = DeleteObject(b.into());
                }
                if !f.icon.is_invalid() {
                    let _ = DestroyIcon(f.icon);
                }
                if f.standalone {
                    PostQuitMessage(0);
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}
