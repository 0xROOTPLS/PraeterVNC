#![windows_subsystem = "windows"]
use praetervnc::settings::Store;
use praetervnc::{capture, gui, install, server::Server, service, Config};
use windows::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "praetervnc                       portable server with tray icon (settings in praetervnc.ini)
praetervnc [options]             console server
  --port N  --bind ADDR  --password PW  --monitor N | --list  --view-only  --verbose
  --inflight N  --alr-ms N  --budget-ms N  --no-scroll  --no-motion  --no-pipeline
praetervnc --install | --uninstall   install or remove the Windows service (admin)
praetervnc --start | --stop          start or stop the service (admin)
praetervnc --settings | --tray        service settings / service tray icon
praetervnc --licenses                license and third-party notices";

const LICENSES: &str = concat!(include_str!("../../../LICENSE"), "\n", include_str!("../../../THIRD-PARTY-NOTICES.txt"));

fn console() -> bool {
    unsafe { GetStdHandle(STD_ERROR_HANDLE).is_ok_and(|h| !h.is_invalid() && !h.0.is_null()) }
}

fn report(msg: &str, error: bool) {
    if console() {
        eprintln!("{msg}");
    } else {
        gui::message(msg, error);
    }
}

fn usage() -> ! {
    report(USAGE, true);
    std::process::exit(2);
}

fn main() {
    unsafe {
        if !console() {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        windows::Win32::Media::timeBeginPeriod(1);
    }
    if !is_x86_feature_detected!("avx2") {
        report("PraeterVNC requires a CPU with AVX2.", true);
        std::process::exit(1);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None if in_install_dir() => gui::service_tray(),
        None => gui::app(false),
        Some("--restarted") => gui::app(true),
        Some("--service") => service::run(),
        Some("--helper") => service::helper(&args[1..]),
        Some("--tray") => gui::service_tray(),
        Some("--settings") => elevate_or(gui::settings_standalone, "--settings"),
        Some("--install") => elevate_or(|| finish(install::install()), "--install"),
        Some("--uninstall") => elevate_or(|| finish(install::uninstall()), "--uninstall"),
        Some("--start") => elevate_or(|| finish(install::start().map(|_| String::new())), "--start"),
        Some("--stop") => elevate_or(|| finish(install::stop().map(|_| String::new())), "--stop"),
        #[cfg(debug_assertions)]
        Some("--preview") => gui::preview(args.get(1).map_or(Store::Registry, |p| Store::Ini(p.into()))),
        #[cfg(debug_assertions)]
        Some("--preview-tray") => gui::preview_tray(args.get(1).map(|p| Store::Ini(p.into()))),
        Some("--help" | "-h" | "/?") => usage(),
        Some("--licenses") if console() => println!("{LICENSES}"),
        Some("--licenses") => report("Run praetervnc --licenses from a command prompt, or see THIRD-PARTY-NOTICES.txt.", false),
        _ => headless(&args),
    }
}

fn in_install_dir() -> bool {
    praetervnc::settings::exe_path().parent().is_some_and(|d| d.as_os_str().eq_ignore_ascii_case(install::install_dir()))
}

/// Runs `f` if elevated, else relaunches with UAC and exits with its code.
fn elevate_or(f: impl FnOnce(), arg: &str) {
    if install::elevated() {
        return f();
    }
    match install::run_elevated(&praetervnc::settings::exe_path(), arg) {
        Some(c) => std::process::exit(c as i32),
        None => std::process::exit(1223),
    }
}

fn finish(r: Result<String, String>) {
    match r {
        Ok(m) if m.is_empty() => {}
        Ok(m) => report(&m, false),
        Err(e) => {
            report(&e, true);
            std::process::exit(1);
        }
    }
}

/// Console server: portable settings overridden by flags.
fn headless(args: &[String]) {
    let mut s = Store::portable().load();
    let mut cfg = Config::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().unwrap_or_else(|| usage());
        match a.as_str() {
            "--port" => s.port = val().parse().unwrap_or_else(|_| usage()),
            "--bind" => cfg.bind = Some(val()),
            "--password" => s.password = Some(val()),
            "--monitor" => s.monitor = Some(val().parse().unwrap_or_else(|_| usage())),
            "--name" => cfg.name = val(),
            "--view-only" => s.view_only = true,
            "--verbose" | "-v" => cfg.verbose = true,
            "--inflight" => cfg.max_inflight = val().parse().unwrap_or_else(|_| usage()),
            "--alr-ms" => cfg.alr_ms = val().parse().unwrap_or_else(|_| usage()),
            "--no-scroll" => cfg.scroll = false,
            "--no-motion" => cfg.motion = false,
            "--no-pipeline" => cfg.pipeline = false,
            "--budget-ms" => cfg.budget_ms = val().parse().unwrap_or_else(|_| usage()),
            "--list" => {
                for (i, o) in capture::enum_outputs().iter().enumerate() {
                    println!("{i}: {} {}x{} at {},{}", o.name, o.rect.right - o.rect.left, o.rect.bottom - o.rect.top, o.rect.left, o.rect.top);
                }
                return;
            }
            _ => usage(),
        }
    }
    let _srv = Server::start(cfg, s);
    loop {
        std::thread::park();
    }
}
