//! Listener, live settings and connected clients.
use crate::auth::Guard;
use crate::capture::Capture;
use crate::settings::Settings;
use crate::Config;
use parking_lot::Mutex;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct Client {
    id: u64,
    peer: String,
    since: Instant,
    sock: TcpStream,
}

#[derive(Default)]
struct Listen {
    init: bool,
    want: Option<(String, u16)>,
    bound: Option<SocketAddr>,
    error: Option<String>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub listen: Option<String>,
    pub error: Option<String>,
    /// (peer, seconds connected)
    pub clients: Vec<(String, u64)>,
    /// (address, seconds left)
    pub blocked: Vec<(String, u64)>,
}

pub struct Server {
    pub cfg: Arc<Config>,
    pub cap: Arc<Capture>,
    pub guard: Guard,
    pub view_only: AtomicBool,
    settings: Mutex<Arc<Settings>>,
    clients: Mutex<Vec<Client>>,
    listen: Mutex<Listen>,
    paused: AtomicBool,
    gen: AtomicU64,
    on_change: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl Server {
    pub fn start(cfg: Config, s: Settings) -> Arc<Server> {
        crate::clipboard::start();
        let cap = Capture::start(s.monitor, cfg.verbose);
        let srv = Arc::new(Server {
            cfg: Arc::new(cfg),
            cap,
            guard: Guard::default(),
            view_only: AtomicBool::new(s.view_only),
            settings: Mutex::new(Arc::new(s)),
            clients: Mutex::new(Vec::new()),
            listen: Mutex::new(Listen::default()),
            paused: AtomicBool::new(false),
            gen: AtomicU64::new(0),
            on_change: Mutex::new(None),
        });
        srv.relisten();
        srv
    }

    pub fn settings(&self) -> Arc<Settings> {
        self.settings.lock().clone()
    }

    pub fn apply(self: &Arc<Self>, s: Settings) {
        self.view_only.store(s.view_only, Ordering::Relaxed);
        *self.cap.monitor.lock() = s.monitor;
        *self.settings.lock() = Arc::new(s);
        self.relisten();
    }

    /// Stops or resumes listening; sessions stay up. Pausing returns once the port is free.
    pub fn pause(self: &Arc<Self>, on: bool) {
        self.paused.store(on, Ordering::SeqCst);
        self.relisten();
    }

    /// Called on client or listener changes.
    pub fn on_change(&self, f: impl Fn() + Send + 'static) {
        *self.on_change.lock() = Some(Box::new(f));
    }

    pub fn changed(&self) {
        if let Some(f) = &*self.on_change.lock() {
            f();
        }
    }

    fn addr_for(&self, s: &Settings) -> Option<(String, u16)> {
        if let Some(b) = &self.cfg.bind {
            return Some((b.clone(), s.port));
        }
        match (&s.password, s.remote) {
            (None, _) if self.cfg.service => None,
            (Some(_), true) => Some(("0.0.0.0".into(), s.port)),
            _ => Some(("127.0.0.1".into(), s.port)),
        }
    }

    fn relisten(self: &Arc<Self>) {
        let paused = self.paused.load(Ordering::SeqCst);
        let want = if paused { None } else { self.addr_for(&self.settings()) };
        let (wake, stopped) = {
            let mut l = self.listen.lock();
            if l.init && l.want == want {
                return;
            }
            l.init = true;
            let gen = self.gen.fetch_add(1, Ordering::SeqCst) + 1;
            l.want = want.clone();
            l.error = None;
            let wake = l.bound.take();
            let mut prev = l.thread.take();
            match want {
                Some((host, port)) => {
                    let prev = prev.take();
                    let me = self.clone();
                    l.thread = Some(std::thread::Builder::new().name("listen".into()).spawn(move || {
                        if let Some(p) = prev {
                            let _ = p.join();
                        }
                        me.accept_loop(gen, &host, port);
                    }).unwrap());
                }
                None if paused => crate::log!("listener paused"),
                None => {
                    l.error = Some("no password set".into());
                    crate::log!("not listening: no password set");
                }
            }
            (wake, prev.filter(|_| paused))
        };
        // Unblocks the old accept().
        if let Some(mut a) = wake {
            if a.ip().is_unspecified() {
                a.set_ip([127, 0, 0, 1].into());
            }
            let _ = TcpStream::connect_timeout(&a, Duration::from_millis(300));
        }
        if let Some(t) = stopped {
            for _ in 0..40 {
                if t.is_finished() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        self.changed();
    }

    fn accept_loop(self: Arc<Self>, gen: u64, host: &str, port: u16) {
        let li = loop {
            if self.gen.load(Ordering::SeqCst) != gen {
                return;
            }
            match TcpListener::bind((host, port)) {
                Ok(li) => break li,
                Err(e) => {
                    let msg = listen_error(port, &e);
                    if self.listen.lock().error.replace(msg.clone()).as_ref() != Some(&msg) {
                        crate::log!("{msg}; retrying");
                        self.changed();
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        };
        {
            let mut l = self.listen.lock();
            if self.gen.load(Ordering::SeqCst) != gen {
                return;
            }
            l.bound = li.local_addr().ok();
            l.error = None;
        }
        crate::log!("listening on {host}:{port}");
        if self.settings().password.is_none() && self.cfg.bind.as_deref().is_some_and(|b| b != "127.0.0.1") {
            crate::log!("warning: no password set; anyone who can reach {host}:{port} gets full control");
        }
        self.changed();
        for s in li.incoming() {
            if self.gen.load(Ordering::SeqCst) != gen {
                return;
            }
            let Ok(s) = s else { continue };
            let me = self.clone();
            std::thread::Builder::new().name("rfb-session".into()).spawn(move || {
                if let Err(e) = crate::session::handle(s, &me) {
                    crate::log!("session: {e}");
                }
            }).unwrap();
        }
    }

    pub fn add_client(&self, id: u64, peer: &str, sock: &TcpStream) {
        if let Ok(sock) = sock.try_clone() {
            self.clients.lock().push(Client { id, peer: peer.into(), since: Instant::now(), sock });
        }
        self.changed();
    }

    pub fn remove_client(&self, id: u64) {
        self.clients.lock().retain(|c| c.id != id);
        self.changed();
    }

    pub fn disconnect_all(&self) {
        for c in self.clients.lock().iter() {
            let _ = c.sock.shutdown(Shutdown::Both);
        }
    }

    pub fn status(&self) -> Status {
        let l = self.listen.lock();
        Status {
            listen: l.bound.map(|a| a.to_string()),
            error: l.error.clone(),
            clients: self.clients.lock().iter().map(|c| (c.peer.clone(), c.since.elapsed().as_secs())).collect(),
            blocked: self.guard.list().into_iter().map(|(ip, d)| (ip.to_string(), d.as_secs() + 1)).collect(),
        }
    }
}

fn listen_error(port: u16, e: &std::io::Error) -> String {
    const IN_USE: i32 = 10048;
    const ACCESS: i32 = 10013;
    match (e.raw_os_error(), crate::net::port_owner(port)) {
        (Some(IN_USE | ACCESS), Some(p)) => format!("Port {port} is in use by {p}"),
        (Some(IN_USE), None) => format!("Port {port} is in use by another program"),
        (Some(ACCESS), None) => format!("Port {port} is reserved by Windows"),
        _ => format!("Can't listen on port {port}: {e}"),
    }
}

impl Status {
    pub fn encode(&self) -> String {
        let mut s = String::new();
        if let Some(l) = &self.listen {
            s += &format!("listen {l}\n");
        }
        if let Some(e) = &self.error {
            s += &format!("error {e}\n");
        }
        for (p, t) in &self.clients {
            s += &format!("client {t} {p}\n");
        }
        for (p, t) in &self.blocked {
            s += &format!("blocked {t} {p}\n");
        }
        s
    }

    pub fn decode(t: &str) -> Status {
        let mut s = Status::default();
        for l in t.lines() {
            match l.split_once(' ') {
                Some(("listen", v)) => s.listen = Some(v.into()),
                Some(("error", v)) => s.error = Some(v.into()),
                Some((k @ ("client" | "blocked"), v)) => {
                    if let Some((t, p)) = v.split_once(' ') {
                        let list = if k == "client" { &mut s.clients } else { &mut s.blocked };
                        list.push((p.into(), t.parse().unwrap_or(0)));
                    }
                }
                _ => {}
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrip() {
        let s = Status {
            listen: Some("0.0.0.0:5900".into()),
            error: Some("Port 5900 is in use by tvnserver.exe".into()),
            clients: vec![("10.0.0.2:50123".into(), 42)],
            blocked: vec![("10.0.0.9".into(), 55)],
        };
        assert_eq!(Status::decode(&s.encode()), s);
    }
}
