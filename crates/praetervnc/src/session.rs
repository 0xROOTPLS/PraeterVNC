//! Per-client RFB session: handshake, reader thread (input); updates are in writer.rs.
use crate::capture::Capture;
use crate::server::Server;
use crate::enc::{Kind, Rect, Subsamp};
use crate::fb::Signal;
use crate::pixfmt::PixelFormat;
use crate::rfb::{self, enc as E};
use parking_lot::Mutex;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct State {
    pub pf: Option<PixelFormat>,
    pub encodings: Option<Vec<i32>>,
    /// Pending FramebufferUpdateRequest credits.
    pub requests: u32,
    /// Monotonic count of FramebufferUpdateRequests received.
    pub req_total: u64,
    pub full: Vec<Rect>,
    /// Some(Some(r)) enable, Some(None) disable.
    pub cu_change: Option<Option<Rect>>,
    pub fences: Vec<(u32, Vec<u8>)>,
    pub fence_acks: Vec<Vec<u8>>,
    pub closed: bool,
    pub last_ptr: (i32, i32),
    pub ptr_time: Option<Instant>,
    /// ExtendedClipboard replies queued by the reader.
    pub clip_out: Vec<Vec<u8>>,
    /// Client clipboard flags and max unsolicited text size.
    pub clip_caps: (u32, u32),
}

pub struct Shared {
    pub st: Mutex<State>,
    pub sig: Arc<Signal>,
    /// Unique per session, nonzero.
    pub id: u64,
}

#[derive(Default)]
pub struct Caps {
    pub kind_order: Vec<Kind>,
    pub jpeg_level: Option<u8>,
    pub fine_q: Option<u8>,
    pub subsamp: Option<Subsamp>,
    pub compress: Option<u8>,
    pub last_rect: bool,
    pub rich_cursor: bool,
    pub alpha_cursor: bool,
    pub pointer_pos: bool,
    pub desktop_size: bool,
    pub ext_desktop_size: bool,
    pub ext_clip: bool,
    pub fence: bool,
    pub cu: bool,
    pub qemu_key: bool,
    pub copyrect: bool,
    pub jpeg21: bool,
}

impl Caps {
    pub fn parse(list: &[i32]) -> Caps {
        let mut c = Caps::default();
        for &e in list {
            match e {
                E::TIGHT => c.kind_order.push(Kind::Tight),
                E::ZRLE => c.kind_order.push(Kind::Zrle),
                E::TRLE => c.kind_order.push(Kind::Trle),
                E::JPEG => c.jpeg21 = true,
                E::ZLIB => c.kind_order.push(Kind::Zlib),
                E::HEXTILE => c.kind_order.push(Kind::Hextile),
                E::RAW => c.kind_order.push(Kind::Raw),
                E::COPYRECT => c.copyrect = true,
                E::JPEG_Q0..=E::JPEG_Q9 => c.jpeg_level = Some((e - E::JPEG_Q0) as u8),
                E::FINE_Q0..=E::FINE_Q100 => c.fine_q = Some((e - E::FINE_Q0) as u8),
                E::SUBSAMP_1X..=E::SUBSAMP_GRAY => {
                    c.subsamp = Some(match e - E::SUBSAMP_1X {
                        0 => Subsamp::None,
                        1 => Subsamp::Sub2x2,
                        2 => Subsamp::Sub2x1,
                        3 => Subsamp::Gray,
                        4 => Subsamp::Sub2x2,
                        _ => Subsamp::Gray,
                    })
                }
                E::COMPRESS_0..=E::COMPRESS_9 => c.compress = Some((e - E::COMPRESS_0) as u8),
                E::LAST_RECT => c.last_rect = true,
                E::CURSOR => c.rich_cursor = true,
                E::CURSOR_ALPHA => c.alpha_cursor = true,
                E::POINTER_POS => c.pointer_pos = true,
                E::DESKTOP_SIZE => c.desktop_size = true,
                E::EXT_DESKTOP_SIZE => c.ext_desktop_size = true,
                E::EXT_CLIPBOARD => c.ext_clip = true,
                E::FENCE => c.fence = true,
                E::CONTINUOUS_UPDATES => c.cu = true,
                E::QEMU_EXT_KEY => c.qemu_key = true,
                _ => {}
            }
        }
        c
    }

