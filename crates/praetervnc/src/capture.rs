//! DXGI Desktop Duplication capture into the shared tiled framebuffer.
use crate::fb::*;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::core::Interface;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

#[derive(Clone, Debug)]
pub struct OutputInfo {
    pub adapter: u32,
    pub output: u32,
    pub name: String,
    pub rect: RECT,
}

pub fn enum_outputs() -> Vec<OutputInfo> {
    let mut v = Vec::new();
    unsafe {
        let Ok(f) = CreateDXGIFactory1::<IDXGIFactory1>() else { return v };
        let mut ai = 0;
        while let Ok(a) = f.EnumAdapters1(ai) {
            let mut oi = 0;
            while let Ok(o) = a.EnumOutputs(oi) {
                if let Ok(d) = o.GetDesc() {
                    if d.AttachedToDesktop.as_bool() {
                        v.push(OutputInfo {
                            adapter: ai,
                            output: oi,
                            name: String::from_utf16_lossy(&d.DeviceName).trim_end_matches('\0').to_string(),
                            rect: d.DesktopCoordinates,
                        });
                    }
                }
                oi += 1;
            }
            ai += 1;
        }
    }
    v
}

pub struct Layout {
    pub outputs: Vec<OutputInfo>,
    pub origin: (i32, i32),
    pub w: u32,
    pub h: u32,
}

impl Layout {
    pub fn new(outputs: Vec<OutputInfo>) -> Layout {
        let l = outputs.iter().map(|o| o.rect.left).min().unwrap_or(0);
        let t = outputs.iter().map(|o| o.rect.top).min().unwrap_or(0);
        let r = outputs.iter().map(|o| o.rect.right).max().unwrap_or(0);
        let b = outputs.iter().map(|o| o.rect.bottom).max().unwrap_or(0);
        Layout { outputs, origin: (l, t), w: (r - l) as u32, h: (b - t) as u32 }
    }
}

struct Working {
    tiles: Vec<TileRef>,
    cursor: Option<Arc<CursorShape>>,
    cursor_seq: u64,
    cursor_pos: (i32, i32),
    cursor_visible: bool,
    cursor_owner: usize,
    seq: u64,
}

pub struct Capture {
    pub verbose: bool,
    /// None = all outputs.
    pub monitor: Mutex<Option<usize>>,
    pub store: Arc<FrameStore>,
    pub layout: Mutex<Arc<Layout>>,
    pub stats: Mutex<CapStats>,
}

#[derive(Default, Clone, Debug)]
pub struct CapStats {
    pub frames: u64,
    pub copy_us: u64,
    pub diff_us: u64,
    pub dirty_tiles: u64,
    pub changed_tiles: u64,
}

impl Capture {
    /// Spawns the capture supervisor. `monitor`: None = all outputs.
    pub fn start(monitor: Option<usize>, verbose: bool) -> Arc<Capture> {
        let outs = select(monitor);
        let layout = Arc::new(Layout::new(outs));
        let cap = Arc::new(Capture {
            verbose,
            monitor: Mutex::new(monitor),
            store: Arc::new(FrameStore::new(Snapshot::blank(layout.w.max(1), layout.h.max(1), 1))),
            layout: Mutex::new(layout),
            stats: Mutex::new(CapStats::default()),
        });
        let c2 = cap.clone();
        std::thread::Builder::new().name("capture-sup".into()).spawn(move || c2.supervise()).unwrap();
        cap
    }

