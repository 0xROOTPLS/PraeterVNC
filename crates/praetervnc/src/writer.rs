//! Update scheduler: per-tile client model, scroll copies, budgeted priority selection,
//! streamed parallel encoding, and flow control (request credits or CU + fences).
use crate::capture::Capture;
use crate::enc::{self, Ctx, Emitted, Encoder, Kind, Mode, Rect};
use crate::fb::{Snapshot, Tile, TileRef, TS};
use crate::pixfmt::{Converter, PixelFormat};
use crate::plan::{self, Run};
use crate::rfb::{self, enc as E};
use crate::session::{Caps, Shared};
use crate::Config;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Stats {
    updates: u64,
    bytes: u64,
    rects: u64,
    copies: u64,
    alr_px: u64,
    px: u64,
    deferred: u64,
    enc_us: u64,
    reqs: u64,
    unsolicited: u64,
    since: Option<Instant>,
}

pub struct Writer<'a> {
    cfg: &'a Config,
    cap: &'a Capture,
    sh: &'a Shared,
    peer: &'a str,
    sock: TcpStream,
    caps: Caps,
    enc: Encoder,
    /// What the client holds, per tile; `known[ti]` false means undefined.
    model: Option<Snapshot>,
    known: Vec<bool>,
    lossy: Vec<Option<Instant>>,
    since: Vec<Option<Instant>>,
    /// Consecutive-change streak per tile and when it last changed.
    streak: Vec<u8>,
    last_change: Vec<Option<Instant>>,
    cursor_seq: u64,
    cursor_vis: bool,
    layout_seq: u64,
    size: (u32, u32),
    need_ext_size: bool,
    fence_announced: bool,
    cu_announced: bool,
    cu: Option<Rect>,
    inflight: VecDeque<(u32, Instant)>,
    fence_seq: u32,
    rtt_ms: f64,
    /// Encode throughput estimate, pixels per ms.
    rate: f64,
    deferred: bool,
    /// Unsolicited updates not yet matched by a client request (legacy pipelining).
    owed: u32,
    req_seen: u64,
    backlog_wait: bool,
    /// After SetPixelFormat, wait for a real request before pipelining again.
    pf_barrier: bool,
    written: u64,
    pipe: crate::pipe::Pipe,
    pace: Option<Duration>,
    enc_ms: f64,
    /// Interval between our updates, ms (EMA), and when the last one went.
    gap_ms: f64,
    last_upd: Option<Instant>,
    /// Encoded bytes per pixel (EMA): lossless refresh, motion, normal.
    alr_bpp: f64,
    motion_bpp: f64,
    norm_bpp: f64,
    win: crate::window::Window,
    tcp: crate::net::TcpInfo,
    tcp_at: Option<Instant>,
    clip_seq: u64,
    last_ptr: (i32, i32),
    ptr_at: Option<Instant>,
    /// Pointer-composited tiles: tile index -> (source tile, cursor seq, pos, result).
    comp: Vec<Composited>,
    stats: Stats,
}

/// Tile index, source tile, cursor seq, cursor pos, composited tile.
type Composited = (usize, usize, u64, (i32, i32), TileRef);

fn write_vec(s: &mut TcpStream, bufs: &[Vec<u8>]) -> io::Result<usize> {
    let mut slices: Vec<io::IoSlice> = bufs.iter().filter(|b| !b.is_empty()).map(|b| io::IoSlice::new(b)).collect();
    let total: usize = bufs.iter().map(|b| b.len()).sum();
    let mut sl = &mut slices[..];
    while !sl.is_empty() {
        let n = s.write_vectored(sl)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        io::IoSlice::advance_slices(&mut sl, n);
    }
    Ok(total)
}

