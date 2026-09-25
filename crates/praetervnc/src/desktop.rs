//! Following the input desktop (Default <-> Winlogon) when running as SYSTEM.
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{GENERIC_ALL, HANDLE};
use windows::Win32::System::StationsAndDesktops::*;
use windows::Win32::System::Threading::GetCurrentThreadId;

pub static FOLLOW: AtomicBool = AtomicBool::new(false);

thread_local! {
    static OPENED: RefCell<Option<HDESK>> = const { RefCell::new(None) };
    static CHECKED: Cell<Option<Instant>> = const { Cell::new(None) };
}

fn name(h: HDESK) -> [u16; 64] {
    let mut b = [0u16; 64];
    unsafe {
        let _ = GetUserObjectInformationW(HANDLE(h.0), UOI_NAME, Some(b.as_mut_ptr() as *mut _), 128, None);
    }
    b
}

/// Moves the calling thread (which must own no windows) to the input desktop if it changed.
pub fn follow_input() -> bool {
    if !FOLLOW.load(Ordering::Relaxed) {
        return false;
    }
    unsafe {
        let Ok(d) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0)) else { return false };
        let same = GetThreadDesktop(GetCurrentThreadId()).is_ok_and(|c| name(c) == name(d));
        if same || SetThreadDesktop(d).is_err() {
            let _ = CloseDesktop(d);
            return false;
        }
        if let Some(p) = OPENED.with(|o| o.replace(Some(d))) {
            let _ = CloseDesktop(p);
        }
        true
    }
}

/// `follow_input` at most every 16 ms per thread.
pub fn follow_input_lazy() {
    if !FOLLOW.load(Ordering::Relaxed) {
        return;
    }
    let t = Instant::now();
    if CHECKED.get().is_none_or(|c| t - c >= Duration::from_millis(16)) {
        CHECKED.set(Some(t));
        follow_input();
    }
}