    fn supervise(self: Arc<Self>) {
        let mut layout_seq = 1u64;
        let mut work: Option<(u64, Arc<Mutex<Working>>)> = None;
        loop {
            let layout = self.layout.lock().clone();
            // Keep tiles across transient loss.
            let work = match &work {
                Some((ls, w)) if *ls == layout_seq => w.clone(),
                _ => {
                    let blank = Snapshot::blank(layout.w.max(1), layout.h.max(1), layout_seq);
                    let w = Arc::new(Mutex::new(Working {
                        tiles: blank.tiles.clone(),
                        cursor: None, cursor_seq: 0, cursor_pos: (0, 0), cursor_visible: true, cursor_owner: usize::MAX, seq: 0,
                    }));
                    work = Some((layout_seq, w.clone()));
                    w
                }
            };
            let stop = Arc::new(AtomicBool::new(false));
            let lost = Arc::new(crate::fb::Signal::default());
            let mut handles = Vec::new();
            for (i, o) in layout.outputs.iter().enumerate() {
                let (o, layout, work, stop, lost, me) = (o.clone(), layout.clone(), work.clone(), stop.clone(), lost.clone(), self.clone());
                handles.push(std::thread::Builder::new().name(format!("capture-{i}")).spawn(move || {
                    set_mmcss("Capture");
                    let r = me.run_output(i, &o, &layout, layout_seq, &work, &stop);
                    if let Err(e) = r {
                        crate::log!("capture {}: {e:?}", o.name);
                    }
                    lost.notify();
                }).unwrap());
            }
            // Wait for any output to fail or the layout to change.
            let mut last_log = std::time::Instant::now();
            loop {
                if lost.wait_timeout(Duration::from_millis(1000)) {
                    break;
                }
                if self.verbose && last_log.elapsed() >= Duration::from_secs(5) {
                    let s = std::mem::take(&mut *self.stats.lock());
                    let f = s.frames.max(1) as f64;
                    crate::log!("capture: {:.0} fps copy+map {:.2} ms diff {:.2} ms dirty {:.0} changed {:.0} tiles/frame",
                        s.frames as f64 / last_log.elapsed().as_secs_f64(), s.copy_us as f64 / f / 1e3, s.diff_us as f64 / f / 1e3, s.dirty_tiles as f64 / f, s.changed_tiles as f64 / f);
                    last_log = std::time::Instant::now();
                }
                let now = Layout::new(select(*self.monitor.lock()));
                let moved = now.outputs.iter().zip(&layout.outputs).any(|(a, b)| a.name != b.name || a.rect != b.rect);
                if now.w != layout.w || now.h != layout.h || now.origin != layout.origin || now.outputs.len() != layout.outputs.len() || moved {
                    break;
                }
            }
            stop.store(true, Ordering::SeqCst);
            for h in handles {
                let _ = h.join();
            }
            std::thread::sleep(Duration::from_millis(100));
            let nl = Arc::new(Layout::new(select(*self.monitor.lock())));
            if nl.w != layout.w || nl.h != layout.h {
                layout_seq += 1;
                crate::log!("layout changed to {}x{}", nl.w, nl.h);
            }
            *self.layout.lock() = nl;
        }
    }

