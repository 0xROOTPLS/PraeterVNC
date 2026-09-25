//! Tight encoding stage 1 (analysis/filtering); zlib is applied per stream afterwards.
use super::*;
use crate::pixfmt::put_rgb24;

pub const MAX_WIDTH: u32 = 2048;

fn put_tpixel(cx: &Ctx, p: u32, o: &mut Vec<u8>) {
    if cx.conv.pf.tight_24() {
        o.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
    } else {
        cx.conv.put(p, o);
    }
}

fn put_tpixels(cx: &Ctx, px: &[u32], o: &mut Vec<u8>) {
    if cx.conv.pf.tight_24() {
        put_rgb24(px, o);
    } else {
        cx.conv.put_slice(px, o);
    }
}

/// Photographic content: many neighbour pairs differ slightly (<= 32 per channel).
/// UI/text is mostly exact repeats plus hard edges (measured: <10% vs >=30% for photos).
pub fn photo_like(px: &[u32], w: usize) -> bool {
    let h = px.len() / w;
    let (mut near, mut n) = (0usize, 0usize);
    for y in (0..h).step_by(2) {
        let row = &px[y * w..(y + 1) * w];
        for x in 1..w {
            let (a, b) = (row[x], row[x - 1]);
            if a != b {
                let d = (0..3).map(|c| ((a >> (c * 8)) as u8).abs_diff((b >> (c * 8)) as u8)).max().unwrap();
                near += (d <= 32) as usize;
            }
        }
        n += w - 1;
    }
    near * 5 >= n
}

const SUB: usize = 64;

/// Encodes `r` into one or more Tight rects; colourful non-photo rects become 64x64 palette blocks.
/// `stream` rotates over 0..=3.
pub fn encode_into(cx: &Ctx, r: Rect, px: &[u32], pal: &mut Palette, stream: &mut usize, out: &mut Vec<(Rect, Part, bool)>) {
    let (w, h) = (r.w as usize, r.h as usize);
    if w * h > SUB * SUB && !crate::simd::all_eq(px, px[0]) && pal.build(px, 256).is_none() {
        let jpeg_ok = cx.jpeg.is_some() && cx.conv.pf.tight_24() && px.len() >= cx.jpeg_min_area && !cx.force_lossless;
        if !(jpeg_ok && photo_like(px, w)) {
            let mut blk = Vec::with_capacity(SUB * SUB);
            for by in (0..h).step_by(SUB) {
                for bx in (0..w).step_by(SUB) {
                    let (bw, bh) = (SUB.min(w - bx), SUB.min(h - by));
                    blk.clear();
                    for y in by..by + bh {
                        blk.extend_from_slice(&px[y * w + bx..y * w + bx + bw]);
                    }
                    let br = Rect::new(r.x as u32 + bx as u32, r.y as u32 + by as u32, bw as u32, bh as u32);
                    let p = encode(cx, br, &blk, pal, *stream);
                    *stream = (*stream + 1) & 3;
                    out.push((br, p, false));
                }
            }
            return;
        }
    }
    let p = encode(cx, r, px, pal, *stream);
    *stream = (*stream + 1) & 3;
    let lossy = matches!(&p, Part::Done(d) if d.len() > 12 && d[12] == 0x90);
    out.push((r, p, lossy));
}

/// `stream`: zlib stream for basic compression (0..=3).
pub fn encode(cx: &Ctx, r: Rect, px: &[u32], pal: &mut Palette, stream: usize) -> Part {
    let mut hdr = Vec::with_capacity(32);
    put_rect_header(&mut hdr, r.x, r.y, r.w, r.h, crate::rfb::enc::TIGHT);
    let area = px.len();
    if crate::simd::all_eq(px, px[0]) {
        hdr.push(0x80);
        put_tpixel(cx, px[0], &mut hdr);
        return Part::Done(hdr);
    }
    let max = if area >= 1024 { 256 } else { (area / 4).max(2) };
    let n = pal.build(px, max);
    let sbits = (stream as u8) << 4;
    match n {
        Some(2) => {
            hdr.extend_from_slice(&[0x40 | sbits, 1, 1]);
            put_tpixel(cx, pal.colors[0], &mut hdr);
            put_tpixel(cx, pal.colors[1], &mut hdr);
            let w = r.w as usize;
            let rb = w.div_ceil(8);
            let mut bits = vec![0u8; rb * r.h as usize];
            for (y, row) in pal.idx.chunks_exact(w).enumerate() {
                let o = &mut bits[y * rb..(y + 1) * rb];
                for (x, &v) in row.iter().enumerate() {
                    o[x >> 3] |= v << (7 - (x & 7));
                }
            }
            Part::Z { hdr, stream, raw: bits }
        }
        Some(nc) => {
            hdr.extend_from_slice(&[0x40 | sbits, 1, (nc - 1) as u8]);
            for &c in &pal.colors {
                put_tpixel(cx, c, &mut hdr);
            }
            Part::Z { hdr, stream, raw: std::mem::take(&mut pal.idx) }
        }
        None => {
            let jpeg_ok = cx.jpeg.is_some() && cx.conv.pf.tight_24() && area >= cx.jpeg_min_area && !cx.force_lossless;
            if jpeg_ok && photo_like(px, r.w as usize) {
                let (q, ss) = cx.jpeg.unwrap();
                hdr.push(0x90);
                let j = super::jpeg::compress(px, r.w as usize, r.h as usize, q, ss);
                put_compact_len(&mut hdr, j.len());
                hdr.extend_from_slice(&j);
                return Part::Done(hdr);
            }
            hdr.push(sbits);
            let mut raw = Vec::with_capacity(area * 3);
            put_tpixels(cx, px, &mut raw);
            Part::Z { hdr, stream, raw }
        }
    }
}

/// Appends zlib payload per Tight rules (raw if < 12 bytes).
pub fn finish(hdr: &mut Vec<u8>, raw: &[u8], z: Option<&[u8]>) {
    match z {
        None => hdr.extend_from_slice(raw),
        Some(z) => {
            put_compact_len(hdr, z.len());
            hdr.extend_from_slice(z);
        }
    }
}
