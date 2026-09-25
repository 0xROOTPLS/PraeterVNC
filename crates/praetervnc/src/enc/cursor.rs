use super::*;
use crate::fb::CursorShape;

/// RichCursor (-239): client-format pixels + 1bpp mask.
pub fn rich(cx: &Ctx, c: &CursorShape, o: &mut Vec<u8>) {
    put_rect_header(o, c.hot_x, c.hot_y, c.w, c.h, crate::rfb::enc::CURSOR);
    let (w, h) = (c.w as usize, c.h as usize);
    for &p in &c.argb {
        cx.conv.put(p & 0xFF_FFFF, o);
    }
    let rb = w.div_ceil(8);
    for y in 0..h {
        let mut row = vec![0u8; rb];
        for x in 0..w {
            if c.argb[y * w + x] >> 24 >= 0x80 {
                row[x >> 3] |= 0x80 >> (x & 7);
            }
        }
        o.extend_from_slice(&row);
    }
}

/// CursorWithAlpha (-314): u32 encoding (Raw) then premultiplied RGBA.
pub fn alpha(c: &CursorShape, o: &mut Vec<u8>) {
    put_rect_header(o, c.hot_x, c.hot_y, c.w, c.h, crate::rfb::enc::CURSOR_ALPHA);
    o.extend_from_slice(&crate::rfb::enc::RAW.to_be_bytes());
    for &p in &c.argb {
        let a = p >> 24;
        let pm = |v: u32| ((v * a + 127) / 255) as u8;
        o.extend_from_slice(&[pm((p >> 16) & 255), pm((p >> 8) & 255), pm(p & 255), a as u8]);
    }
}

pub fn empty(o: &mut Vec<u8>) {
    put_rect_header(o, 0, 0, 0, 0, crate::rfb::enc::CURSOR);
}
