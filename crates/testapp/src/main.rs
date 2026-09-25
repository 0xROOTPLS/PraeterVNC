//! Test pattern generator: renders scenarios with a frame-id barcode and logs
//! composition timestamps (QPC) to shared memory for latency measurement.
use std::time::{Duration, Instant};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const MAGIC: u64 = 0x5052_4145_5445_5231;
pub const RING: usize = 65536;
pub const CELL: i32 = 16;
pub const BITS: i32 = 32;

#[repr(C)]
pub struct Shm {
    pub magic: u64,
    pub bar_x: i32,
    pub bar_y: i32,
    pub frames: u64,
    pub ts: [u64; RING],
    pub win: [i32; 4],
    pub final_ready: u32,
    pub _pad: u32,
}

const MAX_PX: usize = 3840 * 2160;

pub fn code(id: u32) -> u32 {
    let id = id & 0xFF_FFFF;
    id | ((id.wrapping_mul(0x9E37_79B1) >> 24) << 24)
}

fn qpc() -> u64 {
    let mut v = 0i64;
    unsafe { let _ = QueryPerformanceCounter(&mut v); }
    v as u64
}

struct Canvas {
    w: i32,
    h: i32,
    dc: HDC,
    px: *mut u32,
}

impl Canvas {
    fn new(w: i32, h: i32) -> Canvas {
        unsafe {
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, ..Default::default() },
                ..Default::default()
            };
            let dc = CreateCompatibleDC(None);
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            SelectObject(dc, bmp.into());
            Canvas { w, h, dc, px: bits as *mut u32 }
        }
    }
    fn pixels(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.px, (self.w * self.h) as usize) }
    }
    fn fill(&mut self, c: u32) {
        self.pixels().fill(c);
    }
    fn text(&self, x: i32, y: i32, s: &str, color: u32, font: HFONT) {
        unsafe {
            SelectObject(self.dc, font.into());
            SetBkMode(self.dc, TRANSPARENT);
            SetTextColor(self.dc, COLORREF(color));
            let w: Vec<u16> = s.encode_utf16().collect();
            let _ = TextOutW(self.dc, x, y, &w);
        }
    }
}

fn font(h: i32, mono: bool) -> HFONT {
    unsafe {
        let face = if mono { w!("Consolas") } else { w!("Segoe UI") };
        CreateFontW(h, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, face)
    }
}

const WORDS: &[&str] = &["the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "latency", "framebuffer", "encoder", "tile", "render",
    "pixel", "compress", "stream", "network", "vector", "cursor", "window", "scroll", "update", "region", "palette", "delta"];

fn lcg(s: &mut u32) -> u32 {
    *s = s.wrapping_mul(1103515245).wrapping_add(12345);
    *s >> 8
}

fn line(seed: &mut u32, n: usize) -> String {
    (0..n).map(|_| WORDS[lcg(seed) as usize % WORDS.len()]).collect::<Vec<_>>().join(" ")
}

fn load_photo(w: i32, h: i32) -> Vec<u32> {
    let img = image::open(r"C:\Windows\Web\Screen\img100.jpg").map(|i| i.to_rgb8()).ok();
    let (iw, ih) = img.as_ref().map(|i| (i.width() as i32, i.height() as i32)).unwrap_or((w, h));
    let mut v = vec![0u32; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            v[(y * w + x) as usize] = match &img {
                Some(i) => {
                    let p = i.get_pixel((x * iw / w) as u32, (y * ih / h) as u32);
                    (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32
                }
                None => ((x ^ y) as u32 & 255) * 0x010101,
            };
        }
    }
    v
}

