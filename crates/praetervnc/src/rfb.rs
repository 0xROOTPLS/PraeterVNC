//! RFB constants and wire helpers.

pub mod enc {
    pub const RAW: i32 = 0;
    pub const COPYRECT: i32 = 1;
    pub const RRE: i32 = 2;
    pub const HEXTILE: i32 = 5;
    pub const ZLIB: i32 = 6;
    pub const TIGHT: i32 = 7;
    pub const TRLE: i32 = 15;
    pub const ZRLE: i32 = 16;
    pub const JPEG: i32 = 21;
    pub const H264: i32 = 50;
    pub const TIGHT_PNG: i32 = -260;
    pub const JPEG_Q0: i32 = -32;
    pub const JPEG_Q9: i32 = -23;
    pub const COMPRESS_0: i32 = -256;
    pub const COMPRESS_9: i32 = -247;
    pub const FINE_Q0: i32 = -512;
    pub const FINE_Q100: i32 = -412;
    pub const SUBSAMP_1X: i32 = -768;
    pub const SUBSAMP_GRAY: i32 = -763;
    pub const DESKTOP_SIZE: i32 = -223;
    pub const LAST_RECT: i32 = -224;
    pub const POINTER_POS: i32 = -232;
    pub const CURSOR: i32 = -239;
    pub const XCURSOR: i32 = -240;
    pub const QEMU_POINTER_MOTION: i32 = -257;
    pub const QEMU_EXT_KEY: i32 = -258;
    pub const DESKTOP_NAME: i32 = -307;
    pub const EXT_DESKTOP_SIZE: i32 = -308;
    pub const FENCE: i32 = -312;
    pub const CONTINUOUS_UPDATES: i32 = -313;
    pub const CURSOR_ALPHA: i32 = -314;
    pub const EXT_CLIPBOARD: i32 = 0xC0A1E5CEu32 as i32;
}

pub fn put_rect_header(o: &mut Vec<u8>, x: u16, y: u16, w: u16, h: u16, encoding: i32) {
    o.extend_from_slice(&x.to_be_bytes());
    o.extend_from_slice(&y.to_be_bytes());
    o.extend_from_slice(&w.to_be_bytes());
    o.extend_from_slice(&h.to_be_bytes());
    o.extend_from_slice(&encoding.to_be_bytes());
}

/// Tight compact length.
pub fn put_compact_len(o: &mut Vec<u8>, n: usize) {
    let n = n as u32;
    if n < 0x80 {
        o.push(n as u8);
    } else if n < 0x4000 {
        o.push((n & 0x7F) as u8 | 0x80);
        o.push((n >> 7) as u8);
    } else {
        o.push((n & 0x7F) as u8 | 0x80);
        o.push(((n >> 7) & 0x7F) as u8 | 0x80);
        o.push((n >> 14) as u8);
    }
}

/// VNC authentication response: DES-ECB of the challenge keyed by the bit-reversed password.
pub fn vnc_auth_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    use des::cipher::{BlockEncrypt, KeyInit};
    let mut key = [0u8; 8];
    for (i, b) in password.bytes().take(8).enumerate() {
        key[i] = b.reverse_bits();
    }
    let c = des::Des::new_from_slice(&key).unwrap();
    let mut out = *challenge;
    for blk in out.chunks_exact_mut(8) {
        c.encrypt_block(blk.into());
    }
    out
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    let mut b = [0u8; N];
    unsafe {
        let _ = BCryptGenRandom(None, &mut b, BCRYPT_USE_SYSTEM_PREFERRED_RNG);
    }
    b
}
