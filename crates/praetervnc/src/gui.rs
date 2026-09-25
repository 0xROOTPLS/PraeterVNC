//! Tray icon and settings window.
use crate::server::{Server, Status};
use crate::settings::{Settings, Store};
use parking_lot::Mutex;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::Win32::System::Services::{SERVICE_RUNNING, SERVICE_STATUS_CURRENT_STATE};
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

/// Form geometry in DIPs.
const FORM_W: i32 = 440;
const BANNER_H: i32 = 52;
const FOOTER_Y: i32 = 448;
const FORM_H: i32 = 504;
const DIVIDERS: [i32; 2] = [162, 336];

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}
const WARN_BG: COLORREF = rgb(0xFF, 0xF4, 0xCE);
const OK_BG: COLORREF = rgb(0xDF, 0xF6, 0xDD);
const TEXT: COLORREF = rgb(0x1B, 0x1B, 0x1B);
const ACCENT: COLORREF = rgb(0x1A, 0x52, 0x99);
const HINT: COLORREF = rgb(0x6B, 0x6B, 0x6B);
const FOOTER_BG: COLORREF = rgb(0xF3, 0xF3, 0xF3);
const LINE: COLORREF = rgb(0xE0, 0xE0, 0xE0);

const M_SETTINGS: usize = 1;
const M_DISCONNECT: usize = 2;
const M_INSTALL: usize = 3;
const M_EXIT: usize = 4;
const M_RELEASE: usize = 5;

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
}

struct Form {
    hwnd: HWND,
    store: Store,
    srv: Option<Arc<Server>>,
    has_pw: bool,
    dpi: u32,
    font: HFONT,
    bold: HFONT,
    icon: HICON,
    banner: HBRUSH,
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

fn load_icon(id: u16, dpi: u32) -> HICON {
    unsafe {
        let cx = GetSystemMetricsForDpi(SM_CXSMICON, dpi);
        LoadImageW(Some(hinst()), PCWSTR(id as usize as *const u16), IMAGE_ICON, cx, cx, LR_DEFAULTCOLOR)
            .map(|h| HICON(h.0))
            .or_else(|_| LoadIconW(None, IDI_APPLICATION))
            .unwrap_or_default()
    }
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

fn register() {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let icon = LoadImageW(Some(hinst()), PCWSTR(1 as *const u16), IMAGE_ICON, 0, 0, LR_DEFAULTSIZE).map(|h| HICON(h.0)).unwrap_or_default();
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
                taskbar: RegisterWindowMessageW(w!("TaskbarCreated")),
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
        (Some(l), _) => format!("Listening on {l}"),
        (None, None) => "Starting…".into(),
    }
}

/// Re-reads status, updates the icon and tooltip, and announces new viewers; false once the service is gone.
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
        let known: Vec<String> = t.status.clients.iter().map(|c| c.0.clone()).collect();
        let new: Vec<String> = if t.polled && t.reachable { st.clients.iter().filter(|c| !known.contains(&c.0)).map(|c| c.0.clone()).collect() } else { Vec::new() };
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
        if let Some(p) = new.first() {
            nid.uFlags |= NIF_INFO;
            nid.dwInfoFlags = NIIF_INFO;
            copy_to(&mut nid.szInfoTitle, "Viewer connected");
            copy_to(&mut nid.szInfo, &format!("{} is viewing this computer.", ip_of(p)));
        }
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
        true
    })
}

fn show_menu() {
    let Some((hwnd, lines, clients, app)) = TRAY.with(|t| {
        let t = t.borrow();
        let t = t.as_ref()?;
        let mut lines = vec![headline(t)];
        if t.reachable && t.status.listen.is_some() && t.status.error.is_some() {
            lines.push(t.status.error.clone().unwrap_or_default());
        }
        let clients: Vec<String> = t.status.clients.iter().map(|(p, s)| format!("{}  ·  {}", ip_of(p), fmt_dur(*s))).collect();
        Some((t.hwnd, lines, clients, matches!(t.mode, Mode::App(_))))
    }) else { return };
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        for l in &lines {
            let _ = AppendMenuW(m, MF_STRING | MF_GRAYED, 0, &HSTRING::from(l.as_str()));
        }
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        if clients.is_empty() {
            let _ = AppendMenuW(m, MF_STRING | MF_GRAYED, 0, w!("No viewers connected"));
        }
        for c in &clients {
            let _ = AppendMenuW(m, MF_STRING | MF_GRAYED, 0, &HSTRING::from(c.as_str()));
        }
        let dis = if clients.is_empty() { MF_GRAYED } else { MF_STRING };
        let _ = AppendMenuW(m, MF_STRING | dis, M_DISCONNECT, w!("Disconnect all"));
        let _ = AppendMenuW(m, MF_STRING, M_RELEASE, w!("Release stuck keys"));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, M_SETTINGS, w!("Settings…"));
        if app {
            let _ = AppendMenuW(m, MF_STRING, M_INSTALL, w!("Install as service…"));
        }
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
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
            M_INSTALL => install_from_app(None),
            M_EXIT => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
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
            // One UAC prompt at a time.
            static BUSY: AtomicBool = AtomicBool::new(false);
            if BUSY.swap(true, Ordering::AcqRel) {
                return;
            }
            let exe = crate::settings::exe_path();
            std::thread::spawn(move || {
                crate::install::run_elevated(&exe, "--settings");
                BUSY.store(false, Ordering::Release);
            });
        }
    }
}

