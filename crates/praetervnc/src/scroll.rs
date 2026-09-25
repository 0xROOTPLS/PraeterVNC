//! Vertical scroll detection against the client's framebuffer model -> CopyRect.
use crate::fb::{Rect, Snapshot, TS};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug)]
pub struct Copy {
    pub dst: Rect,
    pub sx: u32,
    pub sy: u32,
}

const MIN_BAND: u32 = 8;

#[inline]
fn row_hash(s: &Snapshot, tx: u32, y: u32) -> (u64, bool) {
    let h = s.tile(tx, y / TS as u32).hashes();
    let r = (y % TS as u32) as usize;
    (h.h[r], h.uniform >> r & 1 != 0)
}

/// Best dy for rows [y0, y1) of tile column `tx`, or None.
fn vote(new: &Snapshot, old: &Snapshot, tx: u32, y0: u32, y1: u32) -> Option<i32> {
    let len = y1 - y0;
    let o0 = y0.saturating_sub(len.saturating_sub(16));
    let o1 = (y1 + len.saturating_sub(16)).min(new.h);
    // Unique non-uniform old rows only.
    let mut map: HashMap<u64, u32> = HashMap::with_capacity((o1 - o0) as usize);
    for y in o0..o1 {
        let (h, uni) = row_hash(old, tx, y);
        if uni {
            continue;
        }
        map.entry(h).and_modify(|v| *v = u32::MAX).or_insert(y);
    }
    let mut votes: HashMap<i32, u32> = HashMap::new();
    let mut cand = 0u32;
    for y in y0..y1 {
        let (h, uni) = row_hash(new, tx, y);
        if uni {
            continue;
        }
        cand += 1;
        if let Some(&ym) = map.get(&h) {
            if ym != u32::MAX {
                *votes.entry(y as i32 - ym as i32).or_default() += 1;
            }
        }
    }
    let (&dy, &n) = votes.iter().filter(|(&d, _)| d != 0).max_by_key(|(_, &n)| n)?;
    let still = votes.get(&0).copied().unwrap_or(0);
    (n >= 4 && n * 4 >= cand && n > still).then_some(dy)
}

fn rows_equal(new: &Snapshot, old: &Snapshot, tx0: u32, tx1: u32, y: u32, ys: u32) -> bool {
    for tx in tx0..tx1 {
        let (a, _) = row_hash(new, tx, y);
        let (b, _) = row_hash(old, tx, ys);
        if a != b {
            return false;
        }
        let ra = new.tile(tx, y / TS as u32).row((y % TS as u32) as usize);
        let rb = old.tile(tx, ys / TS as u32).row((ys % TS as u32) as usize);
        if ra != rb {
            return false;
        }
    }
    true
}

/// `dirty[ti]`: tile differs between old and new.
pub fn detect(new: &Snapshot, old: &Snapshot, dirty: &[bool]) -> Vec<Copy> {
    let (tw, th) = (new.tw, new.th);
    let mut found: Vec<(u32, u32, u32, i32)> = Vec::new();
    for tx in 0..tw {
        let mut ty = 0;
        while ty < th {
            if !dirty[(ty * tw + tx) as usize] {
                ty += 1;
                continue;
            }
            let s = ty;
            while ty < th && dirty[(ty * tw + tx) as usize] {
                ty += 1;
            }
            if ty - s < 2 {
                continue;
            }
            let y0 = s * TS as u32;
            let y1 = (ty * TS as u32).min(new.h);
            if let Some(dy) = vote(new, old, tx, y0, y1) {
                found.push((tx, y0, y1, dy));
            }
        }
    }
    if found.is_empty() {
        return Vec::new();
    }
    // Group adjacent columns with equal dy and overlapping rows.
    found.sort_by_key(|f| (f.3, f.0, f.1));
    let mut groups: Vec<(u32, u32, u32, u32, i32)> = Vec::new();
    for (tx, y0, y1, dy) in found {
        if let Some(g) = groups.iter_mut().find(|g| g.4 == dy && g.1 == tx && y0 < g.3 && y1 > g.2) {
            g.1 = tx + 1;
            g.2 = g.2.min(y0);
            g.3 = g.3.max(y1);
        } else {
            groups.push((tx, tx + 1, y0, y1, dy));
        }
    }
    let mut copies = Vec::new();
    for (tx0, tx1, y0, y1, dy) in groups {
        let x0 = tx0 * TS as u32;
        let x1 = (tx1 * TS as u32).min(new.w);
        let mut band: Option<u32> = None;
        for y in y0..=y1 {
            let ok = y < y1 && {
                let ys = y as i32 - dy;
                ys >= 0 && (ys as u32) < new.h && rows_equal(new, old, tx0, tx1, y, ys as u32)
            };
            match (ok, band) {
                (true, None) => band = Some(y),
                (false, Some(b)) => {
                    if y - b >= MIN_BAND {
                        copies.push(Copy { dst: Rect::new(x0, b, x1 - x0, y - b), sx: x0, sy: (b as i32 - dy) as u32 });
                    }
                    band = None;
                }
                _ => {}
            }
        }
    }
    // Drop copies whose source overlaps another copy's destination.
    let mut keep = vec![true; copies.len()];
    for i in 0..copies.len() {
        let src = Rect::new(copies[i].sx, copies[i].sy, copies[i].dst.w as u32, copies[i].dst.h as u32);
        for j in 0..copies.len() {
            if i != j && keep[j] && src.intersect(&copies[j].dst).is_some() {
                let (a, b) = (copies[i].dst.area(), copies[j].dst.area());
                keep[if a < b { i } else { j }] = false;
            }
        }
    }
    copies.into_iter().zip(keep).filter(|(_, k)| *k).map(|(c, _)| c).collect()
}
