//! Builds the exe's resources (icons, manifest, version) into a .res file for the linker.
use std::path::PathBuf;

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="PraeterVNC" version="1.0.0.0" processorArchitecture="amd64"/>
  <dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security></trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1"><application><supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/></application></compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings>
    <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
    <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
  </windowsSettings></application>
</assembly>
"#;

#[derive(Clone, Copy)]
enum Variant {
    Normal,
    Connected,
    Inactive,
}

fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn rgb(h: u32) -> [f32; 3] {
    [(h >> 16) as f32 / 255.0, (h >> 8 & 0xFF) as f32 / 255.0, (h & 0xFF) as f32 / 255.0]
}

fn seg(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((px - a.0) * dx + (py - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((px - a.0 - t * dx).powi(2) + (py - a.1 - t * dy).powi(2)).sqrt()
}

fn round_box(px: f32, py: f32, half: f32, r: f32) -> f32 {
    let qx = (px - 0.5).abs() - half + r;
    let qy = (py - 0.5).abs() - half + r;
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

/// Straight-alpha RGBA sample at unit coordinates.
fn sample(x: f32, y: f32, v: Variant) -> [f32; 4] {
    let (c0, c1) = match v {
        Variant::Inactive => (rgb(0x868E96), rgb(0xADB5BD)),
        _ => (rgb(0x3B5BDB), rgb(0x15AABF)),
    };
    if let Variant::Connected = v {
        let d = ((x - 0.78).powi(2) + (y - 0.78).powi(2)).sqrt();
        if d < 0.19 {
            let g = rgb(0x2FB344);
            return [g[0], g[1], g[2], 1.0];
        }
        if d < 0.25 {
            return [1.0; 4];
        }
    }
    if round_box(x, y, 0.47, 0.2) > 0.0 {
        return [0.0; 4];
    }
    let c = lerp(c0, c1, (x + y) * 0.5);
    let hw = 0.068;
    let chevrons = [0.0, 0.24].iter().any(|&o| {
        let (a, b, cc) = ((0.22 + o, 0.28), (0.42 + o, 0.5), (0.22 + o, 0.72));
        seg(x, y, a, b).min(seg(x, y, b, cc)) < hw
    });
    if chevrons {
        return [1.0; 4];
    }
    [c[0], c[1], c[2], 1.0]
}

/// 32bpp icon DIB (XOR + AND mask), 4x4 supersampled.
fn dib(n: u32, v: Variant) -> Vec<u8> {
    let mut px = vec![[0u8; 4]; (n * n) as usize];
    for y in 0..n {
        for x in 0..n {
            let mut acc = [0f32; 4];
            for sy in 0..4 {
                for sx in 0..4 {
                    let s = sample((x as f32 + (sx as f32 + 0.5) / 4.0) / n as f32, (y as f32 + (sy as f32 + 0.5) / 4.0) / n as f32, v);
                    for i in 0..3 {
                        acc[i] += s[i] * s[3];
                    }
                    acc[3] += s[3];
                }
            }
            let a = acc[3] / 16.0;
            let col = |i: usize| if acc[3] > 0.0 { (acc[i] / acc[3] * 255.0).round() as u8 } else { 0 };
            px[(y * n + x) as usize] = [col(2), col(1), col(0), (a * 255.0).round() as u8];
        }
    }
    let stride = n.div_ceil(32) * 4;
    let mut o = Vec::new();
    for v in [40u32, n, n * 2] {
        o.extend_from_slice(&v.to_le_bytes());
    }
    o.extend_from_slice(&1u16.to_le_bytes());
    o.extend_from_slice(&32u16.to_le_bytes());
    for v in [0u32, n * n * 4 + stride * n, 0, 0, 0, 0] {
        o.extend_from_slice(&v.to_le_bytes());
    }
    for y in (0..n).rev() {
        for x in 0..n {
            o.extend_from_slice(&px[(y * n + x) as usize]);
        }
    }
    for y in (0..n).rev() {
        let mut row = vec![0u8; stride as usize];
        for x in 0..n {
            if px[(y * n + x) as usize][3] == 0 {
                row[(x / 8) as usize] |= 0x80 >> (x % 8);
            }
        }
        o.extend_from_slice(&row);
    }
    o
}

struct Res(Vec<u8>);

impl Res {
    fn add(&mut self, ty: u16, id: u16, flags: u16, data: &[u8]) {
        let o = &mut self.0;
        o.extend_from_slice(&(data.len() as u32).to_le_bytes());
        o.extend_from_slice(&32u32.to_le_bytes());
        for v in [0xFFFFu16, ty, 0xFFFF, id] {
            o.extend_from_slice(&v.to_le_bytes());
        }
        o.extend_from_slice(&0u32.to_le_bytes());
        o.extend_from_slice(&flags.to_le_bytes());
        o.extend_from_slice(&0x0409u16.to_le_bytes());
        o.extend_from_slice(&[0u8; 8]);
        o.extend_from_slice(data);
        while o.len() % 4 != 0 {
            o.push(0);
        }
    }
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(|c| c.to_le_bytes()).collect()
}

/// VS_VERSIONINFO-style node: header, key, value, children, each 32-bit aligned.
fn node(key: &str, value: &[u8], value_len: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut o = vec![0u8; 6];
    o.extend_from_slice(&utf16z(key));
    while o.len() % 4 != 0 {
        o.push(0);
    }
    o.extend_from_slice(value);
    for c in children {
        while o.len() % 4 != 0 {
            o.push(0);
        }
        o.extend_from_slice(c);
    }
    let len = o.len() as u16;
    o[0..2].copy_from_slice(&len.to_le_bytes());
    o[2..4].copy_from_slice(&value_len.to_le_bytes());
    o[4..6].copy_from_slice(&(text as u16).to_le_bytes());
    o
}

fn version_info(ver: &str) -> Vec<u8> {
    let p: Vec<u32> = ver.split('.').map(|x| x.parse().unwrap_or(0)).chain([0, 0, 0]).take(4).collect();
    let ms = p[0] << 16 | p[1];
    let ls = p[2] << 16 | p[3];
    let mut fixed = Vec::new();
    for v in [0xFEEF04BDu32, 0x10000, ms, ls, ms, ls, 0x3F, 0, 0x40004, 1, 0, 0, 0] {
        fixed.extend_from_slice(&v.to_le_bytes());
    }
    let s = |k: &str, v: &str| node(k, &utf16z(v), v.encode_utf16().count() as u16 + 1, true, &[]);
    let strings = [
        s("CompanyName", "PraeterVNC"),
        s("FileDescription", "PraeterVNC Server"),
        s("FileVersion", ver),
        s("InternalName", "praetervnc"),
        s("OriginalFilename", "praetervnc.exe"),
        s("ProductName", "PraeterVNC"),
        s("ProductVersion", ver),
    ];
    let table = node("040904B0", &[], 0, true, &strings);
    let sfi = node("StringFileInfo", &[], 0, true, &[table]);
    let tr = [0x09u8, 0x04, 0xB0, 0x04];
    let var = node("Translation", &tr, 4, false, &[]);
    let vfi = node("VarFileInfo", &[], 0, true, &[var]);
    node("VS_VERSION_INFO", &fixed, fixed.len() as u16, false, &[sfi, vfi])
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut r = Res(Vec::new());
    r.0.extend_from_slice(&0u32.to_le_bytes());
    r.0.extend_from_slice(&32u32.to_le_bytes());
    r.0.extend_from_slice(&[0xFF, 0xFF, 0, 0, 0xFF, 0xFF, 0, 0]);
    r.0.extend_from_slice(&[0u8; 16]);
    let mut ico = Vec::new();
    let mut next = 1u16;
    for (group, v, sizes) in [
        (1u16, Variant::Normal, &[16u32, 20, 24, 32, 40, 48, 64, 256][..]),
        (2, Variant::Connected, &[16, 20, 24, 32, 40, 48, 64][..]),
        (3, Variant::Inactive, &[16, 20, 24, 32, 40, 48, 64][..]),
    ] {
        let mut dir = Vec::new();
        for v16 in [0u16, 1, sizes.len() as u16] {
            dir.extend_from_slice(&v16.to_le_bytes());
        }
        for &n in sizes {
            let d = dib(n, v);
            r.add(3, next, 0x1010, &d);
            dir.extend_from_slice(&[if n >= 256 { 0 } else { n as u8 }, if n >= 256 { 0 } else { n as u8 }, 0, 0]);
            dir.extend_from_slice(&1u16.to_le_bytes());
            dir.extend_from_slice(&32u16.to_le_bytes());
            dir.extend_from_slice(&(d.len() as u32).to_le_bytes());
            dir.extend_from_slice(&next.to_le_bytes());
            next += 1;
        }
        r.add(14, group, 0x1030, &dir);
        if group == 1 {
            let imgs: Vec<Vec<u8>> = sizes.iter().map(|&n| dib(n, v)).collect();
            ico.extend_from_slice(&[0, 0, 1, 0]);
            ico.extend_from_slice(&(imgs.len() as u16).to_le_bytes());
            let mut off = 6 + 16 * imgs.len() as u32;
            for (&n, d) in sizes.iter().zip(&imgs) {
                let b = if n >= 256 { 0 } else { n as u8 };
                ico.extend_from_slice(&[b, b, 0, 0, 1, 0, 32, 0]);
                ico.extend_from_slice(&(d.len() as u32).to_le_bytes());
                ico.extend_from_slice(&off.to_le_bytes());
                off += d.len() as u32;
            }
            imgs.iter().for_each(|d| ico.extend_from_slice(d));
        }
    }
    r.add(24, 1, 0x1030, MANIFEST.as_bytes());
    r.add(16, 1, 0x0030, &version_info(&std::env::var("CARGO_PKG_VERSION").unwrap()));
    let dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let out = dir.join("praetervnc.res");
    std::fs::write(&out, &r.0).unwrap();
    std::fs::write(dir.join("praetervnc.ico"), &ico).unwrap();
    println!("cargo:rustc-link-arg-bin=praetervnc={}", out.display());
}
