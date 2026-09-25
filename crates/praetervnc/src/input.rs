//! Input injection via SendInput.
use std::collections::HashSet;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Called instead of injecting Ctrl+Alt+Del.
pub static SAS: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

pub struct Injector {
    buttons: u16,
    sas_del: bool,
    down_vk: HashSet<(u16, bool)>,
    down_scan: HashSet<(u16, bool)>,
    down_unicode: HashSet<u32>,
}

const WHEEL: i32 = 120;

thread_local! {
    static BEAT: std::sync::Arc<crate::watchdog::Beat> = crate::watchdog::call("input injection", std::time::Duration::from_secs(10), true);
}

fn send(inputs: &[INPUT]) {
    crate::desktop::follow_input_lazy();
    BEAT.with(|b| {
        b.enter();
        unsafe {
            SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
        }
        b.leave();
    });
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

impl Default for Injector {
    fn default() -> Self {
        Injector { buttons: 0, sas_del: false, down_vk: HashSet::new(), down_scan: HashSet::new(), down_unicode: HashSet::new() }
    }
}

impl Injector {
    /// `x`,`y` in virtual-desktop pixels relative to the framebuffer origin.
    pub fn pointer(&mut self, mask: u16, x: i32, y: i32, origin: (i32, i32)) {
        let (vx, vy, vw, vh) = unsafe {
            (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN), GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN))
        };
        let px = (x + origin.0 - vx).clamp(0, vw - 1) as i64;
        let py = (y + origin.1 - vy).clamp(0, vh - 1) as i64;
        let nx = ((px * 65536 + vw as i64 - 1) / vw as i64) as i32;
        let ny = ((py * 65536 + vh as i64 - 1) / vh as i64) as i32;
        let abs = MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
        let mut v = Vec::with_capacity(4);
        v.push(mouse(nx, ny, 0, abs | MOUSEEVENTF_MOVE));
        let changed = mask ^ self.buttons;
        let btn = [(0, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP), (1, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP), (2, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)];
        for (bit, d, u) in btn {
            if changed & (1 << bit) != 0 {
                v.push(mouse(nx, ny, 0, abs | if mask & (1 << bit) != 0 { d } else { u }));
            }
        }
        for (bit, xb) in [(7u16, XBUTTON1), (8, XBUTTON2)] {
            if changed & (1 << bit) != 0 {
                let f = if mask & (1 << bit) != 0 { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP };
                v.push(mouse(nx, ny, xb as i32, abs | f));
            }
        }
        // Wheel "buttons" fire on press.
        let pressed = changed & mask;
        if pressed & (1 << 3) != 0 {
            v.push(mouse(nx, ny, WHEEL, abs | MOUSEEVENTF_WHEEL));
        }
        if pressed & (1 << 4) != 0 {
            v.push(mouse(nx, ny, -WHEEL, abs | MOUSEEVENTF_WHEEL));
        }
        if pressed & (1 << 5) != 0 {
            v.push(mouse(nx, ny, -WHEEL, abs | MOUSEEVENTF_HWHEEL));
        }
        if pressed & (1 << 6) != 0 {
            v.push(mouse(nx, ny, WHEEL, abs | MOUSEEVENTF_HWHEEL));
        }
        self.buttons = mask;
        send(&v);
    }

    /// QEMU extended key event: XT scancode.
    pub fn scancode(&mut self, down: bool, keycode: u32, keysym: u32) {
        let (scan, ext) = if keycode > 0xFF {
            ((keycode & 0xFF) as u16, (keycode >> 8) == 0xE0)
        } else {
            ((keycode & 0x7F) as u16, keycode & 0x80 != 0)
        };
        if scan == 0 {
            return self.keysym(down, keysym);
        }
        if scan == 0x53 && self.sas(down) {
            return;
        }
        // Pause and PrintScreen need VK-based injection.
        if keysym == 0xff13 || keysym == 0xff61 {
            return self.keysym(down, keysym);
        }
        let mut f = KEYEVENTF_SCANCODE;
        if ext {
            f |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            f |= KEYEVENTF_KEYUP;
            self.down_scan.remove(&(scan, ext));
        } else {
            self.down_scan.insert((scan, ext));
        }
        send(&[key(0, scan, f)]);
    }

    pub fn keysym(&mut self, down: bool, ks: u32) {
        if (ks == 0xffff || ks == 0xff9f) && self.sas(down) {
            return;
        }
        if let Some((vk, ext)) = special_vk(ks) {
            return self.vk(down, vk, ext);
        }
        let ch = match keysym_to_char(ks) {
            Some(c) => c,
            None => return,
        };
        let r = unsafe { VkKeyScanW(ch as u16) };
        if r != -1 && ch < 0x10000 {
            let vk = (r & 0xFF) as u16;
            let need_shift = r & 0x100 != 0;
            let mods_extra = r & 0x600 != 0;
            let shift_down = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } as u16 & 0x8000 != 0;
            if !mods_extra && need_shift == shift_down || !down {
                return self.vk(down, vk, false);
            }
            if !mods_extra {
                // Shift state mismatch: toggle Shift around the key press.
                let sh = VK_LSHIFT.0;
                let mut v = Vec::new();
                if need_shift {
                    v.push(key(sh, 0, KEYBD_EVENT_FLAGS(0)));
                    v.push(key(vk, 0, KEYBD_EVENT_FLAGS(0)));
                    v.push(key(sh, 0, KEYEVENTF_KEYUP));
                } else {
                    let (l, rr) = unsafe { (GetAsyncKeyState(VK_LSHIFT.0 as i32) as u16 & 0x8000 != 0, GetAsyncKeyState(VK_RSHIFT.0 as i32) as u16 & 0x8000 != 0) };
                    if l { v.push(key(VK_LSHIFT.0, 0, KEYEVENTF_KEYUP)); }
                    if rr { v.push(key(VK_RSHIFT.0, 0, KEYEVENTF_KEYUP)); }
                    v.push(key(vk, 0, KEYBD_EVENT_FLAGS(0)));
                    if l { v.push(key(VK_LSHIFT.0, 0, KEYBD_EVENT_FLAGS(0))); }
                    if rr { v.push(key(VK_RSHIFT.0, 0, KEYBD_EVENT_FLAGS(0))); }
                }
                self.down_vk.insert((vk, false));
                return send(&v);
            }
        }
        // Fallback: Unicode injection.
        if ch >= 0x10000 {
            if down {
                let mut b = [0u16; 2];
                let s = char::from_u32(ch).map(|c| c.encode_utf16(&mut b).len()).unwrap_or(0);
                let v: Vec<INPUT> = b[..s].iter().flat_map(|&u| [key(0, u, KEYEVENTF_UNICODE), key(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)]).collect();
                send(&v);
            }
            return;
        }
        let f = if down { KEYEVENTF_UNICODE } else { KEYEVENTF_UNICODE | KEYEVENTF_KEYUP };
        if down {
            self.down_unicode.insert(ch);
        } else if !self.down_unicode.remove(&ch) {
            return;
        }
        send(&[key(0, ch as u16, f)]);
    }

    /// Delete with Ctrl+Alt held goes to the SAS hook; true if handled.
    fn sas(&mut self, down: bool) -> bool {
        let Some(f) = SAS.get() else { return false };
        if !down {
            return std::mem::take(&mut self.sas_del);
        }
        let vk = |a: VIRTUAL_KEY, b: VIRTUAL_KEY| self.down_vk.iter().any(|&(v, _)| v == a.0 || v == b.0);
        let sc = |c: u16| self.down_scan.iter().any(|&(s, _)| s == c);
        if (vk(VK_LCONTROL, VK_RCONTROL) || sc(0x1D)) && (vk(VK_LMENU, VK_RMENU) || sc(0x38)) {
            self.sas_del = true;
            f();
            return true;
        }
        false
    }

    fn vk(&mut self, down: bool, vk: u16, ext: bool) {
        let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
        let mut f = KEYBD_EVENT_FLAGS(0);
        if ext {
            f |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            f |= KEYEVENTF_KEYUP;
            self.down_vk.remove(&(vk, ext));
        } else {
            self.down_vk.insert((vk, ext));
        }
        send(&[key(vk, scan, f)]);
    }

    pub fn release_all(&mut self) {
        let mut v = Vec::new();
        for &(vk, ext) in &self.down_vk {
            let mut f = KEYEVENTF_KEYUP;
            if ext { f |= KEYEVENTF_EXTENDEDKEY; }
            v.push(key(vk, 0, f));
        }
        for &(sc, ext) in &self.down_scan {
            let mut f = KEYEVENTF_KEYUP | KEYEVENTF_SCANCODE;
            if ext { f |= KEYEVENTF_EXTENDEDKEY; }
            v.push(key(0, sc, f));
        }
        for &u in &self.down_unicode {
            v.push(key(0, u as u16, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        }
        self.down_vk.clear();
        self.down_scan.clear();
        self.down_unicode.clear();
        if self.buttons & 7 != 0 {
            let fl = [(1, MOUSEEVENTF_LEFTUP), (2, MOUSEEVENTF_MIDDLEUP), (4, MOUSEEVENTF_RIGHTUP)];
            for (b, f) in fl {
                if self.buttons & b != 0 {
                    v.push(mouse(0, 0, 0, f));
                }
            }
            self.buttons = 0;
        }
        if !v.is_empty() {
            send(&v);
        }
    }
}

/// Key-up for held modifiers and mouse buttons.
pub fn release_stuck() -> usize {
    crate::desktop::follow_input();
    let down = |v: VIRTUAL_KEY| unsafe { GetAsyncKeyState(v.0 as i32) } as u16 & 0x8000 != 0;
    let mut v = Vec::new();
    for (k, ext) in [(VK_LSHIFT, false), (VK_RSHIFT, false), (VK_LCONTROL, false), (VK_RCONTROL, true), (VK_LMENU, false), (VK_RMENU, true), (VK_LWIN, true), (VK_RWIN, true)] {
        if down(k) {
            v.push(key(k.0, 0, KEYEVENTF_KEYUP | if ext { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) }));
        }
    }
    for (b, f) in [(VK_LBUTTON, MOUSEEVENTF_LEFTUP), (VK_RBUTTON, MOUSEEVENTF_RIGHTUP), (VK_MBUTTON, MOUSEEVENTF_MIDDLEUP)] {
        if down(b) {
            v.push(mouse(0, 0, 0, f));
        }
    }
    if !v.is_empty() {
        send(&v);
    }
    v.len()
}

