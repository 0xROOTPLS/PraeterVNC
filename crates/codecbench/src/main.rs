use std::time::Instant;

const W: usize = 2560;
const H: usize = 1440;

fn to_rgb(bgra: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(bgra.len() / 4 * 3);
    for p in bgra.chunks_exact(4) {
        o.extend_from_slice(&[p[2], p[1], p[0]]);
    }
    o
}

fn zbox<T>() -> Box<T> {
    unsafe { Box::from_raw(std::alloc::alloc_zeroed(std::alloc::Layout::new::<T>()) as *mut T) }
}

trait Z {
    fn name(&self) -> &'static str;
    fn compress(&mut self, input: &[u8], out: &mut Vec<u8>);
}

struct Rs(Box<libz_rs_sys::z_stream>);
impl Rs {
    fn new(level: i32) -> Self {
        unsafe {
            let mut s: Box<libz_rs_sys::z_stream> = zbox();
            let r = libz_rs_sys::deflateInit2_(&mut *s, level, 8, 15, 9, 0, libz_rs_sys::zlibVersion(), std::mem::size_of::<libz_rs_sys::z_stream>() as i32);
            assert_eq!(r, 0);
            Rs(s)
        }
    }
}
impl Z for Rs {
    fn name(&self) -> &'static str { "zlib-rs" }
    fn compress(&mut self, input: &[u8], out: &mut Vec<u8>) {
        unsafe {
            let s = &mut *self.0;
            out.clear();
            out.reserve(input.len() + input.len() / 100 + 64);
            s.next_in = input.as_ptr();
            s.avail_in = input.len() as u32;
            s.next_out = out.as_mut_ptr();
            s.avail_out = out.capacity() as u32;
            let r = libz_rs_sys::deflate(s, 2);
            assert!(r == 0 || r == -5, "{r}");
            out.set_len(out.capacity() - s.avail_out as usize);
        }
    }
}

struct Mz(Box<miniz_oxide::deflate::core::CompressorOxide>);
impl Z for Mz {
    fn name(&self) -> &'static str { "miniz" }
    fn compress(&mut self, input: &[u8], out: &mut Vec<u8>) {
        use miniz_oxide::deflate::core::*;
        out.clear();
        out.resize(input.len() + input.len() / 100 + 64, 0);
        let (_, _, n) = compress(&mut self.0, input, out, TDEFLFlush::Sync);
        out.truncate(n);
    }
}

fn main() {
    let dir = std::env::args().nth(1).expect("frames dir");
    let names = ["mon0", "mon1", "web", "code", "photo"];
    let rect_rows = 64;
    let rect_w = 2048;
    for n in names {
        let bgra = std::fs::read(format!("{dir}/{n}_{W}x{H}.bgra")).unwrap();
        // simulate rects of 2048x64 as TPIXEL rgb
        let mut rects = Vec::new();
        for y in (0..H).step_by(rect_rows) {
            for x in (0..W).step_by(rect_w) {
                let x1 = (x + rect_w).min(W);
                let y1 = (y + rect_rows).min(H);
                let mut buf = Vec::new();
                for yy in y..y1 {
                    buf.extend_from_slice(&bgra[(yy * W + x) * 4..(yy * W + x1) * 4]);
                }
                rects.push(to_rgb(&buf));
            }
        }
        let total: usize = rects.iter().map(|r| r.len()).sum();
        println!("== {n}: rgb {:.1} MB", total as f64 / 1e6);
        for level in [1, 2, 3, 6] {
            let mut zs: Vec<Box<dyn Z>> = vec![Box::new(Rs::new(level)), Box::new(Mz(Box::new(miniz_oxide::deflate::core::CompressorOxide::new(miniz_oxide::deflate::core::create_comp_flags_from_zip_params(level, 15, 0)))))];
            for z in zs.iter_mut() {
                let mut out = Vec::new();
                let mut best = f64::MAX;
                let mut sz = 0;
                for _ in 0..3 {
                    sz = 0;
                    let t = Instant::now();
                    for r in &rects {
                        z.compress(r, &mut out);
                        sz += out.len();
                    }
                    best = best.min(t.elapsed().as_secs_f64());
                }
                println!("  L{level} {:8} ratio {:6.1}x  {:7.0} MB/s  {:6.2} ms", z.name(), total as f64 / sz as f64, total as f64 / best / 1e6, best * 1e3);
            }
        }
        for (q, ss) in [(60, turbojpeg::Subsamp::Sub2x2), (80, turbojpeg::Subsamp::Sub2x2), (95, turbojpeg::Subsamp::None)] {
            let mut c = turbojpeg::Compressor::new().unwrap();
            c.set_quality(q).unwrap();
            c.set_subsamp(ss).unwrap();
            let img = turbojpeg::Image { pixels: &bgra[..], width: W, pitch: W * 4, height: H, format: turbojpeg::PixelFormat::BGRX };
            let mut best = f64::MAX;
            let mut sz = 0;
            for _ in 0..3 {
                let t = Instant::now();
                sz = c.compress_to_vec(img).unwrap().len();
                best = best.min(t.elapsed().as_secs_f64());
            }
            println!("  turbojpeg q{q} {:?}: ratio {:6.1}x  {:6.1} MP/s  {:6.2} ms", ss, (W * H * 3) as f64 / sz as f64, (W * H) as f64 / best / 1e6, best * 1e3);
        }
        {
            let mut best = f64::MAX;
            let mut sz = 0;
            for _ in 0..3 {
                let mut out = Vec::new();
                let t = Instant::now();
                let mut e = jpeg_encoder::Encoder::new(&mut out, 80);
                e.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_2_0);
                e.encode(&bgra, W as u16, H as u16, jpeg_encoder::ColorType::Bgra).unwrap();
                best = best.min(t.elapsed().as_secs_f64());
                sz = out.len();
            }
            println!("  jpeg-encoder q80 420: ratio {:6.1}x  {:6.1} MP/s  {:6.2} ms", (W * H * 3) as f64 / sz as f64, (W * H) as f64 / best / 1e6, best * 1e3);
        }
    }
}