/// Ken Burns frames: zoom/pan over the photo; every pixel changes per frame.
fn video_frames(w: i32, h: i32, n: usize) -> Vec<Vec<u32>> {
    let (pw, ph) = (w * 3 / 2, h * 3 / 2);
    let src = load_photo(pw, ph);
    (0..n).map(|k| {
        let t = k as f32 / n as f32;
        let s = 1.0 - 0.25 * (t * std::f32::consts::TAU).sin().abs();
        let ox = (pw as f32 - w as f32 * s) * (0.5 + 0.4 * (t * std::f32::consts::TAU).cos());
        let oy = (ph as f32 - h as f32 * s) * (0.5 + 0.4 * (t * std::f32::consts::TAU).sin());
        let mut f = vec![0u32; (w * h) as usize];
        for y in 0..h {
            let sy = ((oy + y as f32 * s) as i32).clamp(0, ph - 1);
            for x in 0..w {
                let sx = ((ox + x as f32 * s) as i32).clamp(0, pw - 1);
                f[(y * w + x) as usize] = src[(sy * pw + sx) as usize];
            }
        }
        f
    }).collect()
}

/// Flip-model presenter; display times come from swap-chain frame statistics.
struct Flip {
    ctx: ID3D11DeviceContext,
    sc: IDXGISwapChain1,
    wait: HANDLE,
    w: i32,
}

impl Flip {
    unsafe fn new(hwnd: HWND, w: i32, h: i32) -> Flip {
        unsafe {
            let mut dev = None;
            let mut ctx = None;
            D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)).unwrap();
            let dev: ID3D11Device = dev.unwrap();
            let dxgi: IDXGIDevice = dev.cast().unwrap();
            let f: IDXGIFactory2 = dxgi.GetAdapter().unwrap().GetParent().unwrap();
            let d = DXGI_SWAP_CHAIN_DESC1 {
                Width: w as u32, Height: h as u32, Format: DXGI_FORMAT_B8G8R8A8_UNORM, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT, BufferCount: 2, Scaling: DXGI_SCALING_NONE, SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE, Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32, ..Default::default()
            };
            let sc = f.CreateSwapChainForHwnd(&dev, hwnd, &d, None, None).unwrap();
            let sc2: IDXGISwapChain2 = sc.cast().unwrap();
            let _ = sc2.SetMaximumFrameLatency(1);
            let wait = sc2.GetFrameLatencyWaitableObject();
            Flip { ctx: ctx.unwrap(), sc, wait, w }
        }
    }

    unsafe fn present(&mut self, px: &[u32], dirty: RECT) {
        unsafe {
            WaitForSingleObject(self.wait, 1000);
            let bb: ID3D11Texture2D = self.sc.GetBuffer(0).unwrap();
            self.ctx.UpdateSubresource(&bb, 0, None, px.as_ptr() as *const _, (self.w * 4) as u32, 0);
            let mut r = [dirty];
            let p = DXGI_PRESENT_PARAMETERS { DirtyRectsCount: 1, pDirtyRects: r.as_mut_ptr(), pScrollRect: std::ptr::null_mut(), pScrollOffset: std::ptr::null_mut() };
            let _ = self.sc.Present1(1, DXGI_PRESENT(0), &p);
        }
    }
}