    fn run_output(&self, idx: usize, oi: &OutputInfo, layout: &Layout, layout_seq: u64, work: &Mutex<Working>, stop: &AtomicBool) -> windows::core::Result<()> {
        unsafe {
            let f: IDXGIFactory1 = CreateDXGIFactory1()?;
            let adapter = f.EnumAdapters1(oi.adapter)?;
            let output: IDXGIOutput1 = adapter.EnumOutputs(oi.output)?.cast()?;
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(&adapter, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_0]), D3D11_SDK_VERSION,
                Some(&mut device), None, Some(&mut ctx))?;
            let (device, ctx) = (device.unwrap(), ctx.unwrap());
            if let Ok(d) = device.cast::<IDXGIDevice>() {
                let _ = d.SetGPUThreadPriority(7);
            }
            let spin_map = std::env::var("PRAETER_SPIN_MAP").is_ok();
            let beat = crate::watchdog::looped(format!("capture {}", oi.name), Duration::from_secs(10));
            let dup = loop {
                beat.tick();
                crate::desktop::follow_input();
                match output.DuplicateOutput(&device) {
                    Ok(d) => break d,
                    Err(e) if stop.load(Ordering::Relaxed) => return Err(e),
                    Err(e) => {
                        crate::log!("DuplicateOutput {}: {e:?}; retrying", oi.name);
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            };
            let dd = dup.GetDesc();
            let (ow, oh) = (dd.ModeDesc.Width, dd.ModeDesc.Height);
            if dd.Rotation != DXGI_MODE_ROTATION_IDENTITY && dd.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED {
                crate::log!("warning: rotated output {} unsupported", oi.name);
            }
            let ox = (oi.rect.left - layout.origin.0) as u32;
            let oy = (oi.rect.top - layout.origin.1) as u32;
            crate::log!("capturing {} {}x{} at +{}+{} ({}Hz)", oi.name, ow, oh, ox, oy,
                dd.ModeDesc.RefreshRate.Numerator / dd.ModeDesc.RefreshRate.Denominator.max(1));
            let sdesc = D3D11_TEXTURE2D_DESC {
                Width: ow, Height: oh, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING,
                BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0,
            };
            let mut staging = None;
            device.CreateTexture2D(&sdesc, None, Some(&mut staging))?;
            let staging = staging.unwrap();

            let tw = layout.w.div_ceil(TS as u32);
            let th = layout.h.div_ceil(TS as u32);
            // Global tile range covered by this output.
            let t0x = ox / TS as u32;
            let t0y = oy / TS as u32;
            // The mode can briefly exceed the layout snapshot.
            let t1x = (ox + ow).div_ceil(TS as u32).min(tw);
            let t1y = (oy + oh).div_ceil(TS as u32).min(th);
            let mut mark = vec![false; (tw * th) as usize];
            let mut rects: Vec<RECT> = vec![RECT::default(); 64];
            let mut moves: Vec<DXGI_OUTDUPL_MOVE_RECT> = vec![Default::default(); 16];
            let mut shape_buf: Vec<u8> = Vec::new();
            let mut first = true;

            while !stop.load(Ordering::Relaxed) {
                beat.tick();
                let mut fi = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut res: Option<IDXGIResource> = None;
                match dup.AcquireNextFrame(200, &mut fi, &mut res) {
                    Ok(()) => {}
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
                    Err(e) => return Err(e),
                }
                let t_acq = crate::probe::now();
                let mut cursor_changed = false;
                // Pointer.
                if fi.LastMouseUpdateTime != 0 {
                    let mut w = work.lock();
                    let vis = fi.PointerPosition.Visible.as_bool();
                    if vis || w.cursor_owner == idx {
                        let p = fi.PointerPosition.Position;
                        let np = (p.x + ox as i32, p.y + oy as i32);
                        if w.cursor_visible != vis || w.cursor_pos != np {
                            w.cursor_visible = vis;
                            w.cursor_pos = np;
                            cursor_changed = true;
                        }
                        w.cursor_owner = if vis { idx } else { usize::MAX };
                    }
                }
                if fi.PointerShapeBufferSize > 0 {
                    shape_buf.resize(fi.PointerShapeBufferSize as usize, 0);
                    let mut req = 0u32;
                    let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                    if dup.GetFramePointerShape(shape_buf.len() as u32, shape_buf.as_mut_ptr() as *mut _, &mut req, &mut si).is_ok() {
                        if let Some(cs) = convert_shape(&shape_buf, &si) {
                            let mut w = work.lock();
                            w.cursor = Some(Arc::new(cs));
                            w.cursor_seq += 1;
                            cursor_changed = true;
                        }
                    }
                }
                if fi.LastPresentTime == 0 && !first {
                    dup.ReleaseFrame()?;
                    if cursor_changed {
                        self.publish(work, layout, layout_seq, tw, th, fi.LastPresentTime, t_acq);
                    }
                    continue;
                }
                // Dirty + move-destination rects -> tiles.
                let t_copy = std::time::Instant::now();
                mark.iter_mut().for_each(|m| *m = false);
                let mut any = false;
                let mut mark_rect = |l: i32, t: i32, r: i32, b: i32, mark: &mut Vec<bool>| {
                    let l = (l.max(0) as u32 + ox) / TS as u32;
                    let t = (t.max(0) as u32 + oy) / TS as u32;
                    let r = ((r.min(ow as i32).max(0) as u32 + ox).div_ceil(TS as u32)).min(tw);
                    let b = ((b.min(oh as i32).max(0) as u32 + oy).div_ceil(TS as u32)).min(th);
                    for ty in t..b {
                        for tx in l..r {
                            mark[(ty * tw + tx) as usize] = true;
                            any = true;
                        }
                    }
                };
                if first {
                    mark_rect(0, 0, ow as i32, oh as i32, &mut mark);
                } else if fi.TotalMetadataBufferSize > 0 {
                    let mut req = 0u32;
                    loop {
                        let sz = (moves.len() * std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>()) as u32;
                        match dup.GetFrameMoveRects(sz, moves.as_mut_ptr(), &mut req) {
                            Ok(()) => break,
                            Err(e) if e.code() == DXGI_ERROR_MORE_DATA => moves.resize(req as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>() + 1, Default::default()),
                            Err(e) => return Err(e),
                        }
                    }
                    let nm = req as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
                    for m in &moves[..nm] {
                        let d = m.DestinationRect;
                        mark_rect(d.left, d.top, d.right, d.bottom, &mut mark);
                    }
                    loop {
                        let sz = (rects.len() * std::mem::size_of::<RECT>()) as u32;
                        match dup.GetFrameDirtyRects(sz, rects.as_mut_ptr(), &mut req) {
                            Ok(()) => break,
                            Err(e) if e.code() == DXGI_ERROR_MORE_DATA => rects.resize(req as usize / std::mem::size_of::<RECT>() + 1, RECT::default()),
                            Err(e) => return Err(e),
                        }
                    }
                    let nd = req as usize / std::mem::size_of::<RECT>();
                    for r in &rects[..nd] {
                        mark_rect(r.left, r.top, r.right, r.bottom, &mut mark);
                    }
                }
                if !any {
                    dup.ReleaseFrame()?;
                    if cursor_changed {
                        self.publish(work, layout, layout_seq, tw, th, fi.LastPresentTime, t_acq);
                    }
                    continue;
                }
                let tex: ID3D11Texture2D = res.unwrap().cast()?;
                // Copy marked tile runs (clipped to output) into staging.
                for ty in t0y..t1y {
                    let mut tx = t0x;
                    while tx < t1x {
                        if !mark[(ty * tw + tx) as usize] {
                            tx += 1;
                            continue;
                        }
                        let s = tx;
                        while tx < t1x && mark[(ty * tw + tx) as usize] {
                            tx += 1;
                        }
                        let gx0 = (s * TS as u32).max(ox);
                        let gx1 = (tx * TS as u32).min(ox + ow);
                        let gy0 = (ty * TS as u32).max(oy);
                        let gy1 = ((ty + 1) * TS as u32).min(oy + oh);
                        let b = D3D11_BOX { left: gx0 - ox, top: gy0 - oy, front: 0, right: gx1 - ox, bottom: gy1 - oy, back: 1 };
                        ctx.CopySubresourceRegion(&staging, 0, b.left, b.top, 0, &tex, 0, Some(&b));
                    }
                }
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                if spin_map {
                    ctx.Flush();
                    loop {
                        match ctx.Map(&staging, 0, D3D11_MAP_READ, D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32, Some(&mut m)) {
                            Ok(()) => break,
                            Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => std::hint::spin_loop(),
                            Err(e) => return Err(e),
                        }
                    }
                } else {
                    ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
                }
                let copy_us = t_copy.elapsed().as_micros() as u64;
                let t_diff = std::time::Instant::now();
                let pitch = m.RowPitch as usize / 4;
                let base = m.pData as *const u32;
                let dirty_n;
                let mut changed_n = 0u64;
                let mut jobs: Vec<TileJob> = Vec::new();
                for ty in t0y..t1y {
                    for tx in t0x..t1x {
                        let ti = (ty * tw + tx) as usize;
                        if mark[ti] {
                            jobs.push(TileJob {
                                ti,
                                gx0: (tx * TS as u32).max(ox),
                                gx1: ((tx + 1) * TS as u32).min(ox + ow),
                                gy0: (ty * TS as u32).max(oy),
                                gy1: ((ty + 1) * TS as u32).min(oy + oh),
                                tx0: tx * TS as u32,
                                ty0: ty * TS as u32,
                            });
                        }
                    }
                }
                dirty_n = jobs.len() as u64;
                let src = Src { base, pitch, ox, oy };
                let olds: Vec<TileRef> = {
                    let w = work.lock();
                    jobs.iter().map(|j| w.tiles[j.ti].clone()).collect()
                };
                let results: Vec<Option<TileRef>> = if jobs.len() >= 8 {
                    use rayon::prelude::*;
                    pool().install(|| jobs.par_iter().zip(olds.par_iter()).map(|(j, o)| src.process(j, o)).collect())
                } else {
                    jobs.iter().zip(olds.iter()).map(|(j, o)| src.process(j, o)).collect()
                };
                {
                    let mut w = work.lock();
                    for ((j, old), new) in jobs.iter().zip(olds.iter()).zip(results) {
                        let cur = w.tiles[j.ti].clone();
                        let nt = if Arc::ptr_eq(&cur, old) { new } else { src.process(j, &cur) };
                        if let Some(nt) = nt {
                            changed_n += 1;
                            w.tiles[j.ti] = nt;
                        }
                    }
                }
                ctx.Unmap(&staging, 0);
                dup.ReleaseFrame()?;
                first = false;
                {
                    let mut s = self.stats.lock();
                    s.frames += 1;
                    s.copy_us += copy_us;
                    s.diff_us += t_diff.elapsed().as_micros() as u64;
                    s.dirty_tiles += dirty_n;
                    s.changed_tiles += changed_n;
                }
                if changed_n > 0 || cursor_changed {
                    self.publish(work, layout, layout_seq, tw, th, fi.LastPresentTime, t_acq);
                }
            }
            Ok(())
        }
    }

