//! Size vs time per zlib level: cargo run --release --example zlevel -- <dir>
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
        let mut rects = Vec::new();
        enc::split(Rect::new(0, 0, w, h), 2048, 64 * 1024, &mut rects);
        let rects: Vec<(Rect, Mode)> = rects.into_iter().map(|r| (r, Mode::Motion)).collect();
        for level in [1, 2, 3, 6, 9] {
            let mut best = f64::MAX;
            let mut bytes = 0;
            for _ in 0..3 {
                let mut e = Encoder::new(Kind::Tight, Ctx { conv: Converter::new(PixelFormat::NATIVE), jpeg: Some((77, Subsamp::Sub2x1)), jpeg_min_area: 4096, force_lossless: false, level, jpeg21: None });
                let t = Instant::now();
                let mut n = 0;
                e.encode_stream(&snap, &rects, &mut |x| { n += x.data.len(); Ok(()) }).unwrap();
                best = best.min(t.elapsed().as_secs_f64());
                bytes = n;
            }
            println!("{name:6} level {level} {:7.2} ms {:7} KB", best * 1e3, bytes / 1024);
        }
    }
}