fn special_vk(ks: u32) -> Option<(u16, bool)> {
    let r = match ks {
        0xff08 => (VK_BACK, false),
        0xff09 | 0xfe20 => (VK_TAB, false),
        0xff0d => (VK_RETURN, false),
        0xff1b => (VK_ESCAPE, false),
        0xff13 => (VK_PAUSE, false),
        0xff14 => (VK_SCROLL, false),
        0xff61 => (VK_SNAPSHOT, true),
        0xff63 => (VK_INSERT, true),
        0xffff => (VK_DELETE, true),
        0xff50 => (VK_HOME, true),
        0xff51 => (VK_LEFT, true),
        0xff52 => (VK_UP, true),
        0xff53 => (VK_RIGHT, true),
        0xff54 => (VK_DOWN, true),
        0xff55 => (VK_PRIOR, true),
        0xff56 => (VK_NEXT, true),
        0xff57 => (VK_END, true),
        0xff67 => (VK_APPS, true),
        0xff7f => (VK_NUMLOCK, true),
        0xffe5 => (VK_CAPITAL, false),
        0xffe1 => (VK_LSHIFT, false),
        0xffe2 => (VK_RSHIFT, false),
        0xffe3 => (VK_LCONTROL, false),
        0xffe4 => (VK_RCONTROL, true),
        0xffe7 | 0xffeb => (VK_LWIN, true),
        0xffe8 | 0xffec => (VK_RWIN, true),
        0xffe9 => (VK_LMENU, false),
        0xffea | 0xfe03 => (VK_RMENU, true),
        0xff8d => (VK_RETURN, true),
        0xffaa => (VK_MULTIPLY, false),
        0xffab => (VK_ADD, false),
        0xffac => (VK_SEPARATOR, false),
        0xffad => (VK_SUBTRACT, false),
        0xffae => (VK_DECIMAL, false),
        0xffaf => (VK_DIVIDE, true),
        0xff95 => (VK_HOME, false),
        0xff96 => (VK_LEFT, false),
        0xff97 => (VK_UP, false),
        0xff98 => (VK_RIGHT, false),
        0xff99 => (VK_DOWN, false),
        0xff9a => (VK_PRIOR, false),
        0xff9b => (VK_NEXT, false),
        0xff9c => (VK_END, false),
        0xff9d => (VK_CLEAR, false),
        0xff9e => (VK_INSERT, false),
        0xff9f => (VK_DELETE, false),
        0xffb0..=0xffb9 => return Some((VK_NUMPAD0.0 + (ks - 0xffb0) as u16, false)),
        0xffbe..=0xffd5 => return Some((VK_F1.0 + (ks - 0xffbe) as u16, false)),
        0x1008ff11 => (VK_VOLUME_DOWN, true),
        0x1008ff12 => (VK_VOLUME_MUTE, true),
        0x1008ff13 => (VK_VOLUME_UP, true),
        0x1008ff14 => (VK_MEDIA_PLAY_PAUSE, true),
        0x1008ff15 => (VK_MEDIA_STOP, true),
        0x1008ff16 => (VK_MEDIA_PREV_TRACK, true),
        0x1008ff17 => (VK_MEDIA_NEXT_TRACK, true),
        _ => return None,
    };
    Some((r.0 .0, r.1))
}

fn keysym_to_char(ks: u32) -> Option<u32> {
    match ks {
        0x20..=0x7e | 0xa0..=0xff => Some(ks),
        0x0100_0100..=0x0110_ffff => Some(ks - 0x0100_0000),
        0x20ac => Some(0x20ac),
        _ => None,
    }
}
