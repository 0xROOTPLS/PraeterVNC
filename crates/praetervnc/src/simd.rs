//! Pixel kernels (auto-vectorized under x86-64-v3).

const M: u32 = 0x00FF_FFFF;

/// True if RGB of all pixels match.
#[inline]
pub fn eq_masked(a: &[u32], b: &[u32]) -> bool {
    debug_assert_eq!(a.len(), b.len());
    let mut ca = a.chunks_exact(16);
    let mut cb = b.chunks_exact(16);
    for (x, y) in (&mut ca).zip(&mut cb) {
        let mut acc = 0u32;
        for i in 0..16 {
            acc |= x[i] ^ y[i];
        }
        if acc & M != 0 {
            return false;
        }
    }
    let mut acc = 0u32;
    for (x, y) in ca.remainder().iter().zip(cb.remainder()) {
        acc |= x ^ y;
    }
    acc & M == 0
}

#[inline]
pub fn copy_masked(dst: &mut [u32], src: &[u32]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d = s & M;
    }
}

/// True if every pixel equals `c`.
#[inline]
pub fn all_eq(px: &[u32], c: u32) -> bool {
    let mut ch = px.chunks_exact(16);
    for x in &mut ch {
        let mut acc = 0u32;
        for &v in x {
            acc |= v ^ c;
        }
        if acc != 0 {
            return false;
        }
    }
    ch.remainder().iter().all(|&v| v == c)
}

/// Fast 64-bit hash of a 64-pixel row (4 independent lanes).
#[inline]
pub fn hash_row(px: &[u32]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let w: &[u64] = unsafe { std::slice::from_raw_parts(px.as_ptr() as *const u64, px.len() / 2) };
    let mut l = [0x243F_6A88_85A3_08D3u64, 0x1319_8A2E_0370_7344, 0xA409_3822_299F_31D0, 0x082E_FA98_EC4E_6C89];
    for c in w.chunks_exact(4) {
        for i in 0..4 {
            l[i] = (l[i] ^ (c[i] & 0x00FF_FFFF_00FF_FFFF)).wrapping_mul(K).rotate_left(31);
        }
    }
    let mut h = l[0] ^ l[1].rotate_left(17) ^ l[2].rotate_left(34) ^ l[3].rotate_left(51);
    h ^= h >> 29;
    h = h.wrapping_mul(K);
    h ^ (h >> 32)
}