    pub fn kind(&self) -> Kind {
        self.kind_order.first().copied().unwrap_or(Kind::Raw)
    }

    pub fn jpeg(&self) -> Option<(u8, Subsamp)> {
        const Q: [(u8, Subsamp); 10] = [
            (15, Subsamp::Sub2x2), (29, Subsamp::Sub2x2), (41, Subsamp::Sub2x2), (42, Subsamp::Sub2x1), (62, Subsamp::Sub2x1),
            (77, Subsamp::Sub2x1), (79, Subsamp::None), (86, Subsamp::None), (92, Subsamp::None), (100, Subsamp::None),
        ];
        let base = if let Some(q) = self.fine_q {
            (q.max(1), self.subsamp.unwrap_or(Subsamp::Sub2x2))
        } else {
            let l = self.jpeg_level?;
            let (q, s) = Q[l.min(9) as usize];
            (q, self.subsamp.unwrap_or(s))
        };
        Some(base)
    }

    pub fn zlevel(&self) -> i32 {
        // Client levels are hints.
        match self.compress.unwrap_or(1) {
            0 | 1 => 1,
            2..=4 => 2,
            5..=6 => 3,
            _ => 6,
        }
    }
}

fn rd<const N: usize>(s: &mut impl Read) -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    s.read_exact(&mut b)?;
    Ok(b)
}
fn rd_u8(s: &mut impl Read) -> io::Result<u8> {
    Ok(rd::<1>(s)?[0])
}
fn rd_u16(s: &mut impl Read) -> io::Result<u16> {
    Ok(u16::from_be_bytes(rd::<2>(s)?))
}
fn rd_u32(s: &mut impl Read) -> io::Result<u32> {
    Ok(u32::from_be_bytes(rd::<4>(s)?))
}
fn skip(s: &mut impl Read, n: usize) -> io::Result<()> {
    io::copy(&mut s.take(n as u64), &mut io::sink()).map(|_| ())
}

pub fn handle(mut sock: TcpStream, srv: &Arc<Server>) -> io::Result<()> {
    let (cfg, cap) = (&srv.cfg, &srv.cap);
    let addr = sock.peer_addr().ok();
    let peer = addr.map(|a| a.to_string()).unwrap_or_default();
    sock.set_nodelay(true)?;
    crate::net::tune(&sock);
    sock.set_read_timeout(Some(Duration::from_secs(30)))?;
    handshake(&mut sock, srv, addr.map(|a| a.ip())).map_err(|e| io::Error::new(e.kind(), format!("{peer}: {e}")))?;
    sock.set_read_timeout(None)?;
    let snap = cap.store.latest();
    // ServerInit
    let mut o = Vec::new();
    o.extend_from_slice(&(snap.w as u16).to_be_bytes());
    o.extend_from_slice(&(snap.h as u16).to_be_bytes());
    PixelFormat::NATIVE.write(&mut o);
    let name = cfg.name.as_bytes();
    o.extend_from_slice(&(name.len() as u32).to_be_bytes());
    o.extend_from_slice(name);
    sock.write_all(&o)?;
    crate::log!("{peer}: connected");

    let sh = Arc::new(Shared { st: Mutex::new(State { clip_caps: (crate::clipboard::DEFAULT_CLIENT_FLAGS, 20 << 20), ..Default::default() }), sig: Arc::new(Signal::default()), id: {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    } });
    cap.store.subscribe(sh.sig.clone());
    let rsock = sock.try_clone()?;
    let (sh2, srv2, peer2) = (sh.clone(), srv.clone(), peer.clone());
    let reader = std::thread::Builder::new().name("rfb-reader".into()).spawn(move || {
        let r = reader_loop(rsock, &sh2, &srv2.cap, &srv2.view_only);
        if let Err(e) = r {
            if e.kind() != io::ErrorKind::UnexpectedEof {
                crate::log!("{peer2}: read: {e}");
            }
        }
        sh2.st.lock().closed = true;
        sh2.sig.notify();
    })?;
    srv.add_client(sh.id, &peer, &sock);
    let r = crate::writer::Writer::new(cfg, cap, &sh, &peer, sock).run();
    sh.st.lock().closed = true;
    cap.store.unsubscribe(&sh.sig);
    let _ = reader.join();
    srv.remove_client(sh.id);
    crate::log!("{peer}: disconnected");
    r
}

