pub mod cursor;
pub mod hextile;
pub mod jpeg;
pub mod palette;
pub mod tight;
pub mod zrle;

pub use crate::fb::Rect;
use crate::fb::Snapshot;
pub use crate::pixfmt::Converter;
pub use crate::rfb::{put_compact_len, put_rect_header};
use crate::zstream::{self, run_job, ZStream, PAR_MIN};
pub use palette::Palette;
use std::cell::RefCell;
use std::io;
use std::sync::mpsc;
use std::sync::Arc;
pub use turbojpeg::Subsamp;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Tight,
    Zrle,
    Trle,
    Zlib,
    Hextile,
    Raw,
}

#[derive(Clone)]
pub struct Ctx {
    pub conv: Converter,
    pub jpeg: Option<(u8, Subsamp)>,
    pub jpeg_min_area: usize,
    pub force_lossless: bool,
    pub level: i32,
    /// Client accepts JPEG encoding 21 for photographic rects (non-Tight kinds).
    pub jpeg21: Option<(u8, Subsamp)>,
}

pub enum Part {
    Done(Vec<u8>),
    Z { hdr: Vec<u8>, stream: usize, raw: Vec<u8> },
}

/// Per-rect encoding intent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    /// Lossless refresh.
    Lossless,
    /// Continuously changing: cheaper JPEG (lower quality, 4:2:0).
    Motion,
}

/// Link-driven: `cap` starts at 3/4 of `q` and rises while the link has headroom.
pub fn motion_q(q: u8, cap: u8) -> u8 {
    q.min(cap)
}

pub fn motion_start(q: u8) -> u8 {
    (q as u32 * 3 / 4).max(30) as u8
}

fn motion_jpeg(j: Option<(u8, Subsamp)>, cap: u8) -> Option<(u8, Subsamp)> {
    j.map(|(q, _)| (motion_q(q, cap), Subsamp::Sub2x2))
}

pub struct Emitted {
    pub data: Vec<u8>,
    pub rect: Rect,
    pub lossy: bool,
}

pub struct Encoder {
    pub kind: Kind,
    pub cx: Ctx,
    /// Link-driven ceiling on motion JPEG quality.
    pub motion_cap: u8,
    tight: [ZStream; 4],
    zrle: ZStream,
    zlib: ZStream,
    next_stream: usize,
}

thread_local! {
    static SCRATCH: RefCell<(Vec<u32>, Palette)> = RefCell::new((Vec::new(), Palette::default()));
}

type Item = (Rect, Part, bool);

fn jpeg21_wanted(cx: &Ctx, px: &[u32], r: Rect, pal: &mut Palette) -> bool {
    cx.jpeg21.is_some()
        && !cx.force_lossless
        && cx.conv.pf.bpp >= 16
        && px.len() >= cx.jpeg_min_area
        && !crate::simd::all_eq(px, px[0])
        && pal.count(px, 256).is_none()
        && tight::photo_like(px, r.w as usize)
}

fn stage1(cx: &Ctx, kind: Kind, snap: &Snapshot, r: Rect, stream: usize) -> Vec<Item> {
    SCRATCH.with(|s| {
        let (buf, pal) = &mut *s.borrow_mut();
        snap.gather(r, buf);
        match kind {
            Kind::Tight => {
                let mut out = Vec::with_capacity(1);
                let mut st = stream;
                tight::encode_into(cx, r, buf, pal, &mut st, &mut out);
                out
            }
            _ if jpeg21_wanted(cx, buf, r, pal) => {
                let (q, ss) = cx.jpeg21.unwrap();
                let mut o = Vec::with_capacity(buf.len() / 4);
                put_rect_header(&mut o, r.x, r.y, r.w, r.h, crate::rfb::enc::JPEG);
                o.extend_from_slice(&jpeg::compress(buf, r.w as usize, r.h as usize, q, ss));
                vec![(r, Part::Done(o), true)]
            }
            Kind::Zrle => {
                let mut hdr = Vec::with_capacity(16);
                put_rect_header(&mut hdr, r.x, r.y, r.w, r.h, crate::rfb::enc::ZRLE);
                let mut raw = Vec::with_capacity(buf.len() / 2);
                zrle::encode_rect(cx, r, buf, pal, 64, &mut raw);
                vec![(r, Part::Z { hdr, stream: 0, raw }, false)]
            }
            Kind::Trle => {
                let mut o = Vec::with_capacity(buf.len());
                put_rect_header(&mut o, r.x, r.y, r.w, r.h, crate::rfb::enc::TRLE);
                zrle::encode_rect(cx, r, buf, pal, 16, &mut o);
                vec![(r, Part::Done(o), false)]
            }
            Kind::Zlib => {
                let mut hdr = Vec::with_capacity(16);
                put_rect_header(&mut hdr, r.x, r.y, r.w, r.h, crate::rfb::enc::ZLIB);
                let mut raw = Vec::with_capacity(buf.len() * cx.conv.pf.bytes_pp());
                cx.conv.put_slice(buf, &mut raw);
                vec![(r, Part::Z { hdr, stream: 0, raw }, false)]
            }
            Kind::Hextile => {
                let mut o = Vec::with_capacity(buf.len());
                hextile::encode(cx, r, buf, &mut o);
                vec![(r, Part::Done(o), false)]
            }
            Kind::Raw => {
                let mut o = Vec::with_capacity(12 + buf.len() * cx.conv.pf.bytes_pp());
                put_rect_header(&mut o, r.x, r.y, r.w, r.h, crate::rfb::enc::RAW);
                cx.conv.put_slice(buf, &mut o);
                vec![(r, Part::Done(o), false)]
            }
        }
    })
}

