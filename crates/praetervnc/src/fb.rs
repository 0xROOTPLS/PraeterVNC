//! Immutable tiled framebuffer snapshots. A changed tile gets a new Arc; dirty = pointer inequality.
use parking_lot::{Condvar, Mutex};
use std::sync::{Arc, OnceLock};

pub const TS: usize = 64;
pub const TPX: usize = TS * TS;

pub struct Tile {
    pub px: Box<[u32; TPX]>,
    rh: OnceLock<RowHashes>,
}

/// Per-row hashes; bit y of `uniform` set if row y is a single colour.
pub struct RowHashes {
    pub h: [u64; TS],
    pub uniform: u64,
}

impl Tile {
    pub fn blank() -> Arc<Tile> {
        Arc::new(Tile { px: vec![0u32; TPX].into_boxed_slice().try_into().unwrap(), rh: OnceLock::new() })
    }
    pub fn clone_px(&self) -> Tile {
        Tile { px: self.px.clone(), rh: OnceLock::new() }
    }
    pub fn hashes(&self) -> &RowHashes {
        self.rh.get_or_init(|| {
            let mut r = RowHashes { h: [0; TS], uniform: 0 };
            for y in 0..TS {
                let row = self.row(y);
                r.h[y] = crate::simd::hash_row(row);
                if crate::simd::all_eq(row, row[0]) {
                    r.uniform |= 1 << y;
                }
            }
            r
        })
    }
    #[inline]
    pub fn row(&self, y: usize) -> &[u32] {
        &self.px[y * TS..y * TS + TS]
    }
}

pub type TileRef = Arc<Tile>;

#[derive(Clone, Default)]
pub struct CursorShape {
    pub w: u16,
    pub h: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    /// Straight (non-premultiplied) ARGB, 0xAARRGGBB.
    pub argb: Vec<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect { x: x as u16, y: y as u16, w: w as u16, h: h as u16 }
    }
    pub fn area(&self) -> u32 {
        self.w as u32 * self.h as u32
    }
    pub fn right(&self) -> u32 {
        self.x as u32 + self.w as u32
    }
    pub fn bottom(&self) -> u32 {
        self.y as u32 + self.h as u32
    }
    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let x0 = self.x.max(o.x) as u32;
        let y0 = self.y.max(o.y) as u32;
        let x1 = self.right().min(o.right());
        let y1 = self.bottom().min(o.bottom());
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
    }
}

pub struct Snapshot {
    pub seq: u64,
    pub w: u32,
    pub h: u32,
    pub tw: u32,
    pub th: u32,
    pub tiles: Vec<TileRef>,
    pub cursor: Option<Arc<CursorShape>>,
    pub cursor_seq: u64,
    pub cursor_pos: (i32, i32),
    pub cursor_visible: bool,
    /// Bumped on geometry change.
    pub layout_seq: u64,
}

impl Snapshot {
    pub fn blank(w: u32, h: u32, layout_seq: u64) -> Snapshot {
        let tw = w.div_ceil(TS as u32);
        let th = h.div_ceil(TS as u32);
        let b = Tile::blank();
        Snapshot {
            seq: 0, w, h, tw, th,
            tiles: vec![b; (tw * th) as usize],
            cursor: None, cursor_seq: 0, cursor_pos: (0, 0), cursor_visible: true, layout_seq,
        }
    }

    /// Builds a snapshot from a packed 0x00RRGGBB image.
    pub fn from_pixels(w: u32, h: u32, px: &[u32]) -> Snapshot {
        let mut s = Snapshot::blank(w, h, 1);
        for ty in 0..s.th {
            for tx in 0..s.tw {
                let mut t = Tile::blank().clone_px();
                for y in 0..TS as u32 {
                    let gy = ty * TS as u32 + y;
                    if gy >= h {
                        break;
                    }
                    for x in 0..TS as u32 {
                        let gx = tx * TS as u32 + x;
                        if gx < w {
                            t.px[(y as usize) * TS + x as usize] = px[(gy * w + gx) as usize] & 0xFF_FFFF;
                        }
                    }
                }
                s.tiles[(ty * s.tw + tx) as usize] = Arc::new(t);
            }
        }
        s
    }

    #[inline]
    pub fn tile(&self, tx: u32, ty: u32) -> &TileRef {
        &self.tiles[(ty * self.tw + tx) as usize]
    }

    /// Copies a rect of pixels into a contiguous buffer (row-major, width r.w).
    pub fn gather(&self, r: Rect, out: &mut Vec<u32>) {
        out.clear();
        out.reserve(r.area() as usize);
        let x0 = r.x as usize;
        let x1 = r.right() as usize;
        for y in r.y as usize..r.bottom() as usize {
            let ty = y / TS;
            let ry = y % TS;
            let mut x = x0;
            while x < x1 {
                let tx = x / TS;
                let lx = x % TS;
                let n = (TS - lx).min(x1 - x);
                let t = &self.tiles[ty * self.tw as usize + tx];
                out.extend_from_slice(&t.px[ry * TS + lx..ry * TS + lx + n]);
                x += n;
            }
        }
    }

    #[inline]
    pub fn pixel(&self, x: u32, y: u32) -> u32 {
        let t = &self.tiles[(y as usize / TS) * self.tw as usize + x as usize / TS];
        t.px[(y as usize % TS) * TS + x as usize % TS]
    }
}

/// Latest-snapshot store with change notification.
pub struct FrameStore {
    latest: Mutex<Arc<Snapshot>>,
    subs: Mutex<Vec<Arc<Signal>>>,
}

impl FrameStore {
    pub fn new(s: Snapshot) -> FrameStore {
        FrameStore { latest: Mutex::new(Arc::new(s)), subs: Mutex::new(Vec::new()) }
    }
    pub fn latest(&self) -> Arc<Snapshot> {
        self.latest.lock().clone()
    }
    pub fn publish(&self, s: Snapshot) {
        *self.latest.lock() = Arc::new(s);
        let subs = self.subs.lock();
        for s in subs.iter() {
            s.notify();
        }
    }
    pub fn subscribe(&self, s: Arc<Signal>) {
        self.subs.lock().push(s);
    }
    pub fn unsubscribe(&self, s: &Arc<Signal>) {
        self.subs.lock().retain(|x| !Arc::ptr_eq(x, s));
    }
}

/// Auto-reset event.
#[derive(Default)]
pub struct Signal {
    m: Mutex<bool>,
    c: Condvar,
}

impl Signal {
    pub fn notify(&self) {
        let mut g = self.m.lock();
        *g = true;
        self.c.notify_one();
    }
    /// Returns true if signalled.
    pub fn wait_timeout(&self, d: std::time::Duration) -> bool {
        let mut g = self.m.lock();
        if !*g {
            self.c.wait_for(&mut g, d);
        }
        std::mem::replace(&mut *g, false)
    }
}
