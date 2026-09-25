//! Neighbour-difference statistics per 256x256 region: exact / near (<=32) / far.
use std::collections::BTreeMap;

fn main() {
    let dir = std::env::args().nth(1).expect("frames dir");
    let (w, h) = (2560usize, 1440usize);
    for name in ["mon0", "mon1", "web", "code", "photo"] {
        let raw = std::fs::read(format!("{dir}/{name}_{w}x{h}.bgra")).unwrap();
        let px: Vec<u32> = raw.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) & 0xFF_FFFF).collect();
        let mut hist: BTreeMap<(u32, u32), u32> = BTreeMap::new();
        for ry in (0..h - 256).step_by(256) {
            for rx in (0..w).step_by(256) {
                let (mut eq, mut near, mut far, mut n) = (0, 0, 0, 0);
                let mut colors = std::collections::HashSet::new();
                for y in (ry + 1..ry + 256).step_by(2) {
                    for x in rx + 1..(rx + 256).min(w) {
                        let a = px[y * w + x];
                        let b = px[y * w + x - 1];
                        colors.insert(a);
                        let d = [0, 8, 16].iter().map(|s| ((a >> s & 255) as i32 - (b >> s & 255) as i32).abs()).max().unwrap();
                        if d == 0 { eq += 1 } else if d <= 32 { near += 1 } else { far += 1 }
                        n += 1;
                    }
                }
                if colors.len() <= 256 { continue; }
                *hist.entry((near * 10 / n, eq * 10 / n)).or_default() += 1;
                let _ = far;
            }
        }
        println!("{name}: (near/10, eq/10) -> count, many-colour regions only");
        for (k, v) in hist { print!(" {:?}:{}", k, v); }
        println!();
    }
}
