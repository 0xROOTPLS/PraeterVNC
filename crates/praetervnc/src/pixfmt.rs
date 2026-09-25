//! RFB pixel formats. Native framebuffer pixels are u32 0x00RRGGBB.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PixelFormat {
    pub bpp: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_color: bool,
    pub rmax: u16,
    pub gmax: u16,
    pub bmax: u16,
    pub rshift: u8,
    pub gshift: u8,
    pub bshift: u8,
}

impl PixelFormat {
    pub const NATIVE: PixelFormat = PixelFormat {
        bpp: 32, depth: 24, big_endian: false, true_color: true,
        rmax: 255, gmax: 255, bmax: 255, rshift: 16, gshift: 8, bshift: 0,
    };

    /// BGR233, used when a client asks for a colour map.
    pub const BGR233: PixelFormat = PixelFormat {
        bpp: 8, depth: 8, big_endian: false, true_color: true,
        rmax: 7, gmax: 7, bmax: 3, rshift: 0, gshift: 3, bshift: 6,
    };

    pub fn parse(b: &[u8]) -> PixelFormat {
        PixelFormat {
            bpp: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_color: b[3] != 0,
            rmax: u16::from_be_bytes([b[4], b[5]]),
            gmax: u16::from_be_bytes([b[6], b[7]]),
            bmax: u16::from_be_bytes([b[8], b[9]]),
            rshift: b[10],
            gshift: b[11],
            bshift: b[12],
        }
    }

    pub fn write(&self, o: &mut Vec<u8>) {
        o.extend_from_slice(&[self.bpp, self.depth, self.big_endian as u8, self.true_color as u8]);
        o.extend_from_slice(&self.rmax.to_be_bytes());
        o.extend_from_slice(&self.gmax.to_be_bytes());
        o.extend_from_slice(&self.bmax.to_be_bytes());
        o.extend_from_slice(&[self.rshift, self.gshift, self.bshift, 0, 0, 0]);
    }

    pub fn valid(&self) -> bool {
        matches!(self.bpp, 8 | 16 | 32)
            && (!self.true_color
                || (self.rmax > 0 && self.gmax > 0 && self.bmax > 0 && self.rshift < 32 && self.gshift < 32 && self.bshift < 32))
    }

    #[inline]
    pub fn bytes_pp(&self) -> usize {
        self.bpp as usize / 8
    }

    pub fn is_native(&self) -> bool {
        self.bpp == 32 && self.true_color && !self.big_endian
            && self.rmax == 255 && self.gmax == 255 && self.bmax == 255
            && self.rshift == 16 && self.gshift == 8 && self.bshift == 0
    }

    /// Tight TPIXEL: 3 bytes R,G,B.
    pub fn tight_24(&self) -> bool {
        self.bpp == 32 && self.depth == 24 && self.true_color
            && self.rmax == 255 && self.gmax == 255 && self.bmax == 255
    }
}

/// Precomputed native -> client conversion.
#[derive(Clone)]
pub struct Converter {
    pub pf: PixelFormat,
    tr: Box<[u32; 256]>,
    tg: Box<[u32; 256]>,
    tb: Box<[u32; 256]>,
    /// ZRLE CPIXEL byte count and offset (in client byte order).
    pub cpixel_len: usize,
    cpixel_off: usize,
}

impl Converter {
    pub fn new(pf: PixelFormat) -> Converter {
        let mk = |max: u16, shift: u8| {
            let mut t = Box::new([0u32; 256]);
            for (i, v) in t.iter_mut().enumerate() {
                *v = (((i as u32 * max as u32) + 127) / 255) << shift;
            }
            t
        };
        let (mut cpixel_len, mut cpixel_off) = (pf.bytes_pp(), 0);
        if pf.bpp == 32 && pf.true_color && pf.depth <= 24 {
            let mask = ((pf.rmax as u32) << pf.rshift) | ((pf.gmax as u32) << pf.gshift) | ((pf.bmax as u32) << pf.bshift);
            if mask & 0xFF00_0000 == 0 {
                cpixel_len = 3;
                cpixel_off = if pf.big_endian { 1 } else { 0 };
            } else if mask & 0x0000_00FF == 0 {
                cpixel_len = 3;
                cpixel_off = if pf.big_endian { 0 } else { 1 };
            }
        }
        Converter { pf, tr: mk(pf.rmax, pf.rshift), tg: mk(pf.gmax, pf.gshift), tb: mk(pf.bmax, pf.bshift), cpixel_len, cpixel_off }
    }

    #[inline(always)]
    pub fn value(&self, p: u32) -> u32 {
        self.tr[((p >> 16) & 255) as usize] | self.tg[((p >> 8) & 255) as usize] | self.tb[(p & 255) as usize]
    }

    #[inline(always)]
    pub fn put(&self, p: u32, o: &mut Vec<u8>) {
        let v = self.value(p);
        match (self.pf.bpp, self.pf.big_endian) {
            (32, false) => o.extend_from_slice(&v.to_le_bytes()),
            (32, true) => o.extend_from_slice(&v.to_be_bytes()),
            (16, false) => o.extend_from_slice(&(v as u16).to_le_bytes()),
            (16, true) => o.extend_from_slice(&(v as u16).to_be_bytes()),
            _ => o.push(v as u8),
        }
    }

    #[inline(always)]
    pub fn put_cpixel(&self, p: u32, o: &mut Vec<u8>) {
        if self.cpixel_len == 3 {
            let v = self.value(p);
            let b = if self.pf.big_endian { v.to_be_bytes() } else { v.to_le_bytes() };
            o.extend_from_slice(&b[self.cpixel_off..self.cpixel_off + 3]);
        } else {
            self.put(p, o);
        }
    }

    /// Bulk convert to client pixels.
    pub fn put_slice(&self, px: &[u32], o: &mut Vec<u8>) {
        if self.pf.is_native() {
            let b = unsafe { std::slice::from_raw_parts(px.as_ptr() as *const u8, px.len() * 4) };
            o.extend_from_slice(b);
            return;
        }
        o.reserve(px.len() * self.pf.bytes_pp());
        for &p in px {
            self.put(p, o);
        }
    }
}

/// Native pixels to packed R,G,B (Tight TPIXEL / JPEG-free full colour).
pub fn put_rgb24(px: &[u32], o: &mut Vec<u8>) {
    let start = o.len();
    o.resize(start + px.len() * 3, 0);
    let dst = &mut o[start..];
    for (d, &p) in dst.chunks_exact_mut(3).zip(px) {
        d[0] = (p >> 16) as u8;
        d[1] = (p >> 8) as u8;
        d[2] = p as u8;
    }
}
