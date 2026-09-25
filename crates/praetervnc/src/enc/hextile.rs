use super::*;

pub fn encode(cx: &Ctx, r: Rect, px: &[u32], o: &mut Vec<u8>) {
    put_rect_header(o, r.x, r.y, r.w, r.h, crate::rfb::enc::HEXTILE);
    let (w, h) = (r.w as usize, r.h as usize);
    let bpp = cx.conv.pf.bytes_pp();
    let mut bg: Option<u32> = None;
    let mut sub = Vec::with_capacity(512);
    for ty in (0..h).step_by(16) {
        for tx in (0..w).step_by(16) {
            let tw = 16.min(w - tx);
            let th = 16.min(h - ty);
            let at = |x: usize, y: usize| px[(ty + y) * w + tx + x];
            let b = at(0, 0);
            let solid = (0..th).all(|y| (0..tw).all(|x| at(x, y) == b));
            if solid {
                if bg == Some(b) {
                    o.push(0);
                } else {
                    o.push(2);
                    cx.conv.put(b, o);
                    bg = Some(b);
                }
                continue;
            }
            // Coloured 1-row subrects over the most common edge colour.
            sub.clear();
            let mut n = 0usize;
            let raw_sz = tw * th * bpp;
            let mut too_big = false;
            'rows: for y in 0..th {
                let mut x = 0;
                while x < tw {
                    let c = at(x, y);
                    let mut e = x + 1;
                    while e < tw && at(e, y) == c {
                        e += 1;
                    }
                    if c != b {
                        cx.conv.put(c, &mut sub);
                        sub.push(((x as u8) << 4) | y as u8);
                        sub.push((((e - x - 1) as u8) << 4) | 0);
                        n += 1;
                        if sub.len() + bpp + 2 >= raw_sz || n > 255 {
                            too_big = true;
                            break 'rows;
                        }
                    }
                    x = e;
                }
            }
            if too_big {
                o.push(1);
                for y in 0..th {
                    for x in 0..tw {
                        cx.conv.put(at(x, y), o);
                    }
                }
                bg = None;
                continue;
            }
            let mut mask = 8 | 16;
            if bg != Some(b) {
                mask |= 2;
            }
            o.push(mask);
            if mask & 2 != 0 {
                cx.conv.put(b, o);
                bg = Some(b);
            }
            o.push(n as u8);
            o.extend_from_slice(&sub);
        }
    }
}
