//! Capture microbenchmark: measures DXGI Desktop Duplication characteristics on this machine.
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Performance::*;

#[repr(C)]
struct Shm {
    magic: u64,
    bar_x: i32,
    bar_y: i32,
    frames: u64,
    ts: [u64; 65536],
}

fn main() -> windows::core::Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            let desc = adapter.GetDesc1()?;
            let name = String::from_utf16_lossy(&desc.Description);
            println!("adapter {ai}: {} vram={}MB", name.trim_end_matches('\0'), desc.DedicatedVideoMemory / (1 << 20));
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                let od = output.GetDesc()?;
                let r = od.DesktopCoordinates;
                println!("  output {oi}: {} attached={} rect=({},{})-({},{})", String::from_utf16_lossy(&od.DeviceName).trim_end_matches('\0'), od.AttachedToDesktop.as_bool(), r.left, r.top, r.right, r.bottom);
                oi += 1;
            }
            ai += 1;
        }
        let args: Vec<String> = std::env::args().collect();
        let adapter_idx: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let output_idx: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        let secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);
        let dump: Option<String> = args.get(4).cloned().filter(|d| d != "bar");
        let bar_mode = args.get(4).is_some_and(|d| d == "bar");
        let shm = if bar_mode {
            let m = OpenFileMappingW(FILE_MAP_READ.0, false, windows::core::w!(r"Local\PraeterTestApp"))?;
            Some(&*(MapViewOfFile(m, FILE_MAP_READ, 0, 0, 0).Value as *const Shm))
        } else { None };
        let mut freq = 0i64;
        let _ = QueryPerformanceFrequency(&mut freq);
        let mut bar_lat: Vec<f64> = Vec::new();
        let mut last_id = 0u32;
        let mut f0 = 0u64;

        let adapter = factory.EnumAdapters1(adapter_idx)?;
        let output: IDXGIOutput1 = adapter.EnumOutputs(output_idx)?.cast()?;
        let mut device = None;
        let mut ctx = None;
        D3D11CreateDevice(&adapter, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]), D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut ctx))?;
        let device = device.unwrap();
        let ctx = ctx.unwrap();
        let dup = output.DuplicateOutput(&device)?;
        let dd = dup.GetDesc();
        println!("dup: {}x{} fmt={:?} refresh={}/{} in_sysmem={}", dd.ModeDesc.Width, dd.ModeDesc.Height, dd.ModeDesc.Format, dd.ModeDesc.RefreshRate.Numerator, dd.ModeDesc.RefreshRate.Denominator, dd.DesktopImageInSystemMemory.as_bool());
        let (w, h) = (dd.ModeDesc.Width, dd.ModeDesc.Height);

        let sdesc = D3D11_TEXTURE2D_DESC {
            Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING,
            BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0,
        };
        let mut staging = None;
        device.CreateTexture2D(&sdesc, None, Some(&mut staging))?;
        let staging = staging.unwrap();

        let fb_size = (w * h * 4) as usize;
        let mut prev = vec![0u8; fb_size];
        let mut cur = vec![0u8; fb_size];

        let mut frames = 0u32;
        let mut acq_wait = Vec::new();
        let mut copy_full = Vec::new();
        let mut map_read = Vec::new();
        let mut diff_t = Vec::new();
        let mut dirty_area = Vec::new();
        let mut true_area = Vec::new();
        let mut n_dirty = Vec::new();
        let mut n_move = 0u32;
        let mut accumulated = Vec::new();
        let start = Instant::now();
        let mut rects: Vec<RECT> = vec![RECT::default(); 512];
        while start.elapsed() < Duration::from_secs(secs) {
            let mut fi = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            let t0 = Instant::now();
            match dup.AcquireNextFrame(100, &mut fi, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
                Err(e) => { println!("acquire err {e:?}"); break; }
            }
            let t1 = Instant::now();
            if fi.LastPresentTime == 0 {
                // pointer-only update
                dup.ReleaseFrame()?;
                continue;
            }
            acq_wait.push(t1 - t0);
            accumulated.push(fi.AccumulatedFrames);
            // dirty rects
            let mut req = 0u32;
            let mut area = 0u64;
            let mut nd = 0;
            if fi.TotalMetadataBufferSize > 0 {
                let mut mv: Vec<DXGI_OUTDUPL_MOVE_RECT> = vec![Default::default(); 256];
                if dup.GetFrameMoveRects((mv.len() * std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>()) as u32, mv.as_mut_ptr(), &mut req).is_ok() {
                    n_move += req / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>() as u32;
                }
                if dup.GetFrameDirtyRects((rects.len() * std::mem::size_of::<RECT>()) as u32, rects.as_mut_ptr(), &mut req).is_ok() {
                    nd = req as usize / std::mem::size_of::<RECT>();
                    for r in &rects[..nd] { area += ((r.right - r.left) * (r.bottom - r.top)) as u64; }
                }
            }
            n_dirty.push(nd);
            dirty_area.push(area as f64 / (w * h) as f64);
            let tex: ID3D11Texture2D = res.unwrap().cast()?;
            let t2 = Instant::now();
            ctx.CopyResource(&staging, &tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let t3 = Instant::now();
            let src = std::slice::from_raw_parts(m.pData as *const u8, m.RowPitch as usize * h as usize);
            for y in 0..h as usize {
                let so = y * m.RowPitch as usize;
                cur[y * w as usize * 4..(y + 1) * w as usize * 4].copy_from_slice(&src[so..so + w as usize * 4]);
            }
            ctx.Unmap(&staging, 0);
            let t4 = Instant::now();
            if let Some(sm) = shm {
                if f0 == 0 { f0 = sm.frames; }
                let mut v = 0u32;
                for b in 0..32 {
                    let px = (sm.bar_x + b * 16 + 8) as usize;
                    let py = (sm.bar_y + 8) as usize;
                    let o = (py * w as usize + px) * 4;
                    if cur[o + 1] > 128 { v |= 1 << b; }
                }
                let id = v & 0xFF_FFFF;
                if (id.wrapping_mul(0x9E37_79B1) >> 24) == v >> 24 && id != last_id {
                    let mut now = 0i64;
                    let _ = QueryPerformanceCounter(&mut now);
                    let ts = sm.ts[(id & 0xFFFF) as usize];
                    if ts != 0 { bar_lat.push((now as f64 - ts as f64) / freq as f64 * 1e3); }
                    last_id = id;
                }
            }
            if let Some(d) = &dump {
                let fname = format!("{}_{}x{}.bgra", d, w, h);
                std::fs::write(&fname, &cur).unwrap();
                println!("dumped {fname}");
                dup.ReleaseFrame()?;
                return Ok(());
            }
            dup.ReleaseFrame()?;
            copy_full.push(t3 - t2);
            map_read.push(t4 - t3);
            // tile diff 64x64
            let t5 = Instant::now();
            let mut changed_tiles = 0u64;
            let tw = 64usize;
            let stride = w as usize * 4;
            for ty in (0..h as usize).step_by(tw) {
                for tx in (0..w as usize).step_by(tw) {
                    let x1 = (tx + tw).min(w as usize);
                    let y1 = (ty + tw).min(h as usize);
                    let mut diff = false;
                    for y in ty..y1 {
                        let a = &cur[y * stride + tx * 4..y * stride + x1 * 4];
                        let b = &prev[y * stride + tx * 4..y * stride + x1 * 4];
                        if a != b { diff = true; break; }
                    }
                    if diff { changed_tiles += ((x1 - tx) * (y1 - ty)) as u64; }
                }
            }
            diff_t.push(t5.elapsed());
            true_area.push(changed_tiles as f64 / (w * h) as f64);
            std::mem::swap(&mut prev, &mut cur);
            frames += 1;
        }
        let el = start.elapsed().as_secs_f64();
        if let Some(sm) = shm {
            let n = sm.frames - f0;
            bar_lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p = |q: f64| bar_lat.get(((bar_lat.len().max(1) - 1) as f64 * q) as usize).copied().unwrap_or(f64::NAN);
            println!("capture floor: seen {}/{} lat ms p50 {:.2} p90 {:.2} p99 {:.2} max {:.2}", bar_lat.len(), n, p(0.5), p(0.9), p(0.99), p(1.0));
        }
        println!("frames={} fps={:.1} moves={}", frames, frames as f64 / el, n_move);
        fn stats(name: &str, v: &mut Vec<Duration>) {
            if v.is_empty() { return; }
            v.sort();
            let us = |d: Duration| d.as_secs_f64() * 1e6;
            println!("{name:>12}: p50={:8.1}us p90={:8.1}us max={:8.1}us", us(v[v.len() / 2]), us(v[v.len() * 9 / 10]), us(v[v.len() - 1]));
        }
        stats("acquire", &mut acq_wait);
        stats("copy+map", &mut copy_full);
        stats("readback", &mut map_read);
        stats("cpu diff", &mut diff_t);
        let avg = |v: &Vec<f64>| v.iter().sum::<f64>() / v.len().max(1) as f64;
        println!("dxgi dirty area avg={:.3} ; 64px-tile true changed area avg={:.3}", avg(&dirty_area), avg(&true_area));
        println!("dirty rect count avg={:.1} accumulated avg={:.2}", n_dirty.iter().sum::<usize>() as f64 / n_dirty.len().max(1) as f64, accumulated.iter().sum::<u32>() as f64 / accumulated.len().max(1) as f64);
    }
    Ok(())
}