    fn publish(&self, work: &Mutex<Working>, layout: &Layout, layout_seq: u64, tw: u32, th: u32, present: i64, t_acq: u64) {
        let snap = {
            let mut w = work.lock();
            w.seq += 1;
            Snapshot {
                seq: w.seq, w: layout.w, h: layout.h, tw, th,
                tiles: w.tiles.clone(),
                cursor: w.cursor.clone(), cursor_seq: w.cursor_seq,
                cursor_pos: w.cursor_pos, cursor_visible: w.cursor_visible,
                layout_seq,
            }
        };
        if crate::probe::enabled() {
            crate::probe::captured(&snap, present, t_acq);
        }
        self.store.publish(snap);
    }
}

struct TileJob {
    ti: usize,
    gx0: u32,
    gx1: u32,
    gy0: u32,
    gy1: u32,
    tx0: u32,
    ty0: u32,
}

/// Mapped staging surface of one output.
struct Src {
    base: *const u32,
    pitch: usize,
    ox: u32,
    oy: u32,
}

unsafe impl Sync for Src {}

impl Src {
    /// New tile if the output area of `j` differs from `old` (row hashes precomputed).
    fn process(&self, j: &TileJob, old: &TileRef) -> Option<TileRef> {
        let lx = (j.gx0 - j.tx0) as usize;
        let n = (j.gx1 - j.gx0) as usize;
        let row = |gy: u32| unsafe { std::slice::from_raw_parts(self.base.add((gy - self.oy) as usize * self.pitch + (j.gx0 - self.ox) as usize), n) };
        let first = (j.gy0..j.gy1).find(|&gy| {
            let ly = (gy - j.ty0) as usize;
            !crate::simd::eq_masked(row(gy), &old.px[ly * TS + lx..ly * TS + lx + n])
        })?;
        let mut nt = old.clone_px();
        for gy in first..j.gy1 {
            let ly = (gy - j.ty0) as usize;
            crate::simd::copy_masked(&mut nt.px[ly * TS + lx..ly * TS + lx + n], row(gy));
        }
        nt.hashes();
        Some(Arc::new(nt))
    }
}