/// Security handshake failure with a reason (empty security type list).
fn refuse(s: &mut TcpStream, minor: u32, why: &str) -> io::Result<()> {
    s.write_all(if minor == 3 { &[0, 0, 0, 0][..] } else { &[0] })?;
    s.write_all(&(why.len() as u32).to_be_bytes())?;
    s.write_all(why.as_bytes())
}

fn handshake(s: &mut TcpStream, srv: &Server, ip: Option<IpAddr>) -> io::Result<()> {
    s.write_all(b"RFB 003.008\n")?;
    let v = rd::<12>(s)?;
    let minor = std::str::from_utf8(&v[8..11]).ok().and_then(|m| m.parse::<u32>().ok()).unwrap_or(3);
    let minor = if minor >= 8 { 8 } else if minor == 7 { 7 } else { 3 };
    let set = srv.settings();
    if let Some(left) = ip.and_then(|ip| srv.guard.blocked(ip)) {
        refuse(s, minor, "Too many authentication failures")?;
        return Err(io::Error::other(format!("refused, blocked for {} s", left.as_secs() + 1)));
    }
    let auth = set.password.is_some();
    let sec: u8 = if auth { 2 } else { 1 };
    if minor == 3 {
        s.write_all(&(sec as u32).to_be_bytes())?;
    } else {
        s.write_all(&[1, sec])?;
        let chosen = rd_u8(s)?;
        if chosen != sec {
            return Err(io::Error::other("bad security type"));
        }
    }
    if auth {
        let ch: [u8; 16] = rfb::random_bytes();
        s.write_all(&ch)?;
        let resp = rd::<16>(s)?;
        let ok = rfb::vnc_auth_response(set.password.as_deref().unwrap(), &ch) == resp;
        // A block that began while this attempt was pending fails it too.
        let ok = ok && ip.is_none_or(|ip| srv.guard.blocked(ip).is_none());
        if let Some(ip) = ip {
            if ok {
                srv.guard.ok(ip);
            } else if srv.guard.failed(ip, set.max_attempts, Duration::from_secs(set.block_secs as u64)) {
                crate::log!("{ip}: blocked for {} s after {} failed logins", set.block_secs, set.max_attempts);
                srv.changed();
            }
        }
        if !ok {
            let mut m = 1u32.to_be_bytes().to_vec();
            if minor == 8 {
                let r = b"Authentication failed";
                m.extend_from_slice(&(r.len() as u32).to_be_bytes());
                m.extend_from_slice(r);
            }
            let _ = s.write_all(&m);
            return Err(io::Error::other("authentication failed"));
        }
        s.write_all(&0u32.to_be_bytes())?;
    } else if minor == 8 {
        s.write_all(&0u32.to_be_bytes())?;
    }
    let _shared = rd_u8(s)?;
    Ok(())
}

