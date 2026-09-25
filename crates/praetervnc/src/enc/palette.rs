//! Fast colour counting / indexing.

const SLOTS: usize = 1024;
const EMPTY: u32 = u32::MAX;

pub struct Palette {
    keys: [u32; SLOTS],
    vals: [u8; SLOTS],
    pub colors: Vec<u32>,
    pub idx: Vec<u8>,
}

impl Default for Palette {
    fn default() -> Self {
        Palette { keys: [EMPTY; SLOTS], vals: [0; SLOTS], colors: Vec::with_capacity(256), idx: Vec::new() }
    }
}

#[inline(always)]
fn slot(c: u32) -> usize {
    (c.wrapping_mul(0x9E37_79B1) >> 22) as usize
}

impl Palette {
    fn clear(&mut self) {
        for &c in &self.colors {
            let mut s = slot(c);
            while self.keys[s] != EMPTY {
                self.keys[s] = EMPTY;
                s = (s + 1) & (SLOTS - 1);
            }
        }
        self.colors.clear();
    }

    #[inline(always)]
    fn lookup(&mut self, c: u32, max: usize) -> Option<u8> {
        let mut s = slot(c);
        loop {
            let k = self.keys[s];
            if k == c {
                return Some(self.vals[s]);
            }
            if k == EMPTY {
                if self.colors.len() >= max {
                    return None;
                }
                let i = self.colors.len() as u8;
                self.keys[s] = c;
                self.vals[s] = i;
                self.colors.push(c);
                return Some(i);
            }
            s = (s + 1) & (SLOTS - 1);
        }
    }

    /// Indexes `px` if it has at most `max` (<= 256) colours; fills `colors` and `idx`.
    pub fn build(&mut self, px: &[u32], max: usize) -> Option<usize> {
        self.clear();
        self.idx.clear();
        self.idx.reserve(px.len());
        if px.is_empty() {
            return Some(0);
        }
        let mut last = px[0];
        let mut li = self.lookup(last, max)?;
        let mut i = 0;
        let n = px.len();
        unsafe {
            let out = self.idx.as_mut_ptr();
            while i < n {
                // Skip runs 8 at a time.
                if i + 8 <= n {
                    let c = &px[i..i + 8];
                    if c.iter().all(|&v| v == last) {
                        std::ptr::write_bytes(out.add(i), li, 8);
                        i += 8;
                        continue;
                    }
                }
                let p = *px.get_unchecked(i);
                if p != last {
                    match self.lookup(p, max) {
                        Some(v) => li = v,
                        None => {
                            self.idx.set_len(0);
                            return None;
                        }
                    }
                    last = p;
                }
                *out.add(i) = li;
                i += 1;
            }
            self.idx.set_len(n);
        }
        Some(self.colors.len())
    }

    /// Counts colours up to `max`; None if more.
    pub fn count(&mut self, px: &[u32], max: usize) -> Option<usize> {
        self.clear();
        let mut last = px.first().copied()?;
        self.lookup(last, max)?;
        for &p in px {
            if p != last {
                self.lookup(p, max)?;
                last = p;
            }
        }
        Some(self.colors.len())
    }
}