/// High-priority pool, separate from encoding.
fn pool() -> &'static rayon::ThreadPool {
    static P: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let n = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).clamp(2, 6);
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("capture-pool-{i}"))
            .start_handler(|_| set_mmcss("Capture"))
            .build()
            .unwrap()
    })
}

fn select(monitor: Option<usize>) -> Vec<OutputInfo> {
    let all = enum_outputs();
    match monitor {
        Some(i) if i < all.len() => vec![all[i].clone()],
        _ => all,
    }
}

pub fn set_mmcss(task: &str) {
    use windows::core::HSTRING;
    use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
    let mut idx = 0u32;
    unsafe {
        let _ = AvSetMmThreadCharacteristicsW(&HSTRING::from(task), &mut idx);
    }
}

fn convert_shape(buf: &[u8], si: &DXGI_OUTDUPL_POINTER_SHAPE_INFO) -> Option<CursorShape> {
    let (w, pitch) = (si.Width as usize, si.Pitch as usize);
    let ty = si.Type as i32;
    let h = if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 { si.Height as usize / 2 } else { si.Height as usize };
    if w == 0 || h == 0 || w > 256 || h > 256 {
        return None;
    }
    let mut argb = vec![0u32; w * h];
    // Inverted pixels become black; transparent neighbours get a white halo.
    let mut invert = vec![false; w * h];
    if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
        for y in 0..h {
            for x in 0..w {
                let bit = 0x80 >> (x % 8);
                let and = buf[y * pitch + x / 8] & bit != 0;
                let xor = buf[(y + h) * pitch + x / 8] & bit != 0;
                argb[y * w + x] = match (and, xor) {
                    (false, false) => 0xFF00_0000,
                    (false, true) => 0xFFFF_FFFF,
                    (true, false) => 0,
                    (true, true) => {
                        invert[y * w + x] = true;
                        0xFF00_0000
                    }
                };
            }
        }
    } else {
        let masked = ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0;
        for y in 0..h {
            for x in 0..w {
                let o = y * pitch + x * 4;
                let p = u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
                argb[y * w + x] = if masked {
                    if p >> 24 == 0 {
                        p | 0xFF00_0000
                    } else if p & 0xFF_FFFF == 0 {
                        0
                    } else {
                        invert[y * w + x] = true;
                        0xFF00_0000
                    }
                } else {
                    p
                };
            }
        }
    }
    if invert.iter().any(|&b| b) {
        for y in 0..h {
            for x in 0..w {
                if argb[y * w + x] >> 24 != 0 {
                    continue;
                }
                let near = (y.saturating_sub(1)..(y + 2).min(h)).any(|yy| (x.saturating_sub(1)..(x + 2).min(w)).any(|xx| invert[yy * w + xx]));
                if near {
                    argb[y * w + x] = 0xFFFF_FFFF;
                }
            }
        }
    }
    Some(CursorShape { w: w as u16, h: h as u16, hot_x: si.HotSpot.x as u16, hot_y: si.HotSpot.y as u16, argb })
}