fn finish(kind: Kind, mut hdr: Vec<u8>, raw: &[u8], z: Option<&[u8]>) -> Vec<u8> {
    match kind {
        Kind::Tight => tight::finish(&mut hdr, raw, z),
        _ => {
            let z = z.unwrap_or_default();
            hdr.extend_from_slice(&(z.len() as u32).to_be_bytes());
            hdr.extend_from_slice(z);
        }
    }
    hdr
}

struct ZPart {
    rect: Rect,
    hdr: Vec<u8>,
    stream: usize,
    raw: Arc<Vec<u8>>,
    lossy: bool,
}

impl Encoder {
    pub fn new(kind: Kind, cx: Ctx) -> Encoder {
        Encoder { kind, cx, motion_cap: 100, tight: std::array::from_fn(|_| ZStream::new()), zrle: ZStream::new(), zlib: ZStream::new(), next_stream: 0 }
    }

    pub fn set_kind(&mut self, k: Kind) {
        self.kind = k;
    }

    fn stream(&mut self, kind: Kind, s: usize) -> &mut ZStream {
        match kind {
            Kind::Tight => &mut self.tight[s],
            Kind::Zrle => &mut self.zrle,
            _ => &mut self.zlib,
        }
    }

    /// Encodes rects in parallel, emitting each output rect as soon as it is ready
    /// (per-stream order kept). One input may yield several outputs (Tight subdivision).
    pub fn encode_stream(&mut self, snap: &Arc<Snapshot>, rects: &[(Rect, Mode)], emit: &mut dyn FnMut(Emitted) -> io::Result<()>) -> io::Result<()> {
        let n = rects.len();
        if n == 0 {
            return Ok(());
        }
        let kind = self.kind;
        let (tx, rx) = mpsc::channel::<(usize, Vec<Item>)>();
        let base = Arc::new(self.cx.clone());
        let mut lossless_cx: Option<Arc<Ctx>> = None;
        let mut motion_cx: Option<Arc<Ctx>> = None;
        for (i, &(r, mode)) in rects.iter().enumerate() {
            let cx = match mode {
                Mode::Normal => base.clone(),
                Mode::Lossless => lossless_cx.get_or_insert_with(|| Arc::new(Ctx { force_lossless: true, ..self.cx.clone() })).clone(),
                Mode::Motion => motion_cx
                    .get_or_insert_with(|| Arc::new(Ctx { jpeg: motion_jpeg(self.cx.jpeg, self.motion_cap), jpeg21: motion_jpeg(self.cx.jpeg21, self.motion_cap), ..self.cx.clone() }))
                    .clone(),
            };
            let (tx, snap) = (tx.clone(), snap.clone());
            let stream = self.next_stream;
            self.next_stream = (self.next_stream + 1) & 3;
            let job = move || {
                let _ = tx.send((i, stage1(&cx, kind, &snap, r, stream)));
            };
            if n == 1 {
                job();
            } else {
                rayon::spawn(job);
            }
        }
        drop(tx);
        // Stage 1 results: finished rects go out immediately; zlib parts wait, ordered by (input, sub).
        let mut zparts: Vec<((usize, usize), ZPart)> = Vec::new();
        let mut ztotal = 0usize;
        for _ in 0..n {
            let (i, items) = rx.recv().expect("encoder worker died");
            for (j, (rect, p, lossy)) in items.into_iter().enumerate() {
                match p {
                    Part::Done(d) => emit(Emitted { data: d, rect, lossy })?,
                    Part::Z { hdr, raw, .. } if kind == Kind::Tight && raw.len() < 12 => {
                        emit(Emitted { data: finish(kind, hdr, &raw, None), rect, lossy })?
                    }
                    Part::Z { hdr, stream, raw } => {
                        ztotal += raw.len();
                        zparts.push(((i, j), ZPart { rect, hdr, stream, raw: Arc::new(raw), lossy }));
                    }
                }
            }
        }
        if zparts.is_empty() {
            return Ok(());
        }
        zparts.sort_by_key(|z| z.0);
        let mut zparts: Vec<Option<ZPart>> = zparts.into_iter().map(|z| Some(z.1)).collect();
        let level = self.cx.level;
        // Small: sequential on the persistent streams.
        if ztotal < PAR_MIN {
            for zp in zparts.into_iter().flatten() {
                let z = self.stream(kind, zp.stream).compress_seq(level, &zp.raw);
                emit(Emitted { data: finish(kind, zp.hdr, &zp.raw, Some(&z)), rect: zp.rect, lossy: zp.lossy })?;
            }
            return Ok(());
        }
        // Large: dictionary-primed chunks per stream, run in parallel, reassembled in stream order.
        let m = zparts.len();
        let nstreams = if kind == Kind::Tight { 4 } else { 1 };
        let mut order: Vec<Vec<usize>> = vec![Vec::new(); nstreams];
        for (i, zp) in zparts.iter().enumerate() {
            order[zp.as_ref().unwrap().stream].push(i);
        }
        let (tx, rx) = mpsc::channel::<(Vec<zstream::Piece>, Vec<Vec<u8>>)>();
        let mut pieces_needed = vec![0usize; m];
        let mut jobs_total = 0;
        let raws: Arc<Vec<Arc<Vec<u8>>>> = Arc::new(zparts.iter().map(|z| z.as_ref().unwrap().raw.clone()).collect());
        for (s, idxs) in order.iter().enumerate() {
            if idxs.is_empty() {
                continue;
            }
            let inputs: Vec<&[u8]> = idxs.iter().map(|&i| zparts[i].as_ref().unwrap().raw.as_slice()).collect();
            let mut jobs = self.stream(kind, s).plan(&inputs);
            for j in jobs.iter_mut() {
                for p in j.pieces.iter_mut() {
                    p.input = idxs[p.input];
                    pieces_needed[p.input] = pieces_needed[p.input].max(p.piece + 1);
                }
            }
            for j in jobs {
                jobs_total += 1;
                let (tx, raws) = (tx.clone(), raws.clone());
                rayon::spawn(move || {
                    let o = run_job(level, &j, &|i| raws[i].clone());
                    let _ = tx.send((j.pieces, o));
                });
            }
        }
        drop(tx);
        let mut got: Vec<Vec<Option<Vec<u8>>>> = pieces_needed.iter().map(|&c| vec![None; c]).collect();
        let mut remaining = pieces_needed.clone();
        let mut cursor = vec![0usize; nstreams];
        for _ in 0..jobs_total {
            let (pieces, outs) = rx.recv().expect("zlib worker died");
            for (p, o) in pieces.iter().zip(outs) {
                got[p.input][p.piece] = Some(o);
                remaining[p.input] -= 1;
            }
            // Flush every stream whose next part is complete.
            for s in 0..nstreams {
                while cursor[s] < order[s].len() && remaining[order[s][cursor[s]]] == 0 {
                    let i = order[s][cursor[s]];
                    cursor[s] += 1;
                    let z: Vec<u8> = got[i].drain(..).flat_map(|c| c.unwrap()).collect();
                    let zp = zparts[i].take().unwrap();
                    emit(Emitted { data: finish(kind, zp.hdr, &zp.raw, Some(&z)), rect: zp.rect, lossy: zp.lossy })?;
                }
            }
        }
        Ok(())
    }
}

/// Splits a rect into pieces of at most `max_w` width and about `max_area` pixels.
pub fn split(r: Rect, max_w: u32, max_area: u32, out: &mut Vec<Rect>) {
    let mut x = r.x as u32;
    while x < r.right() {
        let w = (r.right() - x).min(max_w);
        let rows = (max_area / w).max(1);
        let mut y = r.y as u32;
        while y < r.bottom() {
            let h = (r.bottom() - y).min(rows);
            out.push(Rect::new(x, y, w, h));
            y += h;
        }
        x += w;
    }
}
