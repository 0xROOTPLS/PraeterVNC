//! Optional pipeline probe (PRAETER_PROBE=1): follows the test app's barcode through
//! present -> acquire -> publish -> send and prints per-stage latency percentiles.
use crate::fb::Snapshot;
use parking_lot::Mutex;
use std::sync::OnceLock;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Performance::*;

#[repr(C)]
struct Shm {
    magic: u64,
    bar_x: i32,
    bar_y: i32,
    frames: u64,
    ts: [u64; 65536],
}

#[derive(Clone, Copy, Default)]
struct Rec {
    app: u64,
    present: u64,
    acquire: u64,
    publish: u64,
    sent: u64,
}

struct Probe {
    shm: &'static Shm,
    freq: f64,
    recs: Mutex<std::collections::HashMap<u32, Rec>>,
    done: Mutex<Vec<Rec>>,
    last_cap: Mutex<u32>,
}

static P: OnceLock<Probe> = OnceLock::new();
static ENABLED: OnceLock<bool> = OnceLock::new();

fn get() -> Option<&'static Probe> {
    if !*ENABLED.get_or_init(|| std::env::var("PRAETER_PROBE").is_ok()) {
        return None;
    }
    if let Some(p) = P.get() {
        return Some(p);
    }
    unsafe {
        let m = OpenFileMappingW(FILE_MAP_READ.0, false, windows::core::w!(r"Local\PraeterTestApp")).ok()?;
        let v = MapViewOfFile(m, FILE_MAP_READ, 0, 0, 0);
        let mut f = 0i64;
        let _ = QueryPerformanceFrequency(&mut f);
        let _ = P.set(Probe { shm: &*(v.Value as *const Shm), freq: f as f64, recs: Default::default(), done: Default::default(), last_cap: Mutex::new(0) });
    }
    P.get()
}

pub fn now() -> u64 {
    let mut v = 0i64;
    unsafe { let _ = QueryPerformanceCounter(&mut v); }
    v as u64
}

fn read_id(p: &Probe, snap: &Snapshot) -> Option<u32> {
    if p.shm.magic != 0x5052_4145_5445_5231 {
        return None;
    }
    let mut v = 0u32;
    for b in 0..32 {
        let x = p.shm.bar_x + b * 16 + 8;
        let y = p.shm.bar_y + 8;
        if x < 0 || y < 0 || x as u32 >= snap.w || y as u32 >= snap.h {
            return None;
        }
        if (snap.pixel(x as u32, y as u32) >> 8) & 255 > 128 {
            v |= 1 << b;
        }
    }
    let id = v & 0xFF_FFFF;
    ((id.wrapping_mul(0x9E37_79B1) >> 24) == v >> 24).then_some(id)
}

pub fn trace() -> bool {
    static T: OnceLock<bool> = OnceLock::new();
    *T.get_or_init(|| std::env::var("PRAETER_TRACE").is_ok())
}

pub fn frame_id(snap: &Snapshot) -> u32 {
    get().and_then(|p| read_id(p, snap)).unwrap_or(0)
}

pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var("PRAETER_PROBE").is_ok())
}

/// Called after a capture publish.
pub fn captured(snap: &Snapshot, present: i64, acquire: u64) {
    let Some(p) = get() else { return };
    let Some(id) = read_id(p, snap) else { return };
    let mut lc = p.last_cap.lock();
    if *lc == id {
        return;
    }
    *lc = id;
    let app = p.shm.ts[(id & 0xFFFF) as usize];
    p.recs.lock().insert(id, Rec { app, present: present as u64, acquire, publish: now(), sent: 0 });
}

/// Called after an update containing `snap` was written.
pub fn sent(snap: &Snapshot) {
    let Some(p) = get() else { return };
    let Some(id) = read_id(p, snap) else { return };
    let t = now();
    let mut recs = p.recs.lock();
    if let Some(mut r) = recs.remove(&id) {
        r.sent = t;
        let mut d = p.done.lock();
        d.push(r);
        if d.len() >= 400 {
            report(p, &d);
            d.clear();
        }
    }
    recs.retain(|&k, _| k + 256 > id);
}

fn report(p: &Probe, d: &[Rec]) {
    let ms = |a: u64, b: u64| (b as f64 - a as f64) / p.freq * 1e3;
    let stat = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |x: f64| v[((v.len() - 1) as f64 * x) as usize];
        format!("p50 {:6.2} p90 {:6.2} p99 {:6.2} max {:6.2}", q(0.5), q(0.9), q(0.99), q(1.0))
    };
    crate::log!("probe n={}", d.len());
    crate::log!("  app->present   {}", stat(d.iter().map(|r| ms(r.app, r.present)).collect()));
    crate::log!("  present->acq   {}", stat(d.iter().map(|r| ms(r.present, r.acquire)).collect()));
    crate::log!("  acq->publish   {}", stat(d.iter().map(|r| ms(r.acquire, r.publish)).collect()));
    crate::log!("  publish->sent  {}", stat(d.iter().map(|r| ms(r.publish, r.sent)).collect()));
    crate::log!("  app->sent      {}", stat(d.iter().map(|r| ms(r.app, r.sent)).collect()));
}
