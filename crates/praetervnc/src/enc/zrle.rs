//! ZRLE/TRLE tile coding (ZRLE: 64px tiles, zlib applied afterwards; TRLE: 16px, raw).
use super::*;

fn put_len(o: &mut Vec<u8>, mut n: usize) {
    n -= 1;
    while n >= 255 {
        o.push(255);
        n -= 255;
    }
    o.push(n as u8);
}

/// Encodes all `t`x`t` tiles of `r` (tiles relative to rect origin) into `o`.
pub fn encode_rect(cx: &Ctx, r: Rect, px: &[u32], pal: &mut Palette, t: usize, o: &mut Vec<u8>) {
    let (w, h) = (r.w as usize, r.h as usize);
    let cp = cx.conv.cpixel_len;
    let mut tile = Vec::with_capacity(t * t);
    for ty in (0..h).step_by(t) {
        for tx in (0..w).step_by(t) {
            let tw = t.min(w - tx);
            let th = t.min(h - ty);
            tile.clear();
            for y in ty..ty + th {
                tile.extend_from_slice(&px[y * w + tx..y * w + tx + tw]);
            }
            encode_tile(cx, &tile, tw, th, cp, pal, o);
        }
    }
}

fn encode_tile(cx: &Ctx, px: &[u32], w: usize, h: usize, cp: usize, pal: &mut Palette, o: &mut Vec<u8>) {
    let n = px.len();
    if crate::simd::all_eq(px, px[0]) {
        o.push(1);
        cx.conv.put_cpixel(px[0], o);
        return;
    }
    let mut runs = 1usize;
    let mut single = 0usize;
    let mut rl = 1usize;
    for i in 1..n {
        if px[i] != px[i - 1] {
            runs += 1;
            if rl == 1 {
                single += 1;
            }
            rl = 1;
        } else {
            rl += 1;
        }
    }
    if rl == 1 {
        single += 1;
    }
    let raw_sz = n * cp;
    let rle_sz = runs * (cp + 1);
    let nc = pal.build(px, 127);
    let mut best = (raw_sz, 0u8);
    if rle_sz < best.0 {
        best = (rle_sz, 128);
    }
    if let Some(nc) = nc {
        if nc <= 16 {
            let bits = if nc <= 2 { 1 } else if nc <= 4 { 2 } else { 4 };
            let sz = nc * cp + h * (w * bits).div_ceil(8);
            if sz < best.0 {
                best = (sz, nc as u8);
            }
        }
        let prle = nc * cp + (runs - single) * 2 + single;
        if prle < best.0 {
            best = (prle, 128 + nc as u8);
        }
    }
    let sub = best.1;
    o.push(sub);
    match sub {
        0 => {
            for &p in px {
                cx.conv.put_cpixel(p, o);
            }
        }
        128 => {
            let mut i = 0;
            while i < n {
                let c = px[i];
                let mut j = i + 1;
                while j < n && px[j] == c {
                    j += 1;
                }
                cx.conv.put_cpixel(c, o);
                put_len(o, j - i);
                i = j;
            }
        }
        2..=16 => {
            for &c in &pal.colors {
                cx.conv.put_cpixel(c, o);
            }
            let nc = sub as usize;
            let bits = if nc <= 2 { 1 } else if nc <= 4 { 2 } else { 4 };
            let ppb = 8 / bits;
            for row in pal.idx.chunks_exact(w) {
                for ch in row.chunks(ppb) {
                    let mut b = 0u8;
                    for (k, &v) in ch.iter().enumerate() {
                        b |= v << (8 - bits * (k + 1));
                    }
                    o.push(b);
                }
            }
        }
        _ => {
            for &c in &pal.colors {
                cx.conv.put_cpixel(c, o);
            }
            let idx = &pal.idx;
            let mut i = 0;
            while i < n {
                let v = idx[i];
                let mut j = i + 1;
                while j < n && idx[j] == v {
                    j += 1;
                }
                if j - i == 1 {
                    o.push(v);
                } else {
                    o.push(v | 0x80);
                    put_len(o, j - i);
                }
                i = j;
            }
        }
    }
}