/// Installs the service from the portable app, then hands over to the installed tray.
fn install_from_app(owner: Option<HWND>) {
    let exe = crate::settings::exe_path();
    match crate::install::run_elevated(&exe, "--install") {
        Some(0) => {
            let srv = TRAY.with(|t| t.borrow().as_ref().and_then(|t| if let Mode::App(s) = &t.mode { Some(s.clone()) } else { None }));
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

unsafe extern "system" fn tray_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let taskbar = TRAY.with(|t| t.try_borrow().ok().and_then(|t| t.as_ref().map(|t| t.taskbar))).unwrap_or(u32::MAX);
    match m {
        WM_TRAY => {
            match (lp.0 & 0xFFFF) as u32 {
                WM_CONTEXTMENU => show_menu(),
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

fn open_form(store: Store, srv: Option<Arc<Server>>, standalone: bool) {
    let s = srv.as_ref().map_or_else(|| store.load(), |s| (*s.settings()).clone());
    let service = matches!(store, Store::Registry);
    unsafe {
        let dpi = GetDpiForSystem();
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut r = RECT { left: 0, top: 0, right: scale(FORM_W, dpi), bottom: scale(FORM_H, dpi) };
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, WS_EX_CONTROLPARENT, dpi);
        let title = if service { SERVICE_TITLE } else { w!("PraeterVNC Settings") };
        let Ok(hwnd) = CreateWindowExW(WS_EX_CONTROLPARENT, FORM_CLASS, title, style, CW_USEDEFAULT, CW_USEDEFAULT, r.right - r.left, r.bottom - r.top, None, None, Some(hinst()), None) else { return };
        let has_pw = s.password.is_some();
        let mut f = Form {
            hwnd, store, srv, has_pw, dpi: GetDpiForWindow(hwnd),
            font: HFONT::default(), bold: HFONT::default(), icon: HICON::default(),
            banner: CreateSolidBrush(if has_pw { OK_BG } else { WARN_BG }),
            footer: CreateSolidBrush(FOOTER_BG),
            line: CreateSolidBrush(LINE),
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

impl Form {
    fn add(&mut self, class: PCWSTR, text: &str, style: i32, ex: WINDOW_EX_STYLE, id: i32, r: (i32, i32, i32, i32)) -> HWND {
        unsafe {
            let h = CreateWindowExW(ex, class, &HSTRING::from(text), WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style as u32), 0, 0, 0, 0, Some(self.hwnd), Some(HMENU(id as usize as *mut _)), Some(hinst()), None)
                .unwrap_or_default();
            self.ctrls.push((h, r));
            h
        }
    }

    fn build(&mut self, s: &Settings, service: bool) {
        let tab = WS_TABSTOP.0 as i32;
        let edit = WS_EX_CLIENTEDGE;
        let none = WINDOW_EX_STYLE(0);
        let (lx, fx, fw) = (20, 200, 220);
        let info = if self.has_pw { "Password set. Leave the password fields empty to keep it." } else { "No password set! Praeter will not allow inbound connections." };
        self.add(w!("STATIC"), info, 0x200, none, ID_INFO, (48, 0, FORM_W - 68, BANNER_H));
        let head = |f: &mut Form, t: &str, y: i32| {
            f.add(w!("STATIC"), t, 0, none, ID_HEAD, (lx, y, FORM_W - 2 * lx, 20));
        };
        let label = |f: &mut Form, t: &str, y: i32| {
            f.add(w!("STATIC"), t, 0, none, -1, (lx, y + 4, fx - lx - 8, 20));
        };

        head(self, "Connection", 68);
        label(self, "Port", 96);
        self.add(w!("EDIT"), &s.port.to_string(), tab | ES_NUMBER | ES_AUTOHSCROLL, edit, ID_PORT, (fx, 96, 80, 24));
        let remote = self.add(w!("BUTTON"), "Allow connections from other computers", tab | BS_AUTOCHECKBOX, none, ID_REMOTE, (lx, 128, FORM_W - 2 * lx, 22));

        head(self, "Security", 176);
        label(self, "Password", 204);
        let pw = self.add(w!("EDIT"), "", tab | ES_PASSWORD | ES_AUTOHSCROLL, edit, ID_PW, (fx, 204, fw, 24));
        label(self, "Confirm password", 236);
        let pw2 = self.add(w!("EDIT"), "", tab | ES_PASSWORD | ES_AUTOHSCROLL, edit, ID_PW2, (fx, 236, fw, 24));
        label(self, "Failed logins before block", 268);
        self.add(w!("EDIT"), &s.max_attempts.to_string(), tab | ES_NUMBER, edit, ID_ATTEMPTS, (fx, 268, 60, 24));
        self.add(w!("STATIC"), "0 = no limit", 0, none, ID_HINT, (fx + 70, 272, fw - 70, 20));
        label(self, "Block time", 300);
        self.add(w!("EDIT"), &s.block_secs.to_string(), tab | ES_NUMBER, edit, ID_BLOCK, (fx, 300, 60, 24));
        self.add(w!("STATIC"), "seconds", 0, none, ID_HINT, (fx + 70, 304, fw - 70, 20));

        head(self, "Display and input", 350);
        label(self, "Monitor", 378);
        let mon = self.add(w!("COMBOBOX"), "", tab | CBS_DROPDOWNLIST | WS_VSCROLL.0 as i32, none, ID_MONITOR, (fx, 378, fw, 200));
        let vo = self.add(w!("BUTTON"), "View only (ignore viewer mouse and keyboard)", tab | BS_AUTOCHECKBOX, none, ID_VIEWONLY, (lx, 410, FORM_W - 2 * lx, 22));

        let by = FOOTER_Y + 14;
        if !service {
            self.add(w!("BUTTON"), "Install as service…", tab | BS_PUSHBUTTON, none, ID_INSTALL, (lx, by, 150, 28));
        }
        self.add(w!("BUTTON"), "Save", tab | BS_DEFPUSHBUTTON, none, IDOK.0, (FORM_W - lx - 88 - 8 - 88, by, 88, 28));
        self.add(w!("BUTTON"), "Cancel", tab | BS_PUSHBUTTON, none, IDCANCEL.0, (FORM_W - lx - 88, by, 88, 28));
        unsafe {
            for (h, cue) in [(pw, if self.has_pw { "unchanged" } else { "up to 8 characters" }), (pw2, "")] {
                let c = wide(cue);
                SendMessageW(h, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(c.as_ptr() as isize)));
                SendMessageW(h, EM_LIMITTEXT, Some(WPARAM(8)), None);
            }
            let add = |t: &str| {
                let v = wide(t);
                SendMessageW(mon, CB_ADDSTRING, None, Some(LPARAM(v.as_ptr() as isize)));
            };
            add("All monitors");
            for (i, o) in crate::capture::enum_outputs().iter().enumerate() {
                add(&format!("Monitor {}: {}×{} at {},{}", i + 1, o.rect.right - o.rect.left, o.rect.bottom - o.rect.top, o.rect.left, o.rect.top));
            }
            SendMessageW(mon, CB_SETCURSEL, Some(WPARAM(s.monitor.map_or(0, |m| m + 1))), None);
            SendMessageW(remote, BM_SETCHECK, Some(WPARAM(s.remote as usize)), None);
            SendMessageW(vo, BM_SETCHECK, Some(WPARAM(s.view_only as usize)), None);
        }
        self.layout();
    }

    /// Fonts, icon and control positions for the current DPI.
    fn layout(&mut self) {
        unsafe {
            let mut ncm = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
            let _ = SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, ncm.cbSize, Some(&mut ncm as *mut _ as *mut _), 0, self.dpi);
            let (old, old_bold) = (self.font, self.bold);
            self.font = CreateFontIndirectW(&ncm.lfMessageFont);
            let mut lf = ncm.lfMessageFont;
            lf.lfWeight = 600;
            lf.lfHeight = lf.lfHeight * 11 / 10;
            self.bold = CreateFontIndirectW(&lf);
            let cx = GetSystemMetricsForDpi(SM_CXSMICON, self.dpi);
            let oic = if self.has_pw { 32516 } else { 32515 };
            // System icons load only as shared; never destroyed.
            self.icon = LoadImageW(None, PCWSTR(oic as *const u16), IMAGE_ICON, cx, cx, LR_SHARED).map(|h| HICON(h.0)).unwrap_or_default();
            let d = self.dpi;
            for &(h, (x, y, w, hh)) in &self.ctrls {
                let _ = SetWindowPos(h, None, scale(x, d), scale(y, d), scale(w, d), scale(hh, d), SWP_NOZORDER | SWP_NOACTIVATE);
                let f = if GetDlgCtrlID(h) == ID_HEAD { self.bold } else { self.font };
                SendMessageW(h, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));
            }
            for o in [old, old_bold] {
                if !o.is_invalid() {
                    let _ = DeleteObject(o.into());
                }
            }
            let _ = InvalidateRect(Some(self.hwnd), None, true);
        }
    }

    fn paint(&self, hdc: HDC) {
        unsafe {
            let s = |v: i32| scale(v, self.dpi);
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);
            FillRect(hdc, &RECT { left: 0, top: 0, right: rc.right, bottom: s(BANNER_H) }, self.banner);
            let cx = GetSystemMetricsForDpi(SM_CXSMICON, self.dpi);
            let _ = DrawIconEx(hdc, s(20), (s(BANNER_H) - cx) / 2, self.icon, cx, cx, 0, None, DI_NORMAL);
            let px = s(1).max(1);
            for y in DIVIDERS {
                FillRect(hdc, &RECT { left: s(20), top: s(y), right: rc.right - s(20), bottom: s(y) + px }, self.line);
            }
            FillRect(hdc, &RECT { left: 0, top: s(FOOTER_Y), right: rc.right, bottom: rc.bottom }, self.footer);
            FillRect(hdc, &RECT { left: 0, top: s(FOOTER_Y), right: rc.right, bottom: s(FOOTER_Y) + px }, self.line);
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

    /// Validates and saves; Err is shown to the user.
    fn save(&self) -> Result<(), String> {
        let port: u16 = self.text(ID_PORT).trim().parse().ok().filter(|&p| p > 0).ok_or("Port must be between 1 and 65535.")?;
        let (pw, pw2) = (self.text(ID_PW), self.text(ID_PW2));
        if pw != pw2 {
            return Err("The passwords don't match.".into());
        }
        let service = matches!(self.store, Store::Registry);
        if service && pw.is_empty() && !self.has_pw {
            return Err("The service needs a password.".into());
        }
        let max_attempts = self.text(ID_ATTEMPTS).trim().parse().map_err(|_| "Enter a number of failed logins (0 for no limit).")?;
        let block_secs = self.text(ID_BLOCK).trim().parse().map_err(|_| "Enter a block time in seconds.")?;
        let sel = unsafe { GetDlgItem(Some(self.hwnd), ID_MONITOR).map_or(0, |h| SendMessageW(h, CB_GETCURSEL, None, None).0) };
        let s = Settings {
            port,
            password: None,
            remote: self.checked(ID_REMOTE),
            view_only: self.checked(ID_VIEWONLY),
            monitor: (sel > 0).then(|| sel as usize - 1),
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

unsafe extern "system" fn form_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match m {
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as i32;
            match id {
                x if x == IDOK.0 => {
                    let r = FORM.with(|f| f.borrow().as_ref().map(|f| f.save()));
                    match r {
                        Some(Ok(())) => {
                            let _ = DestroyWindow(h);
                        }
                        Some(Err(e)) => {
                            MessageBoxW(Some(h), &HSTRING::from(e), w!("PraeterVNC"), MB_ICONWARNING | MB_OK);
                        }
                        None => {}
                    }
                }
                x if x == IDCANCEL.0 => {
                    let _ = DestroyWindow(h);
                }
                ID_INSTALL => {
                    let r = FORM.with(|f| f.borrow().as_ref().map(|f| f.save()));
                    match r {
                        Some(Ok(())) => install_from_app(Some(h)),
                        Some(Err(e)) => {
                            MessageBoxW(Some(h), &HSTRING::from(e), w!("PraeterVNC"), MB_ICONWARNING | MB_OK);
                        }
                        None => {}
                    }
                }
                _ => {}
            }
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
            let hdc = HDC(wp.0 as *mut _);
            let id = GetDlgCtrlID(HWND(lp.0 as *mut _));
            let (banner, footer, has_pw) = FORM.with(|f| f.try_borrow().ok().and_then(|f| f.as_ref().map(|f| (f.banner, f.footer, f.has_pw)))).unwrap_or_default();
            SetTextColor(hdc, match id {
                ID_HEAD => ACCENT,
                ID_HINT => HINT,
                _ => TEXT,
            });
            let (bg, brush) = match id {
                ID_INFO => (if has_pw { OK_BG } else { WARN_BG }, banner),
                _ if m == WM_CTLCOLORBTN => (FOOTER_BG, footer),
                _ => (COLORREF(GetSysColor(COLOR_WINDOW)), GetSysColorBrush(COLOR_WINDOW)),
            };
            SetBkColor(hdc, bg);
            LRESULT(brush.0 as isize)
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
                for o in [f.font, f.bold] {
                    if !o.is_invalid() {
                        let _ = DeleteObject(o.into());
                    }
                }
                for b in [f.banner, f.footer, f.line] {
                    let _ = DeleteObject(b.into());
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