/// Observer: own Desktop Duplication session reading the barcode; records the compositor's
/// LastPresentTime per frame id.
fn observer(bar: (i32, i32), shm: usize, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use windows::core::Interface;
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let shm = &mut *(shm as *mut Shm);
        let f: IDXGIFactory1 = CreateDXGIFactory1().unwrap();
        let mut found = None;
        let mut ai = 0;
        while let Ok(a) = f.EnumAdapters1(ai) {
            let mut oi = 0;
            while let Ok(o) = a.EnumOutputs(oi) {
                let r = o.GetDesc().unwrap().DesktopCoordinates;
                if bar.0 >= r.left && bar.0 < r.right && bar.1 >= r.top && bar.1 < r.bottom {
                    found = Some((a.clone(), o, r));
                }
                oi += 1;
            }
            ai += 1;
        }
        let (a, o, r) = found.expect("no output for window");
        let mut dev = None;
        let mut ctx = None;
        D3D11CreateDevice(&a, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)).unwrap();
        let (dev, ctx): (ID3D11Device, ID3D11DeviceContext) = (dev.unwrap(), ctx.unwrap());
        let dup = o.cast::<IDXGIOutput1>().unwrap().DuplicateOutput(&dev).unwrap();
        let (bw, bh) = ((BITS * CELL) as u32, CELL as u32);
        let sd = D3D11_TEXTURE2D_DESC {
            Width: bw, Height: bh, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0,
        };
        let mut st = None;
        dev.CreateTexture2D(&sd, None, Some(&mut st)).unwrap();
        let st = st.unwrap();
        let (lx, ly) = ((bar.0 - r.left) as u32, (bar.1 - r.top) as u32);
        let bx = D3D11_BOX { left: lx, top: ly, front: 0, right: lx + bw, bottom: ly + bh, back: 1 };
        let mut last = 0u32;
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let mut fi = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res = None;
            if dup.AcquireNextFrame(50, &mut fi, &mut res).is_err() {
                continue;
            }
            if fi.LastPresentTime != 0 {
                let tex: ID3D11Texture2D = res.unwrap().cast().unwrap();
                ctx.CopySubresourceRegion(&st, 0, 0, 0, 0, &tex, 0, Some(&bx));
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                if ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).is_ok() {
                    let row = std::slice::from_raw_parts((m.pData as *const u8).add(m.RowPitch as usize * (CELL / 2) as usize), (bw * 4) as usize);
                    let mut v = 0u32;
                    for b in 0..BITS {
                        if row[((b * CELL + CELL / 2) * 4 + 1) as usize] > 128 {
                            v |= 1 << b;
                        }
                    }
                    ctx.Unmap(&st, 0);
                    let id = v & 0xFF_FFFF;
                    if code(id) == v && id != last {
                        last = id;
                        shm.ts[(id & 0xFFFF) as usize] = fi.LastPresentTime as u64;
                    }
                }
            }
            let _ = dup.ReleaseFrame();
        }
    }
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str, d: &str| -> String {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned().unwrap_or(d.to_string())
    };
    let scenario = get("--scenario", "ticker");
    let (x, y) = (get("--x", "100").parse::<i32>().unwrap(), get("--y", "100").parse::<i32>().unwrap());
    let (w, h) = (get("--w", "1280").parse::<i32>().unwrap(), get("--h", "720").parse::<i32>().unwrap());
    let fps: f64 = get("--fps", "0").parse().unwrap();
    let secs: f64 = get("--secs", "10").parse().unwrap();
    let speed: i32 = get("--speed", "8").parse().unwrap();
    let hold: f64 = get("--hold", "0").parse().unwrap();
    let gdi = args.iter().any(|a| a == "--gdi");
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let map = CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, (std::mem::size_of::<Shm>() + MAX_PX * 4) as u32, w!("Local\\PraeterTestApp")).unwrap();
        let view = MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, 0);
        let shm = &mut *(view.Value as *mut Shm);
        shm.magic = 0;
        shm.frames = 0;
        shm.final_ready = 0;
        let final_px = std::slice::from_raw_parts_mut((view.Value as *mut u8).add(std::mem::size_of::<Shm>()) as *mut u32, MAX_PX);

        let inst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: w!("PraeterTestApp"), hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(), ..Default::default() };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, w!("PraeterTestApp"), w!("PraeterTestApp"), WS_POPUP | WS_VISIBLE, x, y, w, h, None, None, Some(inst.into()), None).unwrap();
        let wdc = GetDC(Some(hwnd));
        let mut c = Canvas::new(w, h);
        let f_ui = font(20, false);
        let f_mono = font(18, true);
        let mut flip = if gdi { None } else { Some(Flip::new(hwnd, w, h)) };

        // Scenario assets.
        let mut seed = 7u32;
        let doc_h = 12000;
        let doc = if scenario == "scroll" || scenario == "drag" {
            let d = Canvas::new(w, doc_h);
            let p = std::slice::from_raw_parts_mut(d.px, (w * doc_h) as usize);
            p.fill(0xFFFFFF);
            let mut yy = 30;
            while yy < doc_h - 30 {
                let n = 6 + lcg(&mut seed) as usize % 10;
                d.text(24, yy, &line(&mut seed, n), 0x202020, f_ui);
                yy += 26;
                if lcg(&mut seed) % 17 == 0 {
                    for r in yy + 4..yy + 40 {
                        p[(r * w + 24) as usize..(r * w + 220) as usize].fill(0x0067C0);
                    }
                    d.text(60, yy + 12, "Button", 0xFFFFFF, f_ui);
                    yy += 50;
                }
            }
            Some(d)
        } else {
            None
        };
        let frames = if scenario == "video" { video_frames(w, h, 30) } else { Vec::new() };
        let panel = if scenario == "drag" { load_photo(400, 300) } else { Vec::new() };

        c.fill(if scenario == "typing" { 0x1E1E1E } else { 0xF0F0F0 });
        match flip.as_mut() {
            Some(f) => {
                f.present(c.pixels(), RECT { left: 0, top: 0, right: w, bottom: h });
                f.present(c.pixels(), RECT { left: 0, top: 0, right: w, bottom: h });
            }
            None => {
                let _ = BitBlt(wdc, 0, 0, w, h, Some(c.dc), 0, 0, SRCCOPY);
                let _ = DwmFlush();
            }
        }

        let (bx, by) = (8, 8);
        let mut pt = POINT { x: bx, y: by };
        let _ = ClientToScreen(hwnd, &mut pt);
        shm.bar_x = pt.x;
        shm.bar_y = pt.y;
        let mut org = POINT { x: 0, y: 0 };
        let _ = ClientToScreen(hwnd, &mut org);
        shm.win = [org.x, org.y, w, h];
        shm.magic = MAGIC;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let obs = if gdi {
            None
        } else {
            let (st2, sp) = (stop.clone(), shm as *mut Shm as usize);
            let b = (pt.x, pt.y);
            Some(std::thread::spawn(move || observer(b, sp, st2)))
        };
        std::thread::sleep(Duration::from_millis(100));

        let start = Instant::now();
        let mut id: u32 = 0;
        let mut scroll_y = 0i32;
        let mut type_x = 20;
        let mut type_y = 40;
        let mut drag_prev = RECT::default();
        let mut next = Instant::now();
        let mut msg = MSG::default();
        while start.elapsed().as_secs_f64() < secs {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
            if fps > 0.0 {
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
                next += Duration::from_secs_f64(1.0 / fps);
            }
            id += 1;
            // Content; dirty rect in window coords.
            let dirty: RECT = match scenario.as_str() {
                "scroll" => {
                    let d = doc.as_ref().unwrap();
                    scroll_y = (scroll_y + speed) % (doc_h - h);
                    let src = std::slice::from_raw_parts(d.px.add((scroll_y * w) as usize), (w * h) as usize);
                    c.pixels().copy_from_slice(src);
                    RECT { left: 0, top: 0, right: w, bottom: h }
                }
                "video" => {
                    let k = id as usize % (2 * frames.len());
                    let k = if k < frames.len() { k } else { 2 * frames.len() - 1 - k };
                    c.pixels().copy_from_slice(&frames[k]);
                    RECT { left: 0, top: 0, right: w, bottom: h }
                }
                "typing" => {
                    let chs = b"abcdefghijklmnopqrstuvwxyz ";
                    let ch = chs[lcg(&mut seed) as usize % chs.len()] as char;
                    c.text(type_x, type_y, &ch.to_string(), 0xDCDCDC, f_mono);
                    let r = RECT { left: type_x, top: type_y, right: type_x + 10, bottom: type_y + 20 };
                    type_x += 10;
                    if type_x > w - 30 {
                        type_x = 20;
                        type_y += 22;
                        if type_y > h - 30 {
                            type_y = 40;
                            c.fill(0x1E1E1E);
                        }
                    }
                    if type_y == 40 && type_x == 30 {
                        RECT { left: 0, top: 0, right: w, bottom: h }
                    } else {
                        r
                    }
                }
                "drag" => {
                    let d = doc.as_ref().unwrap();
                    if id == 1 {
                        let src = std::slice::from_raw_parts(d.px, (w * h) as usize);
                        c.pixels().copy_from_slice(src);
                    }
                    let t = id as f32 * 0.02;
                    let px = ((w - 420) as f32 * (0.5 + 0.45 * t.cos())) as i32 + 10;
                    let py = ((h - 330) as f32 * (0.5 + 0.45 * (t * 1.3).sin())) as i32 + 30;
                    let nr = RECT { left: px, top: py, right: px + 400, bottom: py + 300 };
                    // Restore background under the old panel, draw new one.
                    let pr = drag_prev;
                    let dp = std::slice::from_raw_parts(d.px, (w * h) as usize);
                    let cp = c.pixels();
                    for yy in pr.top..pr.bottom {
                        let o = (yy * w) as usize;
                        cp[o + pr.left as usize..o + pr.right as usize].copy_from_slice(&dp[o + pr.left as usize..o + pr.right as usize]);
                    }
                    for yy in 0..300 {
                        let o = ((py + yy) * w + px) as usize;
                        cp[o..o + 400].copy_from_slice(&panel[(yy * 400) as usize..(yy * 400 + 400) as usize]);
                    }
                    drag_prev = nr;
                    if id == 1 {
                        RECT { left: 0, top: 0, right: w, bottom: h }
                    } else {
                        RECT { left: pr.left.min(nr.left), top: pr.top.min(nr.top), right: pr.right.max(nr.right), bottom: pr.bottom.max(nr.bottom) }
                    }
                }
                _ => {
                    let msg = format!("frame {id}");
                    let p = c.pixels();
                    for yy in 30..60 {
                        p[(yy * w + 8) as usize..(yy * w + 200) as usize].fill(0xF0F0F0);
                    }
                    c.text(10, 32, &msg, 0x000000, f_ui);
                    RECT { left: 0, top: 0, right: 200, bottom: 60 }
                }
            };
            // Barcode.
            let v = code(id);
            let p = c.pixels();
            for b in 0..BITS {
                let col = if v >> b & 1 != 0 { 0xFFFFFF } else { 0x000000 };
                for yy in by..by + CELL {
                    let o = (yy * w + bx + b * CELL) as usize;
                    p[o..o + CELL as usize].fill(col);
                }
            }
            let bar = RECT { left: bx, top: by, right: bx + BITS * CELL, bottom: by + CELL };
            let u = RECT { left: dirty.left.min(bar.left), top: dirty.top.min(bar.top), right: dirty.right.max(bar.right), bottom: dirty.bottom.max(bar.bottom) };
            shm.ts[(id & 0xFFFF) as usize] = 0;
            match flip.as_mut() {
                Some(f) => f.present(c.pixels(), u),
                None => {
                    let _ = BitBlt(wdc, u.left, u.top, u.right - u.left, u.bottom - u.top, Some(c.dc), u.left, u.top, SRCCOPY);
                    let _ = GdiFlush();
                    let _ = DwmFlush();
                    shm.ts[(id & 0xFFFF) as usize] = qpc();
                }
            }
            shm.frames = id as u64;
        }
        if hold > 0.0 {
            final_px[..(w * h) as usize].copy_from_slice(c.pixels());
            shm.final_ready = 1;
            let t = Instant::now();
            while t.elapsed().as_secs_f64() < hold {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&msg);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(o) = obs {
            let _ = o.join();
        }
        shm.magic = 0;
        println!("frames={} fps={:.1}", id, id as f64 / start.elapsed().as_secs_f64());
    }
}
