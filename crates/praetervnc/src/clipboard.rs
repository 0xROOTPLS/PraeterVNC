//! Server clipboard (CF_UNICODETEXT) <-> RFB cut text: Latin-1 legacy and ExtendedClipboard UTF-8.
use libz_rs_sys as z;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::*;
use windows::Win32::UI::WindowsAndMessaging::*;

static SEQ: AtomicU64 = AtomicU64::new(0);
/// Current text, LF line endings.
static TEXT: Mutex<Option<Arc<String>>> = Mutex::new(None);
/// Text a client set and its session id (not echoed back).
static FROM_CLIENT: Mutex<Option<(String, u64)>> = Mutex::new(None);
/// Session id that set the current text; 0 for local changes.
static ORIGIN: AtomicU64 = AtomicU64::new(0);
const CF_UNICODETEXT: u32 = 13;
const MAX: usize = 64 << 20;

pub const TEXT_FMT: u32 = 1;
pub const CAPS: u32 = 1 << 24;
pub const REQUEST: u32 = 1 << 25;
pub const PEEK: u32 = 1 << 26;
pub const NOTIFY: u32 = 1 << 27;
pub const PROVIDE: u32 = 1 << 28;
/// Assumed until the client sends its caps (spec default).
pub const DEFAULT_CLIENT_FLAGS: u32 = TEXT_FMT | 2 | 4 | REQUEST | NOTIFY | PROVIDE;
/// Largest text a client may push to us unannounced.
const OUR_TEXT_MAX: u32 = 1 << 20;

/// (change seq, text, originating session id or 0).
pub fn current() -> (u64, Option<Arc<String>>, u64) {
    let t = TEXT.lock().clone();
    (SEQ.load(Ordering::Acquire), t, ORIGIN.load(Ordering::Acquire))
}

pub fn set_latin1(b: &[u8], from: u64) {
    set_text(&b.iter().map(|&c| c as char).collect::<String>(), from);
}

pub fn set_text(t: &str, from: u64) {
    let t = t.replace("\r\n", "\n");
    let mut s: Vec<u16> = t.replace('\n', "\r\n").encode_utf16().collect();
    s.push(0);
    *FROM_CLIENT.lock() = Some((t, from));
    unsafe {
        if OpenClipboard(None).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, s.len() * 2) {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(s.as_ptr(), p, s.len());
                let _ = GlobalUnlock(h);
                let _ = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(h.0)));
            }
        }
        let _ = CloseClipboard();
    }
}

pub fn latin1(t: &str) -> Vec<u8> {
    t.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).collect()
}

fn ext(flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut m = vec![3u8, 0, 0, 0];
    m.extend_from_slice(&(-(4 + payload.len() as i32)).to_be_bytes());
    m.extend_from_slice(&flags.to_be_bytes());
    m.extend_from_slice(payload);
    m
}

pub fn ext_caps() -> Vec<u8> {
    ext(TEXT_FMT | CAPS | REQUEST | PEEK | NOTIFY | PROVIDE, &OUR_TEXT_MAX.to_be_bytes())
}

pub fn ext_action(action: u32, formats: u32) -> Vec<u8> {
    ext(action | formats, &[])
}

/// Provide: zlib stream of (u32 size, UTF-8 CRLF text + NUL).
pub fn ext_provide(t: &str) -> Vec<u8> {
    let mut data = t.replace('\n', "\r\n").into_bytes();
    data.push(0);
    let mut raw = (data.len() as u32).to_be_bytes().to_vec();
    raw.extend_from_slice(&data);
    let mut out = vec![0u8; raw.len() + raw.len() / 1000 + 64];
    let mut n = out.len() as z::uLongf;
    let r = unsafe { z::compress2(out.as_mut_ptr(), &mut n, raw.as_ptr(), raw.len() as z::uLong, 6) };
    if r != 0 {
        return ext_action(NOTIFY, 0);
    }
    out.truncate(n as usize);
    ext(PROVIDE | TEXT_FMT, &out)
}

