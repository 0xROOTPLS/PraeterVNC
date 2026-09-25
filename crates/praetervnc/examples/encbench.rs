//! Encoder throughput on captured frames: cargo run --release --example encbench -- <dir>
use praetervnc::enc::{self, Ctx, Encoder, Kind, Mode, Rect, Subsamp};
use praetervnc::fb::Snapshot;
use praetervnc::pixfmt::{Converter, PixelFormat};
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let dir = std::env::args().nth(1).expect("frames dir");
    let (w, h) = (2560u32, 1440u32);
    for name in ["mon0", "web", "code", "photo"] {
        let raw = std::fs::read(format!("{dir}/{name}_{w}x{h}.bgra")).unwrap();
        let px: Vec<u32> = raw.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let snap = Arc::new(Snapshot::from_pixels(w, h, &px));
        let cases: Vec<(&str, Kind, Option<(u8, Subsamp)>, u32)> = vec![
            ("tight-lossless", Kind::Tight, None, 64 * 1024),
            ("tight-jpeg79-444", Kind::Tight, Some((79, Subsamp::None)), 64 * 1024),
            ("tight-jpeg79-444-256k", Kind::Tight, Some((79, Subsamp::None)), 256 * 1024),
            ("tight-jpeg62-420", Kind::Tight, Some((62, Subsamp::Sub2x2)), 64 * 1024),
            ("zrle", Kind::Zrle, None, 128 * 1024),
        ];
        for (label, kind, jpeg, area) in cases {
            let mut rects = Vec::new();
            let max_w = if kind == Kind::Tight { 2048 } else { 4096 };
            enc::split(Rect::new(0, 0, w, h), max_w, area, &mut rects);
            let rects: Vec<(Rect, Mode)> = rects.into_iter().map(|r| (r, Mode::Normal)).collect();
            let mut best = f64::MAX;
            let mut bytes = 0;
            for _ in 0..5 {
                let mut e = Encoder::new(kind, Ctx { conv: Converter::new(PixelFormat::NATIVE), jpeg, jpeg_min_area: 4096, force_lossless: false, level: 1, jpeg21: None });
                let t = Instant::now();
                let mut n = 0;
                e.encode_stream(&snap, &rects, &mut |x| { n += x.data.len(); Ok(()) }).unwrap();
                best = best.min(t.elapsed().as_secs_f64());
                bytes = n;
            }
            println!("{name:6} {label:24} {:6.2} ms  {:6.0} Mpx/s  {:8} KB  ({} rects)", best * 1e3, (w * h) as f64 / best / 1e6, bytes / 1024, rects.len());
        }
    }
}