impl<'a> Writer<'a> {
    pub fn new(cfg: &'a Config, cap: &'a Capture, sh: &'a Shared, peer: &'a str, sock: TcpStream) -> Writer<'a> {
        let snap = cap.store.latest();
        Writer {
            cfg, cap, sh, peer, sock,
            caps: Caps::default(),
            enc: Encoder::new(Kind::Raw, Ctx { conv: Converter::new(PixelFormat::NATIVE), jpeg: None, jpeg_min_area: 64 * 64, force_lossless: false, level: 1, jpeg21: None }),
            model: None,
            known: Vec::new(),
            lossy: Vec::new(),
            since: Vec::new(),
            streak: Vec::new(),
            last_change: Vec::new(),
            cursor_seq: u64::MAX,
            cursor_vis: true,
            layout_seq: snap.layout_seq,
            size: (snap.w, snap.h),
            need_ext_size: false,
            fence_announced: false,
            cu_announced: false,
            cu: None,
            inflight: VecDeque::new(),
            fence_seq: 0,
            rtt_ms: 0.0,
            rate: 1.5e6,
            deferred: false,
            owed: 0,
            req_seen: 0,
            backlog_wait: false,
            pf_barrier: false,
            written: 0,
            pipe: crate::pipe::Pipe::new(),
            pace: None,
            enc_ms: 1.0,
            gap_ms: 16.0,
            last_upd: None,
            alr_bpp: 1.5,
            motion_bpp: 0.15,
            norm_bpp: 0.5,
            win: crate::window::Window::new(cfg.max_inflight.max(1)),
            tcp: Default::default(),
            tcp_at: None,
            clip_seq: crate::clipboard::current().0,
            last_ptr: (-1, -1),
            ptr_at: None,
            comp: Vec::new(),
            stats: Stats { since: Some(Instant::now()), ..Default::default() },
        }
    }

    pub fn run(&mut self) -> io::Result<()> {
        let beat = crate::watchdog::looped(format!("writer {}", self.peer), Duration::from_secs(60));
        loop {
            beat.tick();
            let busy = self.deferred || self.lossy.iter().any(|l| l.is_some());
            let mut wait = if self.backlog_wait { Duration::from_millis(2) } else if busy { Duration::from_millis(8) } else { Duration::from_millis(250) };
            if let Some(p) = self.pace {
                wait = wait.min(p);
            }
            self.sh.sig.wait_timeout(wait);
            let snap = self.cap.store.latest();
            let mut msgs: Vec<Vec<u8>> = Vec::new();
            let (can_send, full, ptr) = {
                let mut st = self.sh.st.lock();
                if st.closed {
                    return Ok(());
                }
                if let Some(pf) = st.pf.take() {
                    self.set_pixel_format(pf, &mut msgs);
                }
                if let Some(list) = st.encodings.take() {
                    self.set_encodings(&list, &mut msgs);
                }
                for (flags, p) in st.fences.drain(..) {
                    let mut m = vec![248u8, 0, 0, 0];
                    m.extend_from_slice(&(flags & 0x3).to_be_bytes());
                    m.push(p.len() as u8);
                    m.extend_from_slice(&p);
                    msgs.push(m);
                }
                for p in st.fence_acks.drain(..) {
                    if p.len() == 4 {
                        let seq = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
                        while let Some(&(s, t)) = self.inflight.front() {
                            if s.wrapping_sub(seq) as i32 > 0 {
                                break;
                            }
                            self.inflight.pop_front();
                            let r = self.pipe.on_ack(Instant::now());
                            if crate::probe::trace() {
                                crate::log!("fence ack rtt {r:?} rate {:.0} prop {:.1} tx {:.1} inflight {}", self.pipe.rate, self.pipe.prop, self.pipe.tx, self.inflight.len());
                            }
                            if s == seq {
                                let r = t.elapsed().as_secs_f64() * 1e3;
                                self.rtt_ms = if self.rtt_ms == 0.0 { r } else { self.rtt_ms * 0.875 + r * 0.125 };
                                self.win.sample(r);
                            }
                        }
                    }
                }
                if let Some(c) = st.cu_change.take() {
                    match c {
                        // Fences are our only ack under CU.
                        Some(r) if self.caps.fence => {
                            self.cu = Some(r);
                            self.inflight.clear();
                            self.pipe.clear();
                        }
                        Some(_) => crate::log!("{}: continuous updates without fence support ignored", self.peer),
                        None => {
                            self.cu = None;
                            self.inflight.clear();
                            self.pipe.clear();
                            msgs.push(vec![150]);
                        }
                    }
                }
                msgs.append(&mut st.clip_out);
                let (cseq, ctext, origin) = crate::clipboard::current();
                if cseq != self.clip_seq {
                    self.clip_seq = cseq;
                    if let Some(c) = ctext.filter(|_| origin != self.sh.id) {
                        use crate::clipboard as cb;
                        let (flags, max) = st.clip_caps;
                        if !self.caps.ext_clip {
                            let t = cb::latin1(&c);
                            let mut m = vec![3u8, 0, 0, 0];
                            m.extend_from_slice(&(t.len() as u32).to_be_bytes());
                            m.extend_from_slice(&t);
                            msgs.push(m);
                        } else if flags & cb::PROVIDE != 0 && c.len() < max as usize {
                            msgs.push(cb::ext_provide(&c));
                        } else if flags & cb::NOTIFY != 0 {
                            msgs.push(cb::ext_action(cb::NOTIFY, cb::TEXT_FMT));
                        }
                    }
                }
                let cu = self.cu.is_some();
                if cu {
                    st.requests = 0;
                    self.owed = 0;
                } else {
                    let new = st.req_total - self.req_seen;
                    self.req_seen = st.req_total;
                    self.stats.reqs += new;
                    // Each new request acknowledges our oldest outstanding update.
                    for _ in 0..new {
                        let Some(r) = self.pipe.on_ack(Instant::now()) else { break };
                        self.win.sample(r);
                        if crate::probe::trace() {
                            crate::log!("ack rtt {r:.1} rate {:.0} KB/s prop {:.1} tx {:.1}", self.pipe.rate, self.pipe.prop, self.pipe.tx);
                        }
                    }
                    // Requests first acknowledge updates we sent ahead of them.
                    let m = st.requests.min(self.owed);
                    st.requests -= m;
                    self.owed -= m;
                    if st.requests > 0 {
                        self.pf_barrier = false;
                    }
                }
                let depth = self.pipeline_depth();
                let cap = if self.pipe.rate > 0.0 { self.cfg.max_inflight } else { self.win.depth };
                let can = if cu {
                    (self.inflight.len() as u32) < cap.clamp(1, self.cfg.max_inflight.max(1))
                } else {
                    st.requests > 0 || (!self.pf_barrier && self.owed < depth)
                };
                self.pace = if can { self.pipe.wait(self.enc_ms, Instant::now()) } else { None };
                if crate::probe::trace() && self.pace.is_some() {
                    crate::log!("pace {:?}", self.pace);
                }
                let can = can && self.pace.is_none();
                let backlog = can && self.link_backlogged();
                self.backlog_wait = backlog;
                let can = can && !backlog;
                let full = if can { std::mem::take(&mut st.full) } else { Vec::new() };
                let ptr = st.ptr_time.filter(|t| t.elapsed() < Duration::from_secs(1)).map(|_| st.last_ptr);
                (can, full, ptr)
            };
            if !msgs.is_empty() {
                self.written += write_vec(&mut self.sock, &msgs)? as u64;
            }
            let snap = self.composite(snap);
            if can_send && self.update(&snap, &full, ptr)? {
                if self.cu.is_none() {
                    let mut st = self.sh.st.lock();
                    if st.requests > 0 {
                        st.requests -= 1;
                    } else {
                        self.owed += 1;
                        self.stats.unsolicited += 1;
                    }
                }
            }
            self.log_stats();
        }
    }

    fn set_pixel_format(&mut self, pf: PixelFormat, msgs: &mut Vec<Vec<u8>>) {
        crate::log!("{}: pixel format {}bpp depth {} max {}/{}/{}{}", self.peer, pf.bpp, pf.depth, pf.rmax, pf.gmax, pf.bmax, if pf.true_color { "" } else { " colour-map" });
        self.pf_barrier = true;
        self.owed = 0;
        self.pipe.clear();
        let mut p = pf;
        if !pf.true_color {
            // Colour-map clients get a fixed BGR233 map.
            let mut m = vec![1u8, 0, 0, 0, 1, 0];
            for i in 0..256u32 {
                let (r, g, b) = ((i & 7) * 65535 / 7, ((i >> 3) & 7) * 65535 / 7, (i >> 6) * 65535 / 3);
                m.extend_from_slice(&(r as u16).to_be_bytes());
                m.extend_from_slice(&(g as u16).to_be_bytes());
                m.extend_from_slice(&(b as u16).to_be_bytes());
            }
            msgs.push(m);
            p = PixelFormat::BGR233;
        }
        self.enc.cx.conv = Converter::new(p);
        self.model = None;
    }

    fn refresh_tcp(&mut self) {
        if self.tcp_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(2)) {
            if let Some(i) = crate::net::tcp_info(&self.sock) {
                self.tcp = i;
            }
            self.tcp_at = Some(Instant::now());
        }
    }

    /// Updates allowed ahead of client requests (delay-based window), 0 on low-RTT links.
    fn pipeline_depth(&mut self) -> u32 {
        if !self.cfg.pipeline {
            return 0;
        }
        self.refresh_tcp();
        let base = self.win.base();
        let rtt_ms = if base == f64::MAX { self.tcp.rtt_us as f64 / 1e3 } else { base.max(self.tcp.min_rtt_us as f64 / 1e3) };
        if rtt_ms < 3.0 {
            return 0;
        }
        // One request credit is always implicit.
        if self.pipe.rate > 0.0 { self.cfg.max_inflight - 1 } else { self.win.depth - 1 }
    }

    /// True while our earlier output still sits unsent in the local socket buffer.
    fn link_backlogged(&mut self) -> bool {
        self.refresh_tcp();
        let sent = self.tcp.bytes_out.saturating_sub(self.tcp.bytes_retrans);
        if sent == 0 {
            return false;
        }
        self.written.saturating_sub(sent) > 64 * 1024
    }

    /// Clients without cursor encodings get the pointer drawn into the pixels; composited
    /// tiles are cached per pointer position.
    fn composite(&mut self, snap: Arc<Snapshot>) -> Arc<Snapshot> {
        let c = match &snap.cursor {
            Some(c) if snap.cursor_visible && !self.caps.rich_cursor && !self.caps.alpha_cursor => c.clone(),
            _ => {
                self.comp.clear();
                return snap;
            }
        };
        let (px, py) = snap.cursor_pos;
        let (x0, y0) = (px.max(0) as u32, py.max(0) as u32);
        let (x1, y1) = ((px + c.w as i32).clamp(0, snap.w as i32) as u32, (py + c.h as i32).clamp(0, snap.h as i32) as u32);
        if x1 <= x0 || y1 <= y0 {
            self.comp.clear();
            return snap;
        }
        let ts = TS as u32;
        let mut tiles = snap.tiles.clone();
        let mut comp = Vec::new();
        for ty in y0 / ts..=(y1 - 1) / ts {
            for tx in x0 / ts..=(x1 - 1) / ts {
                let ti = (ty * snap.tw + tx) as usize;
                let src = Arc::as_ptr(&snap.tiles[ti]) as usize;
                let hit = self.comp.iter().find(|e| e.0 == ti && e.1 == src && e.2 == snap.cursor_seq && e.3 == (px, py));
                let t = match hit {
                    Some(e) => e.4.clone(),
                    None => {
                        let mut t = snap.tiles[ti].clone_px();
                        for y in (ty * ts).max(y0)..((ty + 1) * ts).min(y1) {
                            for x in (tx * ts).max(x0)..((tx + 1) * ts).min(x1) {
                                let s = c.argb[(y as i32 - py) as usize * c.w as usize + (x as i32 - px) as usize];
                                let a = s >> 24;
                                if a == 0 {
                                    continue;
                                }
                                let d = &mut t.px[((y - ty * ts) * ts + (x - tx * ts)) as usize];
                                let mix = |sh: u32| ((((s >> sh) & 255) * a + ((*d >> sh) & 255) * (255 - a) + 127) / 255) << sh;
                                *d = mix(16) | mix(8) | mix(0);
                            }
                        }
                        Arc::new(t)
                    }
                };
                tiles[ti] = t.clone();
                comp.push((ti, src, snap.cursor_seq, (px, py), t));
            }
        }
        self.comp = comp;
        Arc::new(Snapshot { seq: snap.seq, w: snap.w, h: snap.h, tw: snap.tw, th: snap.th, tiles, cursor: snap.cursor.clone(), cursor_seq: snap.cursor_seq,
            cursor_pos: snap.cursor_pos, cursor_visible: snap.cursor_visible, layout_seq: snap.layout_seq })
    }

    /// Keeps motion updates within ~14-28 ms of link time by trading JPEG quality.
    fn adapt_motion(&mut self, bytes: usize, motion: bool) {
        let cur = self.enc.cx.jpeg.or(self.enc.cx.jpeg21).map_or(100, |(q, _)| enc::motion_q(q, self.enc.motion_cap));
        let t = if self.pipe.rate > 0.0 { bytes as f64 / self.pipe.rate } else { 0.0 };
        self.enc.motion_cap = if motion && t > 28.0 {
            cur.saturating_sub(7).max(15)
        } else if !motion || t < 14.0 {
            cur.saturating_add(4).min(100)
        } else {
            cur
        };
    }

    fn set_encodings(&mut self, list: &[i32], msgs: &mut Vec<Vec<u8>>) {
        self.caps = Caps::parse(list);
        self.enc.set_kind(self.caps.kind());
        self.enc.cx.jpeg = self.caps.jpeg();
        // JPEG-21 for photo rects when the primary encoding isn't Tight.
        self.enc.cx.jpeg21 = (self.caps.jpeg21 && self.caps.kind() != Kind::Tight).then(|| self.caps.jpeg().unwrap_or((90, enc::Subsamp::Sub2x2)));
        self.enc.motion_cap = self.enc.cx.jpeg.or(self.enc.cx.jpeg21).map_or(100, |(q, _)| enc::motion_start(q));
        self.enc.cx.level = self.caps.zlevel();
        if self.caps.ext_desktop_size {
            self.need_ext_size = true;
        }
        if self.caps.fence && !self.fence_announced {
            self.fence_announced = true;
            msgs.push(vec![248, 0, 0, 0, 0x80, 0, 0, 0, 0]);
        }
        if self.caps.ext_clip {
            msgs.push(crate::clipboard::ext_caps());
        }
        if self.caps.cu && !self.cu_announced {
            self.cu_announced = true;
            msgs.push(vec![150]);
        }
        crate::log!("{}: encodings {:?} -> {:?} jpeg={:?} zlevel={} cu={} fence={}", self.peer, list, self.caps.kind(), self.enc.cx.jpeg, self.enc.cx.level, self.caps.cu, self.caps.fence);
    }

    fn log_stats(&mut self) {
        let s = &self.stats;
        if !self.cfg.verbose || s.since.is_none_or(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        let el = s.since.unwrap().elapsed().as_secs_f64();
        let u = s.updates.max(1) as f64;
        let (reqs, unsol) = (s.reqs, s.unsolicited);
        crate::log!("{}: {:.1} upd/s {:.2} MB/s {:.1} rects/upd {} copies {:.1} Mpx/s alr {:.1} Mpx/s deferred {} enc {:.2} ms/upd rate {:.0} px/us rtt {:.2} ms",
            self.peer, s.updates as f64 / el, s.bytes as f64 / el / 1e6, s.rects as f64 / u, s.copies, s.px as f64 / el / 1e6, s.alr_px as f64 / el / 1e6,
            s.deferred, s.enc_us as f64 / 1e3 / u, self.rate / 1e3, self.win.srtt);
        if self.cfg.verbose {
            crate::log!("{}: window depth {} base rtt {:.1} ms reqs {} unsolicited {}", self.peer, self.win.depth, if self.win.base() == f64::MAX { 0.0 } else { self.win.base() }, reqs, unsol);
        }
        self.stats = Stats { since: Some(Instant::now()), ..Default::default() };
    }

    fn ext_desktop_size(&self, snap: &Snapshot) -> Vec<u8> {
        let mut b = vec![0u8, 0, 0, 1];
        rfb::put_rect_header(&mut b, 0, 0, snap.w as u16, snap.h as u16, E::EXT_DESKTOP_SIZE);
        let layout = self.cap.layout.lock().clone();
        b.extend_from_slice(&[layout.outputs.len().max(1) as u8, 0, 0, 0]);
        if layout.outputs.is_empty() {
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&[0, 0, 0, 0]);
            b.extend_from_slice(&(snap.w as u16).to_be_bytes());
            b.extend_from_slice(&(snap.h as u16).to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
        }
        for (i, o) in layout.outputs.iter().enumerate() {
            b.extend_from_slice(&(i as u32).to_be_bytes());
            b.extend_from_slice(&((o.rect.left - layout.origin.0) as u16).to_be_bytes());
            b.extend_from_slice(&((o.rect.top - layout.origin.1) as u16).to_be_bytes());
            b.extend_from_slice(&((o.rect.right - o.rect.left) as u16).to_be_bytes());
            b.extend_from_slice(&((o.rect.bottom - o.rect.top) as u16).to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
        }
        b
    }

    /// Sends one update if there is anything to send. Returns true if an update was sent.
    fn update(&mut self, snap: &Arc<Snapshot>, full: &[Rect], ptr: Option<(i32, i32)>) -> io::Result<bool> {
        let t0 = Instant::now();
        let caps = &self.caps;
        let mut pre: Vec<Vec<u8>> = Vec::new();

        // Geometry.
        if snap.layout_seq != self.layout_seq || (snap.w, snap.h) != self.size {
            self.layout_seq = snap.layout_seq;
            self.size = (snap.w, snap.h);
            self.model = None;
            if caps.ext_desktop_size {
                self.need_ext_size = true;
            } else if caps.desktop_size {
                let mut b = Vec::new();
                rfb::put_rect_header(&mut b, 0, 0, snap.w as u16, snap.h as u16, E::DESKTOP_SIZE);
                pre.push(b);
            }
        }
        if self.need_ext_size && caps.ext_desktop_size {
            // Must not share an update with pixel data.
            let m = self.ext_desktop_size(snap);
            self.written += write_vec(&mut self.sock, &[m])? as u64;
            self.need_ext_size = false;
        }

        // Cursor shape.
        if caps.alpha_cursor || caps.rich_cursor {
            let vis = snap.cursor_visible;
            if snap.cursor_seq != self.cursor_seq || vis != self.cursor_vis {
                let mut b = Vec::new();
                match (&snap.cursor, vis) {
                    (Some(c), true) if caps.alpha_cursor => enc::cursor::alpha(c, &mut b),
                    (Some(c), true) => enc::cursor::rich(&self.enc.cx, c, &mut b),
                    _ if caps.rich_cursor => enc::cursor::empty(&mut b),
                    _ => {}
                }
                if !b.is_empty() {
                    pre.push(b);
                }
                self.cursor_seq = snap.cursor_seq;
                self.cursor_vis = vis;
            }
        }
        // Client model.
        let ntiles = snap.tiles.len();
        if self.model.as_ref().is_none_or(|m| m.tiles.len() != ntiles || m.w != snap.w || m.h != snap.h) {
            let blank = Tile::blank();
            let mut m = Snapshot::blank(snap.w, snap.h, snap.layout_seq);
            m.tiles.iter_mut().for_each(|t| *t = blank.clone());
            self.model = Some(m);
            self.known = vec![false; ntiles];
            self.lossy = vec![None; ntiles];
            self.since = vec![None; ntiles];
            self.streak = vec![0; ntiles];
            self.last_change = vec![None; ntiles];
        }
        for f in full {
            if let Some(f) = f.intersect(&Rect::new(0, 0, snap.w, snap.h)) {
                for ti in plan::tiles_of(snap, f) {
                    self.known[ti] = false;
                }
            }
        }
        let model = self.model.as_mut().unwrap();

        // Scroll -> CopyRect. Sources must be fully known to the client.
        let mut clean = vec![0u64; ntiles];
        let mut copies = Vec::new();
        if caps.copyrect && self.cfg.scroll {
            let changed: Vec<bool> = (0..ntiles).map(|ti| self.known[ti] && !Arc::ptr_eq(&model.tiles[ti], &snap.tiles[ti])).collect();
            if changed.iter().any(|&c| c) {
                for c in crate::scroll::detect(snap, model, &changed) {
                    let src = Rect::new(c.sx, c.sy, c.dst.w as u32, c.dst.h as u32);
                    if plan::tiles_of(snap, src).any(|ti| !self.known[ti]) {
                        continue;
                    }
                    let src_lossy = plan::tiles_of(snap, src).any(|ti| self.lossy[ti].is_some());
                    let d = c.dst;
                    for ti in plan::tiles_of(snap, d) {
                        let ty0 = (ti as u32 / snap.tw) * TS as u32;
                        let a = (d.y as u32).max(ty0) - ty0;
                        let b = d.bottom().min(ty0 + TS as u32) - ty0;
                        clean[ti] |= if b - a == 64 { u64::MAX } else { ((1u64 << (b - a)) - 1) << a };
                        if src_lossy {
                            self.lossy[ti] = Some(Instant::now());
                        }
                    }
                    let mut b = Vec::with_capacity(16);
                    rfb::put_rect_header(&mut b, d.x, d.y, d.w, d.h, E::COPYRECT);
                    b.extend_from_slice(&(c.sx as u16).to_be_bytes());
                    b.extend_from_slice(&(c.sy as u16).to_be_bytes());
                    copies.push(b);
                }
            }
        }
        let copy_touched: Vec<bool> = clean.iter().map(|&c| c != 0).collect();

        // Per-tile bboxes.
        let alr_after = Duration::from_millis(self.cfg.alr_ms);
        let now = Instant::now();
        let mut bbox: Vec<Option<plan::BBox>> = vec![None; ntiles];
        let mut alr = vec![false; ntiles];
        let mut settle: Vec<usize> = Vec::new();
        for ti in 0..ntiles {
            let t = &snap.tiles[ti];
            let b = if !self.known[ti] {
                Some(plan::full_bbox(snap, ti))
            } else if Arc::ptr_eq(&model.tiles[ti], t) {
                match self.lossy[ti] {
                    Some(at) if now - at >= alr_after => {
                        alr[ti] = true;
                        Some(plan::full_bbox(snap, ti))
                    }
                    _ => None,
                }
            } else {
                let (tw, th) = plan::tile_dims(snap, ti);
                let b = plan::diff_bbox(&model.tiles[ti], t, tw, th, clean[ti]);
                if b.is_none() {
                    // Same content (or fully covered by copies): adopt the new tile.
                    settle.push(ti);
                }
                b
            };
            if b.is_some() && self.since[ti].is_none() {
                self.since[ti] = Some(now);
            }
            bbox[ti] = b;
        }
        for ti in settle {
            model.tiles[ti] = snap.tiles[ti].clone();
        }

        // Schedule runs: urgent (small / near pointer / copy-touched), then oldest normal within budget, then ALR.
        let runs = plan::runs(snap, &bbox, &alr);
        let near = ptr.map(|(x, y)| Rect::new((x - 192).max(0) as u32, (y - 192).max(0) as u32, 384, 384));
        let (mut urgent, mut normal, mut refresh): (Vec<Run>, Vec<Run>, Vec<Run>) = (Vec::new(), Vec::new(), Vec::new());
        for r in runs {
            if r.alr {
                refresh.push(r);
            } else if r.rect.area() <= 16 * 1024 || near.is_some_and(|n| n.intersect(&r.rect).is_some()) || r.tiles.iter().any(|&t| copy_touched[t]) {
                urgent.push(r);
            } else {
                normal.push(r);
            }
        }
        let age = |r: &Run| r.tiles.iter().filter_map(|&t| self.since[t]).min().unwrap_or(now);
        normal.sort_by_key(|r| (age(r), r.rect.area()));
        let recent_ms = Duration::from_secs_f64(self.gap_ms.clamp(24.0, 200.0) * 2.5 / 1e3);
        let motion_now = |r: &Run| {
            self.cfg.motion && r.tiles.iter().filter(|&&t| self.streak[t] >= 2 && self.last_change[t].is_some_and(|x| now - x < recent_ms)).count() * 2 > r.tiles.len()
        };
        let bpp = |r: &Run| if r.alr { self.alr_bpp } else if motion_now(r) { self.motion_bpp } else { self.norm_bpp };
        // Link budgets in bytes: live changes ~40 ms per update, refresh 20 ms alone or 4 ms alongside.
        let link = self.pipe.rate_or_bound();
        let cap = |ms: f64| if link > 0.0 { link * ms } else { f64::INFINITY };
        let budget_px = self.rate * self.cfg.budget_ms;
        let mut used: f64 = urgent.iter().map(|r| r.rect.area() as f64).sum();
        let mut sent_b: f64 = urgent.iter().map(|r| r.rect.area() as f64 * bpp(r)).sum();
        let mut chosen: Vec<Run> = urgent;
        let mut deferred = 0u64;
        let mut took_normal = 0;
        for r in normal {
            let old = now - age(&r) > Duration::from_millis(100);
            let b = bpp(&r);
            for r in segments(snap, r, cap(40.0) / b) {
                let a = r.rect.area() as f64;
                if (used + a <= budget_px || took_normal == 0 || old) && (sent_b + a * b <= cap(40.0) || took_normal == 0) {
                    used += a;
                    sent_b += a * b;
                    took_normal += 1;
                    chosen.push(r);
                } else {
                    deferred += 1;
                }
            }
        }
        if deferred == 0 {
            let alr_cap = cap(if chosen.is_empty() { 20.0 } else { 4.0 });
            let mut alr_b = 0.0;
            for r in refresh.into_iter().flat_map(|r| segments(snap, r, (alr_cap / self.alr_bpp).max(4096.0))) {
                let a = r.rect.area() as f64;
                if (used + a <= budget_px || used == 0.0) && (alr_b + a * self.alr_bpp <= alr_cap || alr_b == 0.0) {
                    used += a;
                    alr_b += a * self.alr_bpp;
                    chosen.push(r);
                } else {
                    deferred += 1;
                }
            }
        } else {
            deferred += refresh.len() as u64;
        }
        self.deferred = deferred > 0;
        self.stats.deferred += deferred;

        // Motion streaks: tiles changing in back-to-back updates are "video".
        for r in &chosen {
            if r.alr {
                continue;
            }
            for &ti in &r.tiles {
                let recent = self.last_change[ti].is_some_and(|t| now - t < recent_ms);
                self.streak[ti] = if recent { self.streak[ti].saturating_add(1) } else { 1 };
                self.last_change[ti] = Some(now);
            }
        }
        // Below ~100 Mbit, bytes cost more than zlib effort.
        let slow = self.pipe.rate > 0.0 && self.pipe.rate < 12_500.0;
        self.enc.cx.level = if slow { self.caps.zlevel().max(6) } else { self.caps.zlevel() };
        let mut kind = self.caps.kind();
        if slow && matches!(kind, Kind::Trle | Kind::Hextile | Kind::Raw) && self.caps.kind_order.contains(&Kind::Zrle) {
            kind = Kind::Zrle;
        }
        self.enc.set_kind(kind);
        // Encoder rects (split to legal sizes).
        let max_w = if self.enc.kind == Kind::Tight { enc::tight::MAX_WIDTH } else { 4096 };
        let max_area = if self.enc.kind == Kind::Tight { 64 * 1024 } else { 128 * 1024 };
        let mut rects: Vec<(Rect, Mode)> = Vec::new();
        let mut tmp = Vec::new();
        for r in &chosen {
            tmp.clear();
            enc::split(r.rect, max_w, max_area, &mut tmp);
            let mode = if r.alr {
                Mode::Lossless
            } else if self.cfg.motion && r.tiles.iter().filter(|&&t| self.streak[t] >= 3 || !self.known[t]).count() * 2 > r.tiles.len() {
                // Also first paint: cheap now, lossless refresh follows.
                Mode::Motion
            } else {
                Mode::Normal
            };
            rects.extend(tmp.iter().map(|&x| (x, mode)));
        }
        // Local pointer moves when the client isn't driving; alone at most every 8 ms.
        if caps.pointer_pos && ptr.is_none() {
            if let Some(p) = crate::net::cursor_pos(self.cap.layout.lock().origin) {
                if p != self.last_ptr && p.0 >= 0 && p.1 >= 0 && (p.0 as u32) < snap.w && (p.1 as u32) < snap.h {
                    if !pre.is_empty() || !copies.is_empty() || !rects.is_empty() || self.ptr_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(8)) {
                        let mut b = Vec::new();
                        rfb::put_rect_header(&mut b, p.0 as u16, p.1 as u16, 0, 0, E::POINTER_POS);
                        pre.push(b);
                        self.last_ptr = p;
                        self.ptr_at = Some(now);
                    } else {
                        self.deferred = true;
                    }
                }
            }
        } else if let Some(p) = ptr {
            self.last_ptr = p;
        }
        if pre.is_empty() && copies.is_empty() && rects.is_empty() {
            return Ok(false);
        }

        // Header + pseudo + copies, then streamed rects (count unknown up front under LastRect).
        let stream_out = caps.last_rect;
        let mut first: Vec<Vec<u8>> = vec![vec![0u8, 0, 0xFF, 0xFF]];
        let fixed = pre.len() + copies.len();
        first.extend(pre);
        first.extend(copies.iter().cloned());
        let mut bytes = if stream_out { write_vec(&mut self.sock, &first)? } else { 0 };
        let px: u64 = rects.iter().filter(|r| r.1 != Mode::Lossless).map(|r| r.0.area() as u64).sum();
        let motion_px: u64 = rects.iter().filter(|r| r.1 == Mode::Motion).map(|r| r.0.area() as u64).sum();
        let alr_px: u64 = rects.iter().filter(|r| r.1 == Mode::Lossless).map(|r| r.0.area() as u64).sum();
        let mut buf: Vec<Vec<u8>> = Vec::new();
        let mut buffered = 0usize;
        let mut done: Vec<(Rect, bool)> = Vec::with_capacity(rects.len());
        let t_enc = Instant::now();
        {
            let sock = &mut self.sock;
            let mut emit = |e: Emitted| -> io::Result<()> {
                buffered += e.data.len();
                done.push((e.rect, e.lossy));
                buf.push(e.data);
                if stream_out && buffered >= 32 * 1024 {
                    bytes += write_vec(sock, &buf)?;
                    buf.clear();
                    buffered = 0;
                }
                Ok(())
            };
            self.enc.encode_stream(snap, &rects, &mut emit)?;
        }
        let count = fixed + done.len();
        if stream_out {
            let mut l = Vec::new();
            rfb::put_rect_header(&mut l, 0, 0, 0, 0, E::LAST_RECT);
            buf.push(l);
        } else {
            if count > 0xFFFF {
                return Err(io::Error::other("too many rects without LastRect"));
            }
            first[0] = vec![0u8, 0, (count >> 8) as u8, count as u8];
            first.append(&mut buf);
            buf = first;
        }
        let cu = self.cu.is_some();
        if cu {
            self.fence_seq = self.fence_seq.wrapping_add(1);
            let mut f = vec![248u8, 0, 0, 0];
            f.extend_from_slice(&(0x8000_0000u32 | 1).to_be_bytes());
            f.push(4);
            f.extend_from_slice(&self.fence_seq.to_be_bytes());
            buf.push(f);
            self.inflight.push_back((self.fence_seq, Instant::now()));
        }
        bytes += write_vec(&mut self.sock, &buf)?;
        let enc_ms = t_enc.elapsed().as_secs_f64() * 1e3;
        self.enc_ms = self.enc_ms * 0.8 + t0.elapsed().as_secs_f64() * 1e3 * 0.2;
        self.written += bytes as u64;
        self.pipe.on_send(bytes, Instant::now());
        self.adapt_motion(bytes, motion_px * 2 > px + alr_px);
        if let Some(l) = self.last_upd {
            self.gap_ms = self.gap_ms * 0.8 + (l.elapsed().as_secs_f64() * 1e3).min(250.0) * 0.2;
        }
        self.last_upd = Some(Instant::now());
        crate::probe::sent(snap);
        if crate::probe::trace() {
            crate::log!("upd id {} bytes {} px {} alr {} rects {} owed {} enc {:.1}", crate::probe::frame_id(snap), bytes, px, alr_px, count, self.owed, enc_ms);
        }

        // Model: chosen runs now match `snap`.
        let model = self.model.as_mut().unwrap();
        for r in &chosen {
            for &ti in &r.tiles {
                model.tiles[ti] = snap.tiles[ti].clone();
                self.known[ti] = true;
                self.since[ti] = None;
            }
        }
        for ti in 0..ntiles {
            if copy_touched[ti] && bbox[ti].is_none() {
                model.tiles[ti] = snap.tiles[ti].clone();
            }
        }
        update_lossy(&mut self.lossy, snap, &done);
        let all = (px + alr_px) as f64;
        let bpp = bytes as f64 / all.max(1.0);
        for (n, e) in [(alr_px, &mut self.alr_bpp), (motion_px, &mut self.motion_bpp), (px - motion_px, &mut self.norm_bpp)] {
            if n >= 16384 && n as f64 >= 0.8 * all {
                *e = *e * 0.7 + bpp * 0.3;
            }
        }
        if px + alr_px > 256 * 1024 && enc_ms > 0.5 {
            let r = (px + alr_px) as f64 / enc_ms;
            self.rate = self.rate * 0.8 + r * 0.2;
        }
        let s = &mut self.stats;
        s.updates += 1;
        s.bytes += bytes as u64;
        s.rects += count as u64;
        s.copies += copies.len() as u64;
        s.px += px;
        s.alr_px += alr_px;
        s.enc_us += t0.elapsed().as_micros() as u64;
        Ok(true)
    }
}

