//! Persisted settings: HKLM for the service, an ini next to the exe for portable use.
use std::io;
use std::path::PathBuf;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::Cryptography::*;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Registry::*;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Settings {
    pub port: u16,
    pub password: Option<String>,
    /// Accept non-loopback connections (only with a password).
    pub remote: bool,
    pub view_only: bool,
    pub monitor: Option<usize>,
    /// Failed logins per address before it is blocked; 0 = unlimited.
    pub max_attempts: u32,
    pub block_secs: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { port: 5900, password: None, remote: true, view_only: false, monitor: None, max_attempts: 5, block_secs: 60 }
    }
}

pub const KEY: PCWSTR = w!("SOFTWARE\\PraeterVNC");
/// SYSTEM and Administrators only.
pub const KEY_SDDL: PCWSTR = w!("D:P(A;OICI;KA;;;SY)(A;OICI;KA;;;BA)");
const ENTROPY: &[u8] = b"PraeterVNC";

#[derive(Clone, Debug)]
pub enum Store {
    Registry,
    Ini(PathBuf),
}

pub fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_default()
}

impl Store {
    pub fn portable() -> Store {
        Store::Ini(exe_path().with_file_name("praetervnc.ini"))
    }

    pub fn load(&self) -> Settings {
        match self {
            Store::Registry => load_reg().unwrap_or_default(),
            Store::Ini(p) => std::fs::read_to_string(p).map(|t| parse_ini(&t)).unwrap_or_default(),
        }
    }

    /// `password`: None keeps the stored one.
    pub fn save(&self, s: &Settings, password: Option<Option<&str>>) -> io::Result<()> {
        match self {
            Store::Registry => save_reg(s, password),
            Store::Ini(p) => {
                let old = std::fs::read_to_string(p).unwrap_or_default();
                let blob = match password {
                    None => old.lines().find_map(|l| l.strip_prefix("password=")).unwrap_or("").to_string(),
                    Some(None) => String::new(),
                    Some(Some(pw)) => hex(&protect(pw.as_bytes(), false)?),
                };
                let t = format!(
                    "port={}\npassword={}\nremote={}\nview_only={}\nmonitor={}\nmax_attempts={}\nblock_seconds={}\n",
                    s.port, blob, s.remote as u8, s.view_only as u8, s.monitor.map_or("all".into(), |m| m.to_string()), s.max_attempts, s.block_secs
                );
                std::fs::write(p, t)
            }
        }
    }
}

fn parse_ini(t: &str) -> Settings {
    let mut s = Settings::default();
    for l in t.lines() {
        let Some((k, v)) = l.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        let b = v == "1" || v.eq_ignore_ascii_case("true");
        match k {
            "port" => s.port = v.parse().unwrap_or(s.port),
            "password" if !v.is_empty() => {
                s.password = unhex(v).and_then(|b| unprotect(&b)).and_then(|b| String::from_utf8(b).ok());
                if s.password.is_none() {
                    crate::log!("stored password can't be decrypted by this user; set it again");
                }
            }
            "remote" => s.remote = b,
            "view_only" => s.view_only = b,
            "monitor" => s.monitor = v.parse().ok(),
            "max_attempts" => s.max_attempts = v.parse().unwrap_or(s.max_attempts),
            "block_seconds" => s.block_secs = v.parse().unwrap_or(s.block_secs),
            _ => {}
        }
    }
    s
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// DPAPI; `machine`: decryptable by any local account.
pub fn protect(data: &[u8], machine: bool) -> io::Result<Vec<u8>> {
    unsafe {
        let inp = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let ent = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        let f = CRYPTPROTECT_UI_FORBIDDEN | if machine { CRYPTPROTECT_LOCAL_MACHINE } else { 0 };
        CryptProtectData(&inp, w!("PraeterVNC"), Some(&ent), None, None, f, &mut out)?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(Some(HLOCAL(out.pbData as _)));
        Ok(v)
    }
}

pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    unsafe {
        let inp = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let ent = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(&inp, None, Some(&ent), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out).ok()?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(Some(HLOCAL(out.pbData as _)));
        Some(v)
    }
}

