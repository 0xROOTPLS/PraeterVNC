use std::net::TcpStream;
use std::os::windows::io::AsRawSocket;
use windows::Win32::Foundation::POINT;
use windows::Win32::Networking::WinSock::*;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

pub fn tune(s: &TcpStream) {
    unsafe {
        let sock = SOCKET(s.as_raw_socket() as usize);
        // Low-delay DSCP hint.
        let tos: i32 = 0x10;
        let _ = setsockopt(sock, IPPROTO_IP.0, IP_TOS, Some(&tos.to_ne_bytes()));
        // Dead peers are dropped in ~20 s even when idle.
        let ka = tcp_keepalive { onoff: 1, keepalivetime: 10_000, keepaliveinterval: 1_000 };
        let mut ret = 0u32;
        let _ = WSAIoctl(sock, SIO_KEEPALIVE_VALS, Some(&ka as *const _ as *const _), std::mem::size_of::<tcp_keepalive>() as u32, None, 0, &mut ret, None, None);
    }
    let _ = s.set_write_timeout(Some(std::time::Duration::from_secs(30)));
}

/// Cursor hotspot in framebuffer coordinates.
pub fn cursor_pos(origin: (i32, i32)) -> Option<(i32, i32)> {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p).ok()? };
    Some((p.x - origin.0, p.y - origin.1))
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TcpInfo {
    pub rtt_us: u32,
    pub min_rtt_us: u32,
    pub bytes_in_flight: u32,
    pub cwnd: u32,
    pub snd_wnd: u32,
    pub bytes_out: u64,
    pub bytes_retrans: u64,
}

/// SIO_TCP_INFO (Windows 10 1703+).
pub fn tcp_info(s: &TcpStream) -> Option<TcpInfo> {
    unsafe {
        let sock = SOCKET(s.as_raw_socket() as usize);
        let ver: u32 = 0;
        let mut info = TCP_INFO_v0::default();
        let mut ret = 0u32;
        let r = WSAIoctl(sock, SIO_TCP_INFO, Some(&ver as *const u32 as *const _), 4, Some(&mut info as *mut _ as *mut _),
            std::mem::size_of::<TCP_INFO_v0>() as u32, &mut ret, None, None);
        if r != 0 {
            return None;
        }
        Some(TcpInfo {
            rtt_us: info.RttUs, min_rtt_us: info.MinRttUs, bytes_in_flight: info.BytesInFlight, cwnd: info.Cwnd, snd_wnd: info.SndWnd,
            bytes_out: info.BytesOut, bytes_retrans: info.BytesRetrans as u64,
        })
    }
}
