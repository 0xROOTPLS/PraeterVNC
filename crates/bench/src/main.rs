//! Headless RFB benchmark client. Emulates viewer encoding profiles, decodes everything,
//! and measures end-to-end latency via the test app's barcode + shared-memory timestamps.
use libz_rs_sys as z;
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Performance::*;
use windows::Win32::System::Threading::*;
use windows::Win32::Foundation::*;

const MAGIC: u64 = 0x5052_4145_5445_5231;
const RING: usize = 65536;
const CELL: i32 = 16;

#[repr(C)]
struct Shm {
    magic: u64,
    bar_x: i32,
    bar_y: i32,
    frames: u64,
    ts: [u64; RING],
    win: [i32; 4],
    final_ready: u32,
    _pad: u32,
}

fn code_ok(v: u32) -> Option<u32> {
    let id = v & 0xFF_FFFF;
    ((id.wrapping_mul(0x9E37_79B1) >> 24) == v >> 24).then_some(id)
}

fn qpc() -> u64 {
    let mut v = 0i64;
    unsafe { let _ = QueryPerformanceCounter(&mut v); }
    v as u64
}

fn qpf() -> f64 {
    let mut v = 0i64;
    unsafe { let _ = QueryPerformanceFrequency(&mut v); }
    v as f64
}

struct Counting<R> {
    r: R,
    n: u64,
}
impl<R: Read> Read for Counting<R> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let n = self.r.read(b)?;
        self.n += n as u64;
        Ok(n)
    }
}

