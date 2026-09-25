//! Dirty analysis helpers: tile diff bboxes and run building.
use crate::fb::{Rect, Snapshot, TileRef, TS};

/// Inclusive (x0, y0, x1, y1) in tile-local coords.
pub type BBox = (u8, u8, u8, u8);

pub fn full_bbox(snap: &Snapshot, ti: usize) -> BBox {
    let (tw, th) = tile_dims(snap, ti);
    (0, 0, tw as u8 - 1, th as u8 - 1)
}

pub fn tile_dims(snap: &Snapshot, ti: usize) -> (usize, usize) {
    let tx = ti as u32 % snap.tw;
    let ty = ti as u32 / snap.tw;
    ((snap.w - tx * TS as u32).min(TS as u32) as usize, (snap.h - ty * TS as u32).min(TS as u32) as usize)
}

pub fn tile_rect(snap: &Snapshot, ti: usize) -> Rect {
    let (tw, th) = tile_dims(snap, ti);
    Rect::new((ti as u32 % snap.tw) * TS as u32, (ti as u32 / snap.tw) * TS as u32, tw as u32, th as u32)
}

/// Exact changed bbox between two tiles, ignoring rows set in `skip`.
pub fn diff_bbox(a: &TileRef, b: &TileRef, tw: usize, th: usize, skip: u64) -> Option<BBox> {
    let (mut y0, mut y1, mut x0, mut x1) = (usize::MAX, 0, usize::MAX, 0);
    for y in 0..th {
        if skip >> y & 1 != 0 {
            continue;
        }
        let ra = &a.row(y)[..tw];
        let rb = &b.row(y)[..tw];
        if ra == rb {
            continue;
        }
        if y0 == usize::MAX {
            y0 = y;
        }
        y1 = y;
        let f = ra.iter().zip(rb).position(|(p, q)| p != q).unwrap();
        let l = tw - 1 - ra.iter().rev().zip(rb.iter().rev()).position(|(p, q)| p != q).unwrap();
        x0 = x0.min(f);
        x1 = x1.max(l);
    }
    (y0 != usize::MAX).then_some((x0 as u8, y0 as u8, x1 as u8, y1 as u8))
}

pub fn tiles_of(snap: &Snapshot, r: Rect) -> impl Iterator<Item = usize> + '_ {
    let ts = TS as u32;
    let (tx0, tx1) = (r.x as u32 / ts, (r.right() - 1) / ts);
    let (ty0, ty1) = (r.y as u32 / ts, (r.bottom() - 1) / ts);
    (ty0..=ty1).flat_map(move |ty| (tx0..=tx1).map(move |tx| (ty * snap.tw + tx) as usize))
}

/// A mergeable group of tile bboxes; the unit of scheduling.
pub struct Run {
    pub rect: Rect,
    pub tiles: Vec<usize>,
    pub alr: bool,
}

pub fn runs(snap: &Snapshot, bbox: &[Option<BBox>], alr: &[bool]) -> Vec<Run> {
    let tsz = TS as u32;
    let mut row_runs: Vec<(u32, u32, u32, u32, bool, Vec<usize>)> = Vec::new();
    for ty in 0..snap.th {
        let mut tx = 0;
        while tx < snap.tw {
            let ti = (ty * snap.tw + tx) as usize;
            let Some(b) = bbox[ti] else {
                tx += 1;
                continue;
            };
            let a = alr[ti];
            let (x0, mut x1, mut y0, mut y1) = (tx * tsz + b.0 as u32, tx * tsz + b.2 as u32 + 1, ty * tsz + b.1 as u32, ty * tsz + b.3 as u32 + 1);
            let mut tiles = vec![ti];
            let mut right_edge = b.2 as u32 + 1 == tsz;
            tx += 1;
            while tx < snap.tw && right_edge {
                let tj = (ty * snap.tw + tx) as usize;
                match bbox[tj] {
                    Some(nb) if nb.0 == 0 && alr[tj] == a => {
                        x1 = tx * tsz + nb.2 as u32 + 1;
                        y0 = y0.min(ty * tsz + nb.1 as u32);
                        y1 = y1.max(ty * tsz + nb.3 as u32 + 1);
                        right_edge = nb.2 as u32 + 1 == tsz;
                        tiles.push(tj);
                        tx += 1;
                    }
                    _ => break,
                }
            }
            row_runs.push((x0, x1, y0, y1, a, tiles));
        }
    }
    let mut merged: Vec<Run> = Vec::new();
    for (x0, x1, y0, y1, a, tiles) in row_runs {
        if let Some(m) = merged.iter_mut().rev().find(|m| m.rect.x as u32 == x0 && m.rect.right() == x1 && m.rect.bottom() == y0 && m.alr == a) {
            m.rect.h = (y1 - m.rect.y as u32) as u16;
            m.tiles.extend(tiles);
        } else {
            merged.push(Run { rect: Rect::new(x0, y0, x1 - x0, y1 - y0), tiles, alr: a });
        }
    }
    merged
}
