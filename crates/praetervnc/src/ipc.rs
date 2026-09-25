//! Local control pipe between the server and its tray.
use crate::server::Server;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{LocalFree, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    FlushFileBuffers, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_READ, FILE_WRITE_DATA, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::Pipes::*;

pub const SERVICE_PIPE: &str = r"\\.\pipe\PraeterVNC";

/// Interactive users: read/write, no FILE_CREATE_PIPE_INSTANCE.
const SDDL: PCWSTR = w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;0x12019b;;;IU)");

/// Blocking accept loop; one thread per connection.
pub fn serve(srv: Arc<Server>) {
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        if let Err(e) = ConvertStringSecurityDescriptorToSecurityDescriptorW(SDDL, 1, &mut sd, None) {
            return crate::log!("ipc: {e}");
        }
        let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() };
        let r = accept(&srv, &sa);
        LocalFree(Some(HLOCAL(sd.0)));
        if let Err(e) = r {
            crate::log!("ipc: {e}");
        }
    }
}

fn accept(srv: &Arc<Server>, sa: &SECURITY_ATTRIBUTES) -> io::Result<()> {
    let name = HSTRING::from(SERVICE_PIPE);
    let create = |first: bool| -> io::Result<File> {
        let open = if first { PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE } else { PIPE_ACCESS_DUPLEX };
        let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
        let h = unsafe { CreateNamedPipeW(&name, open, mode, PIPE_UNLIMITED_INSTANCES, 4096, 4096, 0, Some(sa as *const _)) };
        if h.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_handle(h.0) })
    };
    // A held instance prevents name squatting.
    let mut next = create(true)?;
    loop {
        let r = unsafe { ConnectNamedPipe(HANDLE(next.as_raw_handle()), None) };
        let pipe = std::mem::replace(&mut next, create(false)?);
        if r.is_ok() || r.is_err_and(|e| e.code() == ERROR_PIPE_CONNECTED.to_hresult()) {
            let srv = srv.clone();
            let _ = std::thread::Builder::new().name("ipc".into()).spawn(move || reply(&srv, pipe));
        }
    }
}

fn reply(srv: &Server, pipe: File) {
    let mut line = Vec::new();
    if BufReader::new((&pipe).take(256)).read_until(b'\n', &mut line).is_err() {
        return;
    }
    let out = match String::from_utf8_lossy(&line).trim() {
        "status" => srv.status().encode(),
        "disconnect" => {
            srv.disconnect_all();
            "ok\n".into()
        }
        "release" => format!("ok {}\n", crate::input::release_stuck()),
        _ => "error\n".into(),
    };
    if (&pipe).write_all(out.as_bytes()).is_ok() {
        // Waits until the client has read it all.
        let _ = unsafe { FlushFileBuffers(HANDLE(pipe.as_raw_handle())) };
    }
}

/// Sends `cmd` and returns the reply; None if the server isn't running.
pub fn request(cmd: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut f = loop {
        let r = OpenOptions::new()
            .access_mode(FILE_GENERIC_READ.0 | FILE_WRITE_DATA.0)
            .security_qos_flags((SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION).0)
            .open(SERVICE_PIPE);
        match r {
            Ok(f) => break f,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) && Instant::now() < deadline => unsafe {
                let _ = WaitNamedPipeW(&HSTRING::from(SERVICE_PIPE), 250);
            },
            Err(_) => return None,
        }
    };
    f.write_all(format!("{cmd}\n").as_bytes()).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}