pub struct Key(pub HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

impl Key {
    pub fn open(root: HKEY, path: PCWSTR, access: REG_SAM_FLAGS) -> io::Result<Key> {
        let mut h = HKEY::default();
        unsafe { RegOpenKeyExW(root, path, None, access, &mut h).ok()? };
        Ok(Key(h))
    }

    /// Creates with `sddl` if new.
    pub fn create(root: HKEY, path: PCWSTR, sddl: Option<PCWSTR>) -> io::Result<Key> {
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            let sa = match sddl {
                Some(s) => {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(s, 1, &mut sd, None)?;
                    Some(SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() })
                }
                None => None,
            };
            let mut h = HKEY::default();
            let r = RegCreateKeyExW(root, path, None, None, REG_OPTION_NON_VOLATILE, KEY_ALL_ACCESS, sa.as_ref().map(|s| s as *const _), &mut h, None);
            if !sd.0.is_null() {
                LocalFree(Some(HLOCAL(sd.0)));
            }
            r.ok()?;
            Ok(Key(h))
        }
    }

    pub fn get_raw(&self, name: PCWSTR) -> Option<(REG_VALUE_TYPE, Vec<u8>)> {
        unsafe {
            let mut ty = REG_VALUE_TYPE::default();
            let mut n = 0u32;
            RegQueryValueExW(self.0, name, None, Some(&mut ty), None, Some(&mut n)).ok().ok()?;
            let mut b = vec![0u8; n as usize];
            RegQueryValueExW(self.0, name, None, Some(&mut ty), Some(b.as_mut_ptr()), Some(&mut n)).ok().ok()?;
            b.truncate(n as usize);
            Some((ty, b))
        }
    }

    pub fn get_u32(&self, name: PCWSTR) -> Option<u32> {
        match self.get_raw(name)? {
            (REG_DWORD, b) if b.len() == 4 => Some(u32::from_le_bytes(b.try_into().ok()?)),
            _ => None,
        }
    }

    pub fn get_str(&self, name: PCWSTR) -> Option<String> {
        let (_, b) = self.get_raw(name)?;
        let u: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        Some(String::from_utf16_lossy(&u))
    }

    pub fn set_u32(&self, name: PCWSTR, v: u32) -> io::Result<()> {
        unsafe { RegSetValueExW(self.0, name, None, REG_DWORD, Some(&v.to_le_bytes())).ok()? };
        Ok(())
    }

    pub fn set_str(&self, name: PCWSTR, v: &str) -> io::Result<()> {
        let b: Vec<u8> = v.encode_utf16().chain([0]).flat_map(|c| c.to_le_bytes()).collect();
        unsafe { RegSetValueExW(self.0, name, None, REG_SZ, Some(&b)).ok()? };
        Ok(())
    }

    pub fn set_bytes(&self, name: PCWSTR, v: &[u8]) -> io::Result<()> {
        unsafe { RegSetValueExW(self.0, name, None, REG_BINARY, Some(v)).ok()? };
        Ok(())
    }

    pub fn delete(&self, name: PCWSTR) {
        unsafe {
            let _ = RegDeleteValueW(self.0, name);
        }
    }
}

fn load_reg() -> io::Result<Settings> {
    let k = Key::open(HKEY_LOCAL_MACHINE, KEY, KEY_READ)?;
    let d = Settings::default();
    Ok(Settings {
        port: k.get_u32(w!("Port")).map_or(d.port, |v| v as u16),
        password: k.get_raw(w!("Password")).and_then(|(_, b)| unprotect(&b)).and_then(|b| String::from_utf8(b).ok()).filter(|p| !p.is_empty()),
        remote: k.get_u32(w!("AllowRemote")).map_or(d.remote, |v| v != 0),
        view_only: k.get_u32(w!("ViewOnly")).is_some_and(|v| v != 0),
        monitor: k.get_u32(w!("Monitor")).filter(|&v| v != u32::MAX).map(|v| v as usize),
        max_attempts: k.get_u32(w!("MaxAttempts")).unwrap_or(d.max_attempts),
        block_secs: k.get_u32(w!("BlockSeconds")).unwrap_or(d.block_secs),
    })
}

fn save_reg(s: &Settings, password: Option<Option<&str>>) -> io::Result<()> {
    let k = Key::create(HKEY_LOCAL_MACHINE, KEY, Some(KEY_SDDL))?;
    k.set_u32(w!("Port"), s.port as u32)?;
    k.set_u32(w!("AllowRemote"), s.remote as u32)?;
    k.set_u32(w!("ViewOnly"), s.view_only as u32)?;
    k.set_u32(w!("Monitor"), s.monitor.map_or(u32::MAX, |m| m as u32))?;
    k.set_u32(w!("MaxAttempts"), s.max_attempts)?;
    k.set_u32(w!("BlockSeconds"), s.block_secs)?;
    match password {
        Some(Some(p)) => k.set_bytes(w!("Password"), &protect(p.as_bytes(), true)?)?,
        Some(None) => k.delete(w!("Password")),
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_roundtrip() {
        let p = std::env::temp_dir().join(format!("praeter-test-{}.ini", std::process::id()));
        let st = Store::Ini(p.clone());
        let s = Settings { port: 5901, password: None, remote: false, view_only: true, monitor: Some(1), max_attempts: 3, block_secs: 120 };
        st.save(&s, Some(Some("secret"))).unwrap();
        let l = st.load();
        assert_eq!(l.password.as_deref(), Some("secret"));
        assert_eq!(Settings { password: None, ..l }, s);
        st.save(&s, None).unwrap();
        assert_eq!(st.load().password.as_deref(), Some("secret"));
        let _ = std::fs::remove_file(p);
    }
}
