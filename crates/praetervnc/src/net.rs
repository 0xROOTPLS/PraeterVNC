use std::net::{IpAddr, TcpStream, UdpSocket};
use std::os::windows::io::AsRawSocket;
use windows::Win32::Foundation::{CloseHandle, POINT};
use windows::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER};
use windows::Win32::Networking::WinSock::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

/// Address of the default-route interface; nothing is sent.
pub fn lan_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_unspecified() && !ip.is_loopback())
}

/// Exe name of the process listening on IPv4 `port`, if any.
pub fn port_owner(port: u16) -> Option<String> {
    let pid = unsafe {
        let mut n = 0u32;
        let _ = GetExtendedTcpTable(None, &mut n, false, AF_INET.0 as u32, TCP_TABLE_OWNER_PID_LISTENER, 0);
        let mut buf = vec![0u32; n as usize / 4 + 64];
        n = buf.len() as u32 * 4;
        if GetExtendedTcpTable(Some(buf.as_mut_ptr() as *mut _), &mut n, false, AF_INET.0 as u32, TCP_TABLE_OWNER_PID_LISTENER, 0) != 0 {
            return None;
        }
        let rows = std::slice::from_raw_parts(buf.as_ptr().add(1) as *const MIB_TCPROW_OWNER_PID, buf[0] as usize);
        rows.iter().find(|r| u16::from_be(r.dwLocalPort as u16) == port)?.dwOwningPid
    };
    Some(process_name(pid).unwrap_or_else(|| format!("process {pid}")))
}

fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut name = None;
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            if e.th32ProcessID == pid {
                let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                name = Some(String::from_utf16_lossy(&e.szExeFile[..n]));
                break;
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
        name
    }
}

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