struct Inflater(Box<z::z_stream>);
impl Inflater {
    fn new() -> Inflater {
        unsafe {
            let mut s: Box<z::z_stream> = Box::from_raw(std::alloc::alloc_zeroed(std::alloc::Layout::new::<z::z_stream>()) as *mut z::z_stream);
            assert_eq!(z::inflateInit_(&mut *s, z::zlibVersion(), std::mem::size_of::<z::z_stream>() as i32), 0);
            Inflater(s)
        }
    }
    fn inflate(&mut self, input: &[u8], out: &mut Vec<u8>, expect: usize) -> io::Result<()> {
        out.clear();
        out.reserve(expect.max(64));
        unsafe {
            let s = &mut *self.0;
            s.next_in = input.as_ptr();
            s.avail_in = input.len() as u32;
            loop {
                if out.capacity() - out.len() < 4096 {
                    out.reserve(out.capacity().max(65536));
                }
                let len = out.len();
                s.next_out = out.as_mut_ptr().add(len);
                s.avail_out = (out.capacity() - len) as u32;
                let before = s.avail_out;
                let r = z::inflate(s, 2);
                out.set_len(len + (before - s.avail_out) as usize);
                if r != 0 && r != -5 {
                    return Err(io::Error::other(format!("inflate error {r}")));
                }
                if s.avail_in == 0 && s.avail_out != 0 {
                    break;
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Profile {
    TightVnc,
    RealVnc,
    TigerVnc,
    Raw,
    Hextile,
    Zrle,
    TightLossless,
    TightVnc28,
    RealVnc7,
}

impl Profile {
    fn parse(s: &str) -> Profile {
        match s {
            "tightvnc" => Profile::TightVnc,
            "realvnc" => Profile::RealVnc,
            "tigervnc" => Profile::TigerVnc,
            "raw" => Profile::Raw,
            "hextile" => Profile::Hextile,
            "zrle" => Profile::Zrle,
            "tight-lossless" => Profile::TightLossless,
            "tightvnc28" => Profile::TightVnc28,
            "realvnc7" => Profile::RealVnc7,
            _ => panic!("unknown profile {s}"),
        }
    }
    fn encodings(&self) -> Vec<i32> {
        match self {
            // TightVNC 2.8 viewer defaults: Tight, compression 6, JPEG quality 6.
            Profile::TightVnc => vec![7, 16, 5, 6, 2, 1, 0, -250, -26, -239, -232, -223, -224, -240],
            Profile::TightLossless => vec![7, 16, 5, 1, 0, -250, -239, -223, -224],
            // RealVNC viewer against third-party servers: ZRLE first.
            Profile::RealVnc => vec![16, 5, 2, 1, 0, -239, -223],
            Profile::TigerVnc => vec![7, 16, 5, 2, 1, 0, -254, -24, -239, -314, -223, -224, -258, -308, -312, -313],
            Profile::Raw => vec![0, 1, -239, -223],
            Profile::Hextile => vec![5, 1, 0, -239, -223],
            Profile::Zrle => vec![16, 1, 0, -239, -223, -224],
            // Observed from the real viewers.
            Profile::TightVnc28 => vec![7, 1, 16, 5, 2, 0, -26, -223, -224, -232, -239],
            Profile::RealVnc7 => vec![24, 15, 5, 16, 22, 21, 6, 2, 0, 1, -314, -239, -223],
        }
    }
}

struct Client {
    r: Counting<BufReader<TcpStream>>,
    w: TcpStream,
    fb: Vec<u32>,
    fw: usize,
    fh: usize,
    tight: [Inflater; 4],
    zrle: Inflater,
    zlib: Inflater,
    jpeg: turbojpeg::Decompressor,
    buf: Vec<u8>,
    zbuf: Vec<u8>,
    cu_enabled: bool,
    fences: u64,
    t_first: Instant,
    cov: Vec<bool>,
    cov_left: usize,
    painted: Option<Instant>,
}

fn rd<const N: usize>(r: &mut impl Read) -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b)?;
    Ok(b)
}
fn u8r(r: &mut impl Read) -> io::Result<u8> { Ok(rd::<1>(r)?[0]) }
fn u16r(r: &mut impl Read) -> io::Result<u16> { Ok(u16::from_be_bytes(rd::<2>(r)?)) }
fn u32r(r: &mut impl Read) -> io::Result<u32> { Ok(u32::from_be_bytes(rd::<4>(r)?)) }

fn compact(r: &mut impl Read) -> io::Result<usize> {
    let b0 = u8r(r)? as usize;
    let mut n = b0 & 0x7F;
    if b0 & 0x80 != 0 {
        let b1 = u8r(r)? as usize;
        n |= (b1 & 0x7F) << 7;
        if b1 & 0x80 != 0 {
            n |= (u8r(r)? as usize) << 14;
        }
    }
    Ok(n)
}

impl Client {
    fn connect(addr: &str, password: Option<&str>) -> io::Result<Client> {
        let s = TcpStream::connect(addr)?;
        s.set_nodelay(true)?;
        let w = s.try_clone()?;
        let mut c = Client {
            r: Counting { r: BufReader::with_capacity(1 << 20, s), n: 0 },
            w, fb: Vec::new(), fw: 0, fh: 0, cov: Vec::new(), cov_left: 0, painted: None,
            tight: std::array::from_fn(|_| Inflater::new()), zrle: Inflater::new(), zlib: Inflater::new(),
            jpeg: turbojpeg::Decompressor::new().unwrap(), buf: Vec::new(), zbuf: Vec::new(), cu_enabled: false, fences: 0, t_first: Instant::now(),
        };
        let v = rd::<12>(&mut c.r)?;
        eprintln!("server version {:?}", std::str::from_utf8(&v).unwrap_or("?").trim());
        c.w.write_all(b"RFB 003.008\n")?;
        let n = u8r(&mut c.r)?;
        if n == 0 {
            let len = u32r(&mut c.r)? as usize;
            let mut m = vec![0u8; len];
            c.r.read_exact(&mut m)?;
            return Err(io::Error::other(String::from_utf8_lossy(&m).to_string()));
        }
        let mut types = vec![0u8; n as usize];
        c.r.read_exact(&mut types)?;
        eprintln!("security types {types:?}");
        if types.contains(&1) && password.is_none() {
            c.w.write_all(&[1])?;
        } else if types.contains(&2) {
            c.w.write_all(&[2])?;
            let ch = rd::<16>(&mut c.r)?;
            use des::cipher::{BlockEncrypt, KeyInit};
            let mut key = [0u8; 8];
            for (i, b) in password.unwrap_or("").bytes().take(8).enumerate() {
                key[i] = b.reverse_bits();
            }
            let d = des::Des::new_from_slice(&key).unwrap();
            let mut out = ch;
            for blk in out.chunks_exact_mut(8) {
                d.encrypt_block(blk.into());
            }
            c.w.write_all(&out)?;
        } else {
            return Err(io::Error::other("no usable security type"));
        }
        let res = u32r(&mut c.r)?;
        if res != 0 {
            return Err(io::Error::other("security failed"));
        }
        c.w.write_all(&[1])?; // shared
        c.fw = u16r(&mut c.r)? as usize;
        c.fh = u16r(&mut c.r)? as usize;
        let pf = rd::<16>(&mut c.r)?;
        let nl = u32r(&mut c.r)? as usize;
        let mut name = vec![0u8; nl];
        c.r.read_exact(&mut name)?;
        eprintln!("desktop {}x{} name={:?} pf={:?}", c.fw, c.fh, String::from_utf8_lossy(&name), pf);
        c.fb = vec![0; c.fw * c.fh];
        // Force 32bpp little-endian BGRX.
        let mut m = vec![0u8, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];
        c.w.write_all(&m)?;
        m.clear();
        Ok(c)
    }

    fn set_encodings(&mut self, e: &[i32]) -> io::Result<()> {
        let mut m = vec![2u8, 0];
        m.extend_from_slice(&(e.len() as u16).to_be_bytes());
        for v in e {
            m.extend_from_slice(&v.to_be_bytes());
        }
        self.w.write_all(&m)
    }

    fn request(&mut self, inc: bool) -> io::Result<()> {
        let mut m = vec![3u8, inc as u8, 0, 0, 0, 0];
        m.extend_from_slice(&(self.fw as u16).to_be_bytes());
        m.extend_from_slice(&(self.fh as u16).to_be_bytes());
        self.w.write_all(&m)
    }

    fn enable_cu(&mut self) -> io::Result<()> {
        let mut m = vec![150u8, 1, 0, 0, 0, 0];
        m.extend_from_slice(&(self.fw as u16).to_be_bytes());
        m.extend_from_slice(&(self.fh as u16).to_be_bytes());
        self.cu_enabled = true;
        self.w.write_all(&m)
    }

    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: u32) {
        for yy in y..y + h {
            self.fb[yy * self.fw + x..yy * self.fw + x + w].fill(c);
        }
    }

    fn put_rgb(&mut self, x: usize, y: usize, w: usize, h: usize, px: &[u8], bpp: usize) {
        for yy in 0..h {
            let row = &mut self.fb[(y + yy) * self.fw + x..(y + yy) * self.fw + x + w];
            let src = &px[yy * w * bpp..(yy + 1) * w * bpp];
            if bpp == 3 {
                for (d, s) in row.iter_mut().zip(src.chunks_exact(3)) {
                    *d = (s[0] as u32) << 16 | (s[1] as u32) << 8 | s[2] as u32;
                }
            } else {
                for (d, s) in row.iter_mut().zip(src.chunks_exact(4)) {
                    *d = u32::from_le_bytes([s[0], s[1], s[2], s[3]]) & 0xFF_FFFF;
                }
            }
        }
    }

    /// Reads one server message. Returns Some(rect count) for a FramebufferUpdate.
    /// Marks 16px cells fully inside the rect; records when the whole screen was painted.
    fn cover(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if self.painted.is_some() || self.fw == 0 {
            return;
        }
        let (cw, ch) = (self.fw.div_ceil(16), self.fh.div_ceil(16));
        if self.cov.is_empty() {
            self.cov = vec![false; cw * ch];
            self.cov_left = cw * ch;
        }
        for cy in y / 16..((y + h).div_ceil(16)).min(ch) {
            for cx in x / 16..((x + w).div_ceil(16)).min(cw) {
                let (x0, y0, x1, y1) = (cx * 16, cy * 16, (cx * 16 + 16).min(self.fw), (cy * 16 + 16).min(self.fh));
                if x0 >= x && y0 >= y && x1 <= x + w && y1 <= y + h && !self.cov[cy * cw + cx] {
                    self.cov[cy * cw + cx] = true;
                    self.cov_left -= 1;
                }
            }
        }
        if self.cov_left == 0 {
            self.painted = Some(Instant::now());
            eprintln!("painted after {} KB", self.r.n / 1024);
        }
    }

    fn read_msg(&mut self) -> io::Result<Option<u32>> {
        let t = u8r(&mut self.r)?;
        self.t_first = Instant::now();
        match t {
            0 => {
                u8r(&mut self.r)?;
                let n = u16r(&mut self.r)?;
                let mut count = 0;
                let mut i = 0u32;
                while n == 0xFFFF || i < n as u32 {
                    i += 1;
                    let x = u16r(&mut self.r)? as usize;
                    let y = u16r(&mut self.r)? as usize;
                    let w = u16r(&mut self.r)? as usize;
                    let h = u16r(&mut self.r)? as usize;
                    let e = u32r(&mut self.r)? as i32;
                    if e == -224 {
                        break;
                    }
                    self.rect(x, y, w, h, e)?;
                    if e >= 0 {
                        self.cover(x, y, w, h);
                    }
                    count += 1;
                }
                Ok(Some(count))
            }
            1 => {
                u8r(&mut self.r)?;
                u16r(&mut self.r)?;
                let n = u16r(&mut self.r)? as usize;
                io::copy(&mut (&mut self.r).take(n as u64 * 6), &mut io::sink())?;
                Ok(None)
            }
            2 => Ok(None),
            3 => {
                rd::<3>(&mut self.r)?;
                let n = u32r(&mut self.r)? as usize;
                io::copy(&mut (&mut self.r).take(n as u64), &mut io::sink())?;
                Ok(None)
            }
            150 => Ok(None),
            248 => {
                rd::<3>(&mut self.r)?;
                let flags = u32r(&mut self.r)?;
                let len = u8r(&mut self.r)? as usize;
                let mut p = vec![0u8; len];
                self.r.read_exact(&mut p)?;
                if flags & 0x8000_0000 != 0 {
                    let mut m = vec![248u8, 0, 0, 0];
                    m.extend_from_slice(&(flags & 7).to_be_bytes());
                    m.push(len as u8);
                    m.extend_from_slice(&p);
                    self.w.write_all(&m)?;
                    self.fences += 1;
                }
                Ok(None)
            }
            _ => Err(io::Error::other(format!("unknown server message {t}"))),
        }
    }

    fn rect(&mut self, x: usize, y: usize, w: usize, h: usize, e: i32) -> io::Result<()> {
        if e >= 0 && (x + w > self.fw || y + h > self.fh) {
            return Err(io::Error::other(format!("rect out of bounds {x},{y} {w}x{h} enc {e}")));
        }
        match e {
            0 => {
                self.buf.resize(w * h * 4, 0);
                self.r.read_exact(&mut self.buf)?;
                let b = std::mem::take(&mut self.buf);
                self.put_rgb(x, y, w, h, &b, 4);
                self.buf = b;
            }
            1 => {
                let sx = u16r(&mut self.r)? as usize;
                let sy = u16r(&mut self.r)? as usize;
                let mut tmp = vec![0u32; w * h];
                for yy in 0..h {
                    tmp[yy * w..(yy + 1) * w].copy_from_slice(&self.fb[(sy + yy) * self.fw + sx..(sy + yy) * self.fw + sx + w]);
                }
                for yy in 0..h {
                    self.fb[(y + yy) * self.fw + x..(y + yy) * self.fw + x + w].copy_from_slice(&tmp[yy * w..(yy + 1) * w]);
                }
            }
            2 => {
                let n = u32r(&mut self.r)?;
                let bg = u32::from_le_bytes(rd::<4>(&mut self.r)?) & 0xFF_FFFF;
                self.fill(x, y, w, h, bg);
                for _ in 0..n {
                    let c = u32::from_le_bytes(rd::<4>(&mut self.r)?) & 0xFF_FFFF;
                    let (sx, sy, sw, sh) = (u16r(&mut self.r)? as usize, u16r(&mut self.r)? as usize, u16r(&mut self.r)? as usize, u16r(&mut self.r)? as usize);
                    self.fill(x + sx, y + sy, sw, sh, c);
                }
            }
            5 => self.hextile(x, y, w, h)?,
            6 => {
                let len = u32r(&mut self.r)? as usize;
                self.buf.resize(len, 0);
                self.r.read_exact(&mut self.buf)?;
                let mut out = std::mem::take(&mut self.zbuf);
                self.zlib.inflate(&self.buf, &mut out, w * h * 4)?;
                self.put_rgb(x, y, w, h, &out, 4);
                self.zbuf = out;
            }
            7 => self.tight(x, y, w, h)?,
            16 => self.zrle(x, y, w, h)?,
            15 => self.trle(x, y, w, h)?,
            21 => {
                let data = read_jpeg(&mut self.r)?;
                let mut out = vec![0u8; w * h * 4];
                let img = turbojpeg::Image { pixels: &mut out[..], width: w, pitch: w * 4, height: h, format: turbojpeg::PixelFormat::BGRX };
                self.jpeg.decompress(&data, img).map_err(io::Error::other)?;
                self.put_rgb(x, y, w, h, &out, 4);
            }
            -239 => {
                let n = w * h * 4 + w.div_ceil(8) * h;
                io::copy(&mut (&mut self.r).take(n as u64), &mut io::sink())?;
            }
            -240 => {
                if w * h > 0 {
                    let n = 6 + 2 * w.div_ceil(8) * h;
                    io::copy(&mut (&mut self.r).take(n as u64), &mut io::sink())?;
                }
            }
            -314 => {
                let enc = u32r(&mut self.r)? as i32;
                if enc != 0 {
                    return Err(io::Error::other("alpha cursor non-raw"));
                }
                io::copy(&mut (&mut self.r).take((w * h * 4) as u64), &mut io::sink())?;
            }
            -232 => {}
            -223 => {
                self.fw = w;
                self.fh = h;
                self.fb = vec![0; w * h];
            }
            -308 => {
                let n = u8r(&mut self.r)? as usize;
                rd::<3>(&mut self.r)?;
                io::copy(&mut (&mut self.r).take((n * 16) as u64), &mut io::sink())?;
                if (w, h) != (self.fw, self.fh) {
                    self.fw = w;
                    self.fh = h;
                    self.fb = vec![0; w * h];
                }
            }
            _ => return Err(io::Error::other(format!("unsupported encoding {e}"))),
        }
        Ok(())
    }

    fn trle(&mut self, x: usize, y: usize, w: usize, h: usize) -> io::Result<()> {
        let mut pal: Vec<u32> = Vec::new();
        let cp = |r: &mut Counting<BufReader<TcpStream>>| -> io::Result<u32> {
            let b = rd::<3>(r)?;
            Ok((b[2] as u32) << 16 | (b[1] as u32) << 8 | b[0] as u32)
        };
        let rl = |r: &mut Counting<BufReader<TcpStream>>| -> io::Result<usize> {
            let mut n = 1;
            loop {
                let b = u8r(r)?;
                n += b as usize;
                if b != 255 {
                    return Ok(n);
                }
            }
        };
        for ty in (0..h).step_by(16) {
            for tx in (0..w).step_by(16) {
                let tw = 16.min(w - tx);
                let th = 16.min(h - ty);
                let (ox, oy) = (x + tx, y + ty);
                let sub = u8r(&mut self.r)?;
                match sub {
                    0 => {
                        for yy in 0..th {
                            for xx in 0..tw {
                                self.fb[(oy + yy) * self.fw + ox + xx] = cp(&mut self.r)?;
                            }
                        }
                    }
                    1 => {
                        let c = cp(&mut self.r)?;
                        self.fill(ox, oy, tw, th, c);
                    }
                    2..=16 | 127 => {
                        if sub != 127 {
                            pal = (0..sub).map(|_| cp(&mut self.r)).collect::<io::Result<_>>()?;
                        }
                        let n = pal.len();
                        let bits = if n <= 2 { 1 } else if n <= 4 { 2 } else { 4 };
                        for yy in 0..th {
                            let rb = (tw * bits).div_ceil(8);
                            let mut row = vec![0u8; rb];
                            self.r.read_exact(&mut row)?;
                            for xx in 0..tw {
                                let bp = xx * bits;
                                let v = (row[bp / 8] >> (8 - bits - bp % 8)) & ((1 << bits) - 1);
                                self.fb[(oy + yy) * self.fw + ox + xx] = pal[v as usize];
                            }
                        }
                    }
                    128 => {
                        let mut i = 0;
                        while i < tw * th {
                            let c = cp(&mut self.r)?;
                            let n = rl(&mut self.r)?;
                            for k in i..i + n {
                                self.fb[(oy + k / tw) * self.fw + ox + k % tw] = c;
                            }
                            i += n;
                        }
                    }
                    129..=255 => {
                        if sub != 129 {
                            pal = (0..sub - 128).map(|_| cp(&mut self.r)).collect::<io::Result<_>>()?;
                        }
                        let mut i = 0;
                        while i < tw * th {
                            let v = u8r(&mut self.r)?;
                            let cnt = if v & 0x80 != 0 { rl(&mut self.r)? } else { 1 };
                            let c = pal[(v & 0x7F) as usize];
                            for k in i..i + cnt {
                                self.fb[(oy + k / tw) * self.fw + ox + k % tw] = c;
                            }
                            i += cnt;
                        }
                    }
                    _ => return Err(io::Error::other(format!("bad trle sub {sub}"))),
                }
            }
        }
        Ok(())
    }

    fn hextile(&mut self, x: usize, y: usize, w: usize, h: usize) -> io::Result<()> {
        let (mut bg, mut fg) = (0u32, 0u32);
        for ty in (0..h).step_by(16) {
            for tx in (0..w).step_by(16) {
                let tw = 16.min(w - tx);
                let th = 16.min(h - ty);
                let m = u8r(&mut self.r)?;
                if m & 1 != 0 {
                    self.buf.resize(tw * th * 4, 0);
                    self.r.read_exact(&mut self.buf)?;
                    let b = std::mem::take(&mut self.buf);
                    self.put_rgb(x + tx, y + ty, tw, th, &b, 4);
                    self.buf = b;
                    continue;
                }
                if m & 2 != 0 {
                    bg = u32::from_le_bytes(rd::<4>(&mut self.r)?) & 0xFF_FFFF;
                }
                self.fill(x + tx, y + ty, tw, th, bg);
                if m & 4 != 0 {
                    fg = u32::from_le_bytes(rd::<4>(&mut self.r)?) & 0xFF_FFFF;
                }
                if m & 8 != 0 {
                    let n = u8r(&mut self.r)?;
                    for _ in 0..n {
                        let c = if m & 16 != 0 { u32::from_le_bytes(rd::<4>(&mut self.r)?) & 0xFF_FFFF } else { fg };
                        let xy = u8r(&mut self.r)? as usize;
                        let wh = u8r(&mut self.r)? as usize;
                        self.fill(x + tx + (xy >> 4), y + ty + (xy & 15), (wh >> 4) + 1, (wh & 15) + 1, c);
                    }
                }
            }
        }
        Ok(())
    }

    fn tight(&mut self, x: usize, y: usize, w: usize, h: usize) -> io::Result<()> {
        let ctl = u8r(&mut self.r)?;
        for i in 0..4 {
            if ctl & (1 << i) != 0 {
                self.tight[i] = Inflater::new();
            }
        }
        let comp = ctl >> 4;
        let tpix = |b: &[u8]| (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        if comp == 8 {
            let c = tpix(&rd::<3>(&mut self.r)?);
            self.fill(x, y, w, h, c);
            return Ok(());
        }
        if comp == 9 {
            let len = compact(&mut self.r)?;
            self.buf.resize(len, 0);
            self.r.read_exact(&mut self.buf)?;
            let hdr = self.jpeg.read_header(&self.buf).map_err(io::Error::other)?;
            if hdr.width != w || hdr.height != h {
                return Err(io::Error::other("jpeg size mismatch"));
            }
            let mut out = vec![0u8; w * h * 4];
            let img = turbojpeg::Image { pixels: &mut out[..], width: w, pitch: w * 4, height: h, format: turbojpeg::PixelFormat::BGRX };
            self.jpeg.decompress(&self.buf, img).map_err(io::Error::other)?;
            self.put_rgb(x, y, w, h, &out, 4);
            return Ok(());
        }
        if comp & 8 != 0 {
            return Err(io::Error::other(format!("bad tight comp {comp}")));
        }
        let stream = (comp & 3) as usize;
        let filter = if comp & 4 != 0 { u8r(&mut self.r)? } else { 0 };
        let mut palette = Vec::new();
        let size = match filter {
            0 => w * h * 3,
            1 => {
                let n = u8r(&mut self.r)? as usize + 1;
                for _ in 0..n {
                    palette.push(tpix(&rd::<3>(&mut self.r)?));
                }
                if n == 2 { w.div_ceil(8) * h } else { w * h }
            }
            2 => w * h * 3,
            _ => return Err(io::Error::other("bad tight filter")),
        };
        let data = if size < 12 {
            let mut d = vec![0u8; size];
            self.r.read_exact(&mut d)?;
            d
        } else {
            let len = compact(&mut self.r)?;
            self.buf.resize(len, 0);
            self.r.read_exact(&mut self.buf)?;
            let mut out = Vec::new();
            self.tight[stream].inflate(&self.buf, &mut out, size)?;
            if out.len() != size {
                return Err(io::Error::other(format!("tight inflate size {} != {}", out.len(), size)));
            }
            out
        };
        match filter {
            0 => self.put_rgb(x, y, w, h, &data, 3),
            1 if palette.len() == 2 => {
                let rb = w.div_ceil(8);
                for yy in 0..h {
                    for xx in 0..w {
                        let bit = data[yy * rb + xx / 8] >> (7 - (xx & 7)) & 1;
                        self.fb[(y + yy) * self.fw + x + xx] = palette[bit as usize];
                    }
                }
            }
            1 => {
                for yy in 0..h {
                    for xx in 0..w {
                        self.fb[(y + yy) * self.fw + x + xx] = palette[data[yy * w + xx] as usize];
                    }
                }
            }
            _ => {
                let mut prev = vec![0i32; w * 3];
                let mut cur = vec![0i32; w * 3];
                for yy in 0..h {
                    for xx in 0..w {
                        for c in 0..3 {
                            let left = if xx > 0 { cur[(xx - 1) * 3 + c] } else { 0 };
                            let up = prev[xx * 3 + c];
                            let ul = if xx > 0 { prev[(xx - 1) * 3 + c] } else { 0 };
                            let p = (left + up - ul).clamp(0, 255);
                            cur[xx * 3 + c] = (p + data[(yy * w + xx) * 3 + c] as i32) & 255;
                        }
                        self.fb[(y + yy) * self.fw + x + xx] = (cur[xx * 3] as u32) << 16 | (cur[xx * 3 + 1] as u32) << 8 | cur[xx * 3 + 2] as u32;
                    }
                    std::mem::swap(&mut prev, &mut cur);
                }
            }
        }
        Ok(())
    }

    fn zrle(&mut self, x: usize, y: usize, w: usize, h: usize) -> io::Result<()> {
        let len = u32r(&mut self.r)? as usize;
        self.buf.resize(len, 0);
        self.r.read_exact(&mut self.buf)?;
        let mut d = std::mem::take(&mut self.zbuf);
        self.zrle.inflate(&self.buf, &mut d, w * h * 3)?;
        let mut p = 0usize;
        let cp = |d: &[u8], p: &mut usize| -> u32 {
            let v = (d[*p + 2] as u32) << 16 | (d[*p + 1] as u32) << 8 | d[*p] as u32;
            *p += 3;
            v
        };
        let rl = |d: &[u8], p: &mut usize| -> usize {
            let mut n = 1;
            loop {
                let b = d[*p];
                *p += 1;
                n += b as usize;
                if b != 255 {
                    return n;
                }
            }
        };
        for ty in (0..h).step_by(64) {
            for tx in (0..w).step_by(64) {
                let tw = 64.min(w - tx);
                let th = 64.min(h - ty);
                let sub = d[p];
                p += 1;
                let (ox, oy) = (x + tx, y + ty);
                match sub {
                    0 => {
                        for yy in 0..th {
                            for xx in 0..tw {
                                self.fb[(oy + yy) * self.fw + ox + xx] = cp(&d, &mut p);
                            }
                        }
                    }
                    1 => {
                        let c = cp(&d, &mut p);
                        self.fill(ox, oy, tw, th, c);
                    }
                    2..=16 => {
                        let n = sub as usize;
                        let pal: Vec<u32> = (0..n).map(|_| cp(&d, &mut p)).collect();
                        let bits = if n <= 2 { 1 } else if n <= 4 { 2 } else { 4 };
                        for yy in 0..th {
                            let mut bitpos = 0;
                            for xx in 0..tw {
                                let byte = d[p + bitpos / 8];
                                let v = (byte >> (8 - bits - bitpos % 8)) & ((1 << bits) - 1);
                                self.fb[(oy + yy) * self.fw + ox + xx] = pal[v as usize];
                                bitpos += bits;
                            }
                            p += (tw * bits).div_ceil(8);
                        }
                    }
                    128 => {
                        let mut i = 0;
                        while i < tw * th {
                            let c = cp(&d, &mut p);
                            let n = rl(&d, &mut p);
                            for k in i..i + n {
                                self.fb[(oy + k / tw) * self.fw + ox + k % tw] = c;
                            }
                            i += n;
                        }
                    }
                    130..=255 => {
                        let n = sub as usize - 128;
                        let pal: Vec<u32> = (0..n).map(|_| cp(&d, &mut p)).collect();
                        let mut i = 0;
                        while i < tw * th {
                            let v = d[p];
                            p += 1;
                            let cnt = if v & 0x80 != 0 { rl(&d, &mut p) } else { 1 };
                            let c = pal[(v & 0x7F) as usize];
                            for k in i..i + cnt {
                                self.fb[(oy + k / tw) * self.fw + ox + k % tw] = c;
                            }
                            i += cnt;
                        }
                    }
                    _ => return Err(io::Error::other(format!("bad zrle sub {sub}"))),
                }
            }
        }
        self.zbuf = d;
        Ok(())
    }

    fn read_bar(&self, bx: i32, by: i32) -> Option<u32> {
        let mut v = 0u32;
        for b in 0..32 {
            let px = bx + b * CELL + CELL / 2;
            let py = by + CELL / 2;
            if px < 0 || py < 0 || px as usize >= self.fw || py as usize >= self.fh {
                return None;
            }
            let p = self.fb[py as usize * self.fw + px as usize];
            let g = (p >> 8) & 255;
            if g > 128 {
                v |= 1 << b;
            }
        }
        code_ok(v)
    }
}

/// Reads one JPEG image (SOI..EOI) from the stream by walking its markers.
fn read_jpeg(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut d = Vec::with_capacity(64 * 1024);
    let soi = rd::<2>(r)?;
    if soi != [0xFF, 0xD8] {
        return Err(io::Error::other("jpeg: no SOI"));
    }
    d.extend_from_slice(&soi);
    let mut pending: Option<u8> = None;
    loop {
        // Next marker.
        let mut b = match pending.take() { Some(b) => b, None => u8r(r)? };
        if b != 0xFF {
            return Err(io::Error::other("jpeg: expected marker"));
        }
        while b == 0xFF {
            b = u8r(r)?;
        }
        d.extend_from_slice(&[0xFF, b]);
        match b {
            0xD9 => return Ok(d),
            0x01 | 0xD0..=0xD7 => continue,
            _ => {}
        }
        let lb = rd::<2>(r)?;
        d.extend_from_slice(&lb);
        let len = u16::from_be_bytes(lb) as usize;
        let mut seg = vec![0u8; len.saturating_sub(2)];
        r.read_exact(&mut seg)?;
        d.extend_from_slice(&seg);
        if b == 0xDA {
            // Entropy-coded data until a real marker.
            loop {
                let c = u8r(r)?;
                if c != 0xFF {
                    d.push(c);
                    continue;
                }
                let n = u8r(r)?;
                if n == 0x00 || (0xD0..=0xD7).contains(&n) {
                    d.extend_from_slice(&[0xFF, n]);
                    continue;
                }
                if n == 0xD9 {
                    d.extend_from_slice(&[0xFF, 0xD9]);
                    return Ok(d);
                }
                // Another segment after the scan (e.g. progressive): copy it inline.
                d.extend_from_slice(&[0xFF, n]);
                let lb = rd::<2>(r)?;
                d.extend_from_slice(&lb);
                let len = u16::from_be_bytes(lb) as usize;
                let mut seg = vec![0u8; len.saturating_sub(2)];
                r.read_exact(&mut seg)?;
                d.extend_from_slice(&seg);
                if n != 0xDA {
                    break;
                }
            }
        }
    }
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn proc_cpu(pid: u32) -> Option<f64> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut c, mut e, mut k, mut u) = Default::default();
        let r = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        let _ = CloseHandle(h);
        r.ok()?;
        let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
        Some(t(k) + t(u))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str, d: &str| -> String { args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned().unwrap_or(d.to_string()) };
    let addr = get("--addr", "127.0.0.1:5905");
    let pw = args.iter().position(|a| a == "--password").and_then(|i| args.get(i + 1)).cloned();
    let profile = Profile::parse(&get("--profile", "tightvnc"));
    let secs: f64 = get("--secs", "10").parse().unwrap();
    let origin: (i32, i32) = { let o = get("--origin", "0,0"); let mut it = o.split(',').map(|v| v.parse::<i32>().unwrap()); (it.next().unwrap(), it.next().unwrap()) };
    let pid: u32 = get("--pid", "0").parse().unwrap();
    let label = get("--label", "");
    let use_cu = args.iter().any(|a| a == "--cu") || profile == Profile::TigerVnc;
    let dump = args.iter().position(|a| a == "--dump").and_then(|i| args.get(i + 1)).cloned();
    let verify = args.iter().any(|a| a == "--verify");

    let (shm, final_px) = unsafe {
        let m = OpenFileMappingW(FILE_MAP_READ.0, false, w!(r"Local\PraeterTestApp"));
        match m.ok() {
            Some(m) => {
                let v = MapViewOfFile(m, FILE_MAP_READ, 0, 0, 0).Value as *const u8;
                (Some(&*(v as *const Shm)), Some(v.add(std::mem::size_of::<Shm>()) as *const u32))
            }
            None => (None, None),
        }
    };
    let mut c = Client::connect(&addr, pw.as_deref()).unwrap_or_else(|e| {
        eprintln!("connect: {e}");
        std::process::exit(1);
    });
    let encs: Vec<i32> = match args.iter().position(|a| a == "--encodings").and_then(|i| args.get(i + 1)) {
        Some(e) => e.split(',').map(|v| v.trim().parse().unwrap()).collect(),
        None => profile.encodings(),
    };
    c.set_encodings(&encs).unwrap();
    c.request(false).unwrap();

    let freq = qpf();
    let mut lat: Vec<f64> = Vec::new();
    let mut recv: Vec<(u32, u64)> = Vec::new();
    let mut last_id = 0u32;
    let mut first_id = None;
    let mut updates = 0u64;
    let mut rects = 0u64;
    let mut decode_s = 0.0f64;
    let mut arrivals: Vec<f64> = Vec::new();
    let mut warm = true;
    let mut first_upd: Option<f64> = None;
    let t_start = Instant::now();
    let mut t_meas = Instant::now();
    let mut bytes0 = 0;
    let mut cpu0 = None;
    let mut app_frames0 = 0u64;
    let mut dead = 0u32;
    loop {
        let m = match c.read_msg() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: {e}");
                break;
            }
        };
        let Some(nr) = m else { continue };
        let now = qpc();
        let dt = c.t_first.elapsed().as_secs_f64();
        if !c.cu_enabled {
            if use_cu {
                c.enable_cu().unwrap();
            }
            c.request(true).unwrap();
        }
        let first_ms = *first_upd.get_or_insert(t_start.elapsed().as_secs_f64() * 1e3);
        if warm && t_start.elapsed().as_secs_f64() * 1e3 > first_ms + 1500.0 {
            warm = false;
            t_meas = Instant::now();
            bytes0 = c.r.n;
            cpu0 = proc_cpu(pid);
            app_frames0 = shm.map(|s| s.frames).unwrap_or(0);
            lat.clear();
            recv.clear();
            arrivals.clear();
            updates = 0;
            rects = 0;
            decode_s = 0.0;
            first_id = None;
        }
        updates += 1;
        rects += nr as u64;
        decode_s += dt;
        if let Some(s) = shm.filter(|s| s.magic == MAGIC) {
            if let Some(id) = c.read_bar(s.bar_x - origin.0, s.bar_y - origin.1) {
                if id != last_id && id as u64 <= s.frames {
                    if !warm {
                        recv.push((id, now));
                        arrivals.push(now as f64 / freq * 1e3);
                        first_id.get_or_insert(id);
                    }
                    last_id = id;
                    dead = 0;
                }
            } else {
                dead += 1;
            }
        }
        if !warm && t_meas.elapsed().as_secs_f64() >= secs {
            break;
        }
    }
    let el = t_meas.elapsed().as_secs_f64();
    let app_frames_end = shm.map(|s| s.frames).unwrap_or(0);
    // Resolve display timestamps that landed after receipt.
    std::thread::sleep(Duration::from_millis(60));
    let mut unresolved = 0;
    if let Some(s) = shm {
        for &(id, t) in &recv {
            let ts = s.ts[(id as usize) & 0xFFFF];
            if ts != 0 && ts < t + (freq as u64) {
                lat.push((t as f64 - ts as f64) / freq * 1e3);
            } else {
                unresolved += 1;
            }
        }
    }
    let mut verify_msg = String::new();
    if verify {
        let s = shm.expect("verify needs test app");
        let t = Instant::now();
        while s.final_ready == 0 && t.elapsed() < Duration::from_secs(20) {
            if let Ok(Some(_)) = c.read_msg() {
                if !c.cu_enabled {
                    let _ = c.request(true);
                }
            }
        }
        // Allow lossless refresh to land.
        c.r.r.get_ref().set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(1500) {
            match c.read_msg() {
                Ok(Some(_)) if !c.cu_enabled => {
                    let _ = c.request(true);
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    verify_msg = format!(" | verify read error {e}");
                    break;
                }
                _ => {}
            }
        }
        let [wx, wy, ww, wh] = s.win;
        let fp = unsafe { std::slice::from_raw_parts(final_px.unwrap(), (ww * wh) as usize) };
        let (mut bad, mut maxd) = (0u64, 0i32);
        for y in 0..wh {
            for x in 0..ww {
                let a = c.fb[((wy - origin.1 + y) as usize) * c.fw + (wx - origin.0 + x) as usize];
                let b = fp[(y * ww + x) as usize] & 0xFF_FFFF;
                if a != b {
                    bad += 1;
                    for sh in [0, 8, 16] {
                        maxd = maxd.max(((a >> sh & 255) as i32 - (b >> sh & 255) as i32).abs());
                    }
                }
            }
        }
        verify_msg += &format!(" | verify: {} bad px (max diff {})", bad, maxd);
    }
    let bytes = c.r.n - bytes0;
    let cpu = match (cpu0, proc_cpu(pid)) {
        (Some(a), Some(b)) => Some((b - a) / el * 100.0),
        _ => None,
    };
    let app_frames = app_frames_end - app_frames0;
    // Frames composed within 4 ms of the previous one likely shared a vsync.
    let mut merged = 0u64;
    if let Some(s) = shm {
        for id in app_frames0 + 1..=app_frames_end {
            let (a, b) = (s.ts[((id - 1) & 0xFFFF) as usize], s.ts[(id & 0xFFFF) as usize]);
            if a != 0 && b > a && ((b - a) as f64 / freq * 1e3) < 4.0 {
                merged += 1;
            }
        }
    }
    let mut gaps: Vec<f64> = arrivals.windows(2).map(|w| w[1] - w[0]).collect();
    let mean_gap = gaps.iter().sum::<f64>() / gaps.len().max(1) as f64;
    let jitter = (gaps.iter().map(|g| (g - mean_gap).powi(2)).sum::<f64>() / gaps.len().max(1) as f64).sqrt();
    let seen = recv.len();
    if unresolved > seen / 20 {
        eprintln!("warning: {unresolved} frames without display timestamp");
    }
    let mut l2 = lat.clone();
    println!(
        "{label:14} upd/s {:7.1} | frames seen {:5}/{:5} ({:5.1}%, src-merged {}) | lat ms p50 {:6.1} p90 {:6.1} p99 {:6.1} max {:6.1} | gap p90 {:5.1} jit {:5.1} | {:7.2} MB/s | rects/upd {:6.1} | dec {:5.2} ms/upd | paint {:6.0} ms | srv cpu {}",
        updates as f64 / el,
        seen, app_frames, 100.0 * seen as f64 / app_frames.max(1) as f64, merged,
        pct(&mut l2, 0.5), pct(&mut l2, 0.9), pct(&mut l2, 0.99), pct(&mut l2, 1.0),
        pct(&mut gaps, 0.9), jitter,
        bytes as f64 / el / 1e6,
        rects as f64 / updates.max(1) as f64,
        decode_s * 1e3 / updates.max(1) as f64,
        c.painted.map_or(-1.0, |p| p.duration_since(t_start).as_secs_f64() * 1e3),
        cpu.map(|c| format!("{c:5.1}%")).unwrap_or("n/a".into()),
    );
    if verify {
        println!("{label:14}{verify_msg}");
    }
    if dead > 50 && seen == 0 {
        eprintln!("warning: barcode never decoded (check origin/position)");
    }
    if let Some(p) = dump {
        let mut o = Vec::with_capacity(c.fb.len() * 4);
        for &v in &c.fb {
            o.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(&p, &o).unwrap();
        eprintln!("dumped {}x{} to {p}", c.fw, c.fh);
    }
}
