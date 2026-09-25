//! Persistent RFB zlib streams compressed in parallel: each chunk is an independent raw
//! deflate primed with the previous 32 KiB of stream input, ending in a sync flush.
use libz_rs_sys as z;
use rayon::prelude::*;
use std::cell::RefCell;

const WIN: usize = 32 * 1024;

pub struct Deflater {
    s: Box<z::z_stream>,
    level: i32,
}

unsafe impl Send for Deflater {}

impl Deflater {
    pub fn new(level: i32) -> Deflater {
        unsafe {
            let mut s: Box<z::z_stream> = Box::from_raw(std::alloc::alloc_zeroed(std::alloc::Layout::new::<z::z_stream>()) as *mut z::z_stream);
            let r = z::deflateInit2_(&mut *s, level, 8, -15, 8, 0, z::zlibVersion(), std::mem::size_of::<z::z_stream>() as i32);
            assert_eq!(r, 0, "deflateInit2");
            Deflater { s, level }
        }
    }

    pub fn reset(&mut self, level: i32, dict: &[u8]) {
        unsafe {
            z::deflateReset(&mut *self.s);
            if level != self.level {
                z::deflateParams(&mut *self.s, level, 0);
                self.level = level;
            }
            if !dict.is_empty() {
                z::deflateSetDictionary(&mut *self.s, dict.as_ptr(), dict.len() as u32);
            }
        }
    }

    pub fn set_level(&mut self, level: i32) {
        if level != self.level {
            unsafe { z::deflateParams(&mut *self.s, level, 0) };
            self.level = level;
        }
    }

    /// Appends sync-flushed deflate data for `input` to `out`.
    pub fn compress(&mut self, input: &[u8], out: &mut Vec<u8>) {
        unsafe {
            let s = &mut *self.s;
            s.next_in = input.as_ptr();
            s.avail_in = input.len() as u32;
            loop {
                let room = input.len() / 2 + 1024;
                out.reserve(room);
                let len = out.len();
                s.next_out = out.as_mut_ptr().add(len);
                s.avail_out = (out.capacity() - len) as u32;
                let before = s.avail_out;
                let r = z::deflate(s, 2);
                out.set_len(len + (before - s.avail_out) as usize);
                assert!(r == 0 || r == -5, "deflate {r}");
                if s.avail_in == 0 && s.avail_out != 0 {
                    break;
                }
            }
        }
    }
}

impl Drop for Deflater {
    fn drop(&mut self) {
        unsafe { z::deflateEnd(&mut *self.s) };
    }
}

thread_local! {
    static WORKER: RefCell<Option<Deflater>> = const { RefCell::new(None) };
}

fn with_worker<R>(level: i32, dict: &[u8], f: impl FnOnce(&mut Deflater) -> R) -> R {
    WORKER.with(|w| {
        let mut w = w.borrow_mut();
        let d = w.get_or_insert_with(|| Deflater::new(level));
        d.reset(level, dict);
        f(d)
    })
}

/// A slice of one input: (input index, piece index within input, offset, len).
#[derive(Clone, Copy)]
pub struct Piece {
    pub input: usize,
    pub piece: usize,
    pub off: usize,
    pub len: usize,
}

/// Consecutive pieces compressed by one worker after a single dictionary load.
pub struct Job {
    pub pieces: Vec<Piece>,
    pub dict: Vec<u8>,
    pub header: bool,
}

/// Compresses a job; one sync-flushed output per piece.
pub fn run_job(level: i32, j: &Job, input: &dyn Fn(usize) -> std::sync::Arc<Vec<u8>>) -> Vec<Vec<u8>> {
    with_worker(level, &j.dict, |d| {
        j.pieces
            .iter()
            .enumerate()
            .map(|(k, p)| {
                let src = input(p.input);
                let mut o = Vec::with_capacity(p.len / 3 + 64);
                if k == 0 && j.header {
                    o.extend_from_slice(&[0x78, 0x01]);
                }
                d.compress(&src[p.off..p.off + p.len], &mut o);
                o
            })
            .collect()
    })
}

/// One client-side zlib stream.
pub struct ZStream {
    tail: Vec<u8>,
    header_sent: bool,
    seq: Option<Deflater>,
    seq_synced: bool,
}

/// Inputs at or above this total size are split across threads.
pub const PAR_MIN: usize = 192 * 1024;
const CHUNK: usize = 128 * 1024;

impl ZStream {
    pub fn new() -> ZStream {
        ZStream { tail: Vec::with_capacity(WIN), header_sent: false, seq: None, seq_synced: true }
    }

    pub fn reset(&mut self) {
        *self = ZStream::new();
    }

    fn push_tail(&mut self, data: &[u8]) {
        if data.len() >= WIN {
            self.tail.clear();
            self.tail.extend_from_slice(&data[data.len() - WIN..]);
        } else {
            let keep = (WIN - data.len()).min(self.tail.len());
            let drop = self.tail.len() - keep;
            self.tail.drain(..drop);
            self.tail.extend_from_slice(data);
        }
    }

    fn header(&mut self, out: &mut Vec<u8>) {
        if !self.header_sent {
            out.extend_from_slice(&[0x78, 0x01]);
            self.header_sent = true;
        }
    }