fn reader_loop(mut s: TcpStream, sh: &Shared, cap: &Capture, view_only: &AtomicBool) -> io::Result<()> {
    let mut inj = crate::input::Injector::default();
    let s = &mut s;
    let res = (|| -> io::Result<()> {
        loop {
            let t = rd_u8(s)?;
            match t {
                0 => {
                    skip(s, 3)?;
                    let pf = PixelFormat::parse(&rd::<16>(s)?);
                    if !pf.valid() {
                        return Err(io::Error::other("invalid pixel format"));
                    }
                    sh.st.lock().pf = Some(pf);
                }
                2 => {
                    skip(s, 1)?;
                    let n = rd_u16(s)? as usize;
                    let mut v = Vec::with_capacity(n);
                    for _ in 0..n {
                        v.push(rd_u32(s)? as i32);
                    }
                    sh.st.lock().encodings = Some(v);
                }
                3 => {
                    let inc = rd_u8(s)?;
                    let r = Rect { x: rd_u16(s)?, y: rd_u16(s)?, w: rd_u16(s)?, h: rd_u16(s)? };
                    let mut st = sh.st.lock();
                    st.requests = st.requests.saturating_add(1);
                    st.req_total += 1;
                    if inc == 0 {
                        st.full.push(r);
                    }
                }
                4 => {
                    let down = rd_u8(s)? != 0;
                    skip(s, 2)?;
                    let ks = rd_u32(s)?;
                    if !view_only.load(Ordering::Relaxed) {
                        inj.keysym(down, ks);
                    }
                    continue;
                }
                5 => {
                    let mask = rd_u8(s)? as u16;
                    let x = rd_u16(s)? as i32;
                    let y = rd_u16(s)? as i32;
                    if !view_only.load(Ordering::Relaxed) {
                        let origin = cap.layout.lock().origin;
                        inj.pointer(mask, x, y, origin);
                        let mut st = sh.st.lock();
                        st.last_ptr = (x, y);
                        st.ptr_time = Some(Instant::now());
                    }
                    continue;
                }
                6 => {
                    skip(s, 3)?;
                    let len = rd_u32(s)? as i32;
                    let n = len.unsigned_abs() as usize;
                    if n > 64 << 20 {
                        return Err(io::Error::other("cut text too large"));
                    }
                    let mut b = vec![0u8; n];
                    s.read_exact(&mut b)?;
                    if len >= 0 {
                        if !view_only.load(Ordering::Relaxed) {
                            crate::clipboard::set_latin1(&b, sh.id);
                        }
                    } else if n >= 4 {
                        ext_clipboard(sh, u32::from_be_bytes([b[0], b[1], b[2], b[3]]), &b[4..], view_only.load(Ordering::Relaxed));
                    }
                    continue;
                }
                150 => {
                    let en = rd_u8(s)? != 0;
                    let r = Rect { x: rd_u16(s)?, y: rd_u16(s)?, w: rd_u16(s)?, h: rd_u16(s)? };
                    sh.st.lock().cu_change = Some(en.then_some(r));
                }
                248 => {
                    skip(s, 3)?;
                    let flags = rd_u32(s)?;
                    let len = rd_u8(s)? as usize;
                    let mut p = vec![0u8; len];
                    s.read_exact(&mut p)?;
                    let mut st = sh.st.lock();
                    if flags & 0x8000_0000 != 0 {
                        st.fences.push((flags, p));
                    } else {
                        st.fence_acks.push(p);
                    }
                }
                251 => {
                    skip(s, 1)?;
                    let _w = rd_u16(s)?;
                    let _h = rd_u16(s)?;
                    let n = rd_u8(s)? as usize;
                    skip(s, 1 + n * 16)?;
                }
                255 => {
                    let sub = rd_u8(s)?;
                    if sub != 0 {
                        return Err(io::Error::other("unsupported QEMU message"));
                    }
                    let down = rd_u16(s)? != 0;
                    let ks = rd_u32(s)?;
                    let kc = rd_u32(s)?;
                    if !view_only.load(Ordering::Relaxed) {
                        inj.scancode(down, kc, ks);
                    }
                    continue;
                }
                _ => return Err(io::Error::other(format!("unknown message {t}"))),
            }
            sh.sig.notify();
        }
    })();
    inj.release_all();
    res
}


/// Client ExtendedClipboard message (text format only).
fn ext_clipboard(sh: &Shared, flags: u32, body: &[u8], view_only: bool) {
    use crate::clipboard as c;
    let mut st = sh.st.lock();
    if flags & c::CAPS != 0 {
        let size = if flags & c::TEXT_FMT != 0 && body.len() >= 4 { u32::from_be_bytes([body[0], body[1], body[2], body[3]]) } else { 0 };
        st.clip_caps = (flags, size);
    } else if flags & c::REQUEST != 0 && flags & c::TEXT_FMT != 0 {
        if let Some(t) = c::current().1 {
            st.clip_out.push(c::ext_provide(&t));
        }
    } else if flags & c::PEEK != 0 {
        let f = if c::current().1.is_some() { c::TEXT_FMT } else { 0 };
        st.clip_out.push(c::ext_action(c::NOTIFY, f));
    } else if flags & c::NOTIFY != 0 && flags & c::TEXT_FMT != 0 && !view_only {
        st.clip_out.push(c::ext_action(c::REQUEST, c::TEXT_FMT));
    } else if flags & c::PROVIDE != 0 && !view_only {
        drop(st);
        if let Some(t) = c::parse_provide(flags, body) {
            c::set_text(&t, sh.id);
        }
        return;
    } else {
        return;
    }
    drop(st);
    sh.sig.notify();
}