/// Splits a run into tile-row segments of at most `max_px` (at least one tile each).
fn segments(snap: &Snapshot, r: Run, max_px: f64) -> Vec<Run> {
    if r.rect.area() as f64 <= max_px || r.tiles.len() == 1 {
        return vec![r];
    }
    let mut out: Vec<Run> = Vec::new();
    let mut push = |rc: Rect, tiles: Vec<usize>| {
        if let Some(x) = rc.intersect(&r.rect) {
            out.push(Run { rect: x, tiles, alr: r.alr });
        }
    };
    let mut cur: Option<(Rect, Vec<usize>, u32)> = None;
    for &ti in &r.tiles {
        let tr = plan::tile_rect(snap, ti);
        let row = ti as u32 / snap.tw;
        if let Some((rc, tiles, cr)) = cur.as_mut() {
            if *cr == row && (rc.area() + tr.area()) as f64 <= max_px && rc.right() == tr.x as u32 {
                rc.w += tr.w;
                tiles.push(ti);
                continue;
            }
            push(*rc, std::mem::take(tiles));
        }
        cur = Some((tr, vec![ti], row));
    }
    if let Some((rc, tiles, _)) = cur {
        push(rc, tiles);
    }
    out
}

/// Lossy rects mark tiles; tiles fully covered by lossless rects are cleared.
fn update_lossy(lossy: &mut [Option<Instant>], snap: &Snapshot, rects: &[(Rect, bool)]) {
    let ts = TS as u32;
    let mut covered: std::collections::HashMap<usize, u32> = Default::default();
    let mut hit = Vec::new();
    let now = Instant::now();
    for &(r, is_lossy) in rects {
        for ti in plan::tiles_of(snap, r) {
            let tx = ti as u32 % snap.tw;
            let ty = ti as u32 / snap.tw;
            if is_lossy {
                lossy[ti] = Some(now);
                hit.push(ti);
            } else if let Some(i) = Rect::new(tx * ts, ty * ts, ts, ts).intersect(&r) {
                *covered.entry(ti).or_default() += i.area();
            }
        }
    }
    for (ti, a) in covered {
        let full = plan::tile_rect(snap, ti).area();
        if a >= full && !hit.contains(&ti) {
            lossy[ti] = None;
        }
    }
}