    /// Compresses each input in order; returns one compressed blob per input.
    pub fn compress_all(&mut self, level: i32, inputs: &[&[u8]]) -> Vec<Vec<u8>> {
        let total: usize = inputs.iter().map(|i| i.len()).sum();
        if total < PAR_MIN {
            return inputs.iter().map(|i| self.compress_seq(level, i)).collect();
        }
        let owned: Vec<std::sync::Arc<Vec<u8>>> = inputs.iter().map(|i| std::sync::Arc::new(i.to_vec())).collect();
        let jobs = self.plan(inputs);
        let outs: Vec<Vec<Vec<u8>>> = jobs.par_iter().map(|j| run_job(level, j, &|i| owned[i].clone())).collect();
        let mut res: Vec<Vec<u8>> = (0..inputs.len()).map(|_| Vec::new()).collect();
        for (j, o) in jobs.iter().zip(outs) {
            for (p, b) in j.pieces.iter().zip(o) {
                res[p.input].extend_from_slice(&b);
            }
        }
        res
    }

    /// Splits inputs into ~CHUNK-sized jobs; each job is primed with the preceding 32 KiB.
    /// Returns jobs and the piece count per input.
    pub fn plan(&mut self, inputs: &[&[u8]]) -> Vec<Job> {
        let mut jobs: Vec<Job> = Vec::new();
        let mut hist = std::mem::take(&mut self.tail);
        let mut cur: Option<Job> = None;
        let mut cur_len = 0usize;
        for (ii, inp) in inputs.iter().enumerate() {
            let mut off = 0;
            let mut piece = 0;
            while off < inp.len() {
                if cur.is_none() {
                    let header = !self.header_sent;
                    self.header_sent = true;
                    cur = Some(Job { pieces: Vec::new(), dict: hist.clone(), header });
                    cur_len = 0;
                }
                let len = (inp.len() - off).min(CHUNK - cur_len);
                cur.as_mut().unwrap().pieces.push(Piece { input: ii, piece, off, len });
                let data = &inp[off..off + len];
                if data.len() >= WIN {
                    hist.clear();
                    hist.extend_from_slice(&data[data.len() - WIN..]);
                } else {
                    let keep = (WIN - data.len()).min(hist.len());
                    let d = hist.len() - keep;
                    hist.drain(..d);
                    hist.extend_from_slice(data);
                }
                off += len;
                piece += 1;
                cur_len += len;
                if cur_len >= CHUNK {
                    jobs.push(cur.take().unwrap());
                }
            }
        }
        if let Some(j) = cur {
            jobs.push(j);
        }
        self.tail = hist;
        self.seq_synced = false;
        jobs
    }

    pub fn compress_seq(&mut self, level: i32, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() / 3 + 64);
        self.header(&mut out);
        let d = self.seq.get_or_insert_with(|| Deflater::new(level));
        if !self.seq_synced {
            d.reset(level, &self.tail);
            self.seq_synced = true;
        } else {
            d.set_level(level);
        }
        d.compress(input, &mut out);
        self.push_tail(input);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inflate_all(chunks: &[Vec<u8>]) -> Vec<u8> {
        unsafe {
            let mut s: Box<z::z_stream> = Box::from_raw(std::alloc::alloc_zeroed(std::alloc::Layout::new::<z::z_stream>()) as *mut z::z_stream);
            assert_eq!(z::inflateInit_(&mut *s, z::zlibVersion(), std::mem::size_of::<z::z_stream>() as i32), 0);
            let mut out: Vec<u8> = Vec::new();
            for c in chunks {
                s.next_in = c.as_ptr();
                s.avail_in = c.len() as u32;
                loop {
                    out.reserve(1 << 20);
                    let len = out.len();
                    s.next_out = out.as_mut_ptr().add(len);
                    s.avail_out = (out.capacity() - len) as u32;
                    let before = s.avail_out;
                    let r = z::inflate(&mut *s, 2);
                    out.set_len(len + (before - s.avail_out) as usize);
                    assert!(r == 0 || r == -5, "inflate {r}");
                    if s.avail_in == 0 { break; }
                }
            }
            out
        }
    }

    #[test]
    fn roundtrip_mixed() {
        let mut data = Vec::new();
        let mut x = 12345u32;
        for i in 0..3_000_000u32 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            data.push(if i % 7 == 0 { (x >> 24) as u8 } else { (i / 97) as u8 });
        }
        let mut zs = ZStream::new();
        let mut sent = Vec::new();
        let mut expect = Vec::new();
        let parts: Vec<&[u8]> = vec![&data[..100], &data[100..900_000], &data[900_000..900_050], &data[900_050..2_500_000], &data[2_500_000..]];
        for round in 0..3 {
            let sel: Vec<&[u8]> = if round == 1 { vec![parts[0], parts[2]] } else { parts.clone() };
            for (s, o) in sel.iter().zip(zs.compress_all(if round == 2 { 6 } else { 1 }, &sel)) {
                expect.extend_from_slice(s);
                sent.push(o);
            }
        }
        assert_eq!(inflate_all(&sent), expect);
    }
}