/// Text from a client provide payload (after flags).
pub fn parse_provide(flags: u32, body: &[u8]) -> Option<String> {
    if flags & TEXT_FMT == 0 {
        return None;
    }
    let raw = inflate(body, MAX)?;
    let n = u32::from_be_bytes(raw.get(..4)?.try_into().ok()?) as usize;
    let mut t = raw.get(4..4 + n)?;
    while let [rest @ .., 0] = t {
        t = rest;
    }
    Some(String::from_utf8_lossy(t).into_owned())
}

/// Inflates a zlib stream that may end with a sync flush instead of a stream end.
fn inflate(input: &[u8], max: usize) -> Option<Vec<u8>> {
    unsafe {
        let mut s: Box<z::z_stream> = Box::from_raw(std::alloc::alloc_zeroed(std::alloc::Layout::new::<z::z_stream>()) as *mut z::z_stream);
        if z::inflateInit_(&mut *s, z::zlibVersion(), std::mem::size_of::<z::z_stream>() as i32) != 0 {
            return None;
        }
        let mut out = vec![0u8; (input.len() * 4).clamp(4096, max)];
        s.next_in = input.as_ptr();
        s.avail_in = input.len() as u32;
        let mut done = 0usize;
        let ok = loop {
            s.next_out = out.as_mut_ptr().add(done);
            s.avail_out = (out.len() - done) as u32;
            let r = z::inflate(&mut *s, 0);
            done = out.len() - s.avail_out as usize;
            match r {
                1 => break true,
                0 | -5 if s.avail_in == 0 && s.avail_out > 0 => break true,
                0 | -5 if out.len() < max => out.resize((out.len() * 2).min(max), 0),
                _ => break false,
            }
        };
        z::inflateEnd(&mut *s);
        ok.then(|| {
            out.truncate(done);
            out
        })
    }
}

fn read_clipboard() -> Option<String> {
    unsafe {
        OpenClipboard(None).ok()?;
        let r = (|| {
            let h = GetClipboardData(CF_UNICODETEXT).ok()?;
            let p = GlobalLock(HGLOBAL(h.0)) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut n = 0;
            while *p.add(n) != 0 && n < MAX / 2 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n)).replace("\r\n", "\n");
            let _ = GlobalUnlock(HGLOBAL(h.0));
            Some(s)
        })();
        let _ = CloseClipboard();
        r
    }
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_CLIPBOARDUPDATE {
        thread_local! {
            static BEAT: Arc<crate::watchdog::Beat> = crate::watchdog::call("clipboard read", std::time::Duration::from_secs(30), false);
        }
        let t = BEAT.with(|b| {
            b.enter();
            let t = read_clipboard();
            b.leave();
            t
        });
        if let Some(t) = t {
            let from = FROM_CLIENT.lock().take().filter(|c| c.0 == t).map_or(0, |c| c.1);
            *TEXT.lock() = Some(Arc::new(t));
            ORIGIN.store(from, Ordering::Release);
            SEQ.fetch_add(1, Ordering::AcqRel);
        }
        return LRESULT(0);
    }
    DefWindowProcW(h, m, w, l)
}

pub fn start() {
    std::thread::Builder::new().name("clipboard".into()).spawn(|| unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: w!("PraeterVNCClip"), ..Default::default() };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(Default::default(), w!("PraeterVNCClip"), w!(""), Default::default(), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(inst.into()), None) else { return };
        let _ = AddClipboardFormatListener(hwnd);
        *TEXT.lock() = read_clipboard().map(Arc::new);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provide_roundtrip() {
        let t = "héllo\nwörld ✓ 日本";
        let m = ext_provide(t);
        assert_eq!(m[0], 3);
        let len = i32::from_be_bytes(m[4..8].try_into().unwrap());
        assert_eq!(-len as usize, m.len() - 8);
        let flags = u32::from_be_bytes(m[8..12].try_into().unwrap());
        assert_eq!(flags, PROVIDE | TEXT_FMT);
        assert_eq!(parse_provide(flags, &m[12..]).unwrap(), t.replace('\n', "\r\n"));
    }
}
