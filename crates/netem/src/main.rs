//! TCP proxy adding one-way delay and a bandwidth cap per direction.
//! praeter-netem <listen_port> <target_host:port> <one_way_delay_ms> [mbit_per_s]
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

struct Pipe {
    q: Mutex<(VecDeque<(Instant, Vec<u8>)>, bool)>,
    cv: Condvar,
}

fn pump(mut from: TcpStream, mut to: TcpStream, delay: Duration, bps: f64) {
    let p = Arc::new(Pipe { q: Mutex::new((VecDeque::new(), false)), cv: Condvar::new() });
    let p2 = p.clone();
    let w = std::thread::spawn(move || {
        let mut next_free = Instant::now();
        loop {
            let (due, buf) = {
                let mut g = p2.q.lock().unwrap();
                loop {
                    if let Some(f) = g.0.pop_front() {
                        break f;
                    }
                    if g.1 {
                        let _ = to.shutdown(std::net::Shutdown::Write);
                        return;
                    }
                    g = p2.cv.wait(g).unwrap();
                }
            };
            // Link busy until `next_free`; then propagation delay.
            let tx = if bps > 0.0 { Duration::from_secs_f64(buf.len() as f64 * 8.0 / bps) } else { Duration::ZERO };
            next_free = due.max(next_free) + tx;
            let deliver = next_free + delay;
            let now = Instant::now();
            if deliver > now {
                std::thread::sleep(deliver - now);
            }
            if to.write_all(&buf).is_err() {
                return;
            }
        }
    });
    let mut b = vec![0u8; 64 * 1024];
    loop {
        match from.read(&mut b) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut g = p.q.lock().unwrap();
                g.0.push_back((Instant::now(), b[..n].to_vec()));
                p.cv.notify_one();
            }
        }
    }
    p.q.lock().unwrap().1 = true;
    p.cv.notify_one();
    let _ = w.join();
}

fn main() {
    unsafe {
        windows::Win32::Media::timeBeginPeriod(1);
    }
    let a: Vec<String> = std::env::args().collect();
    let port: u16 = a[1].parse().unwrap();
    let target = a[2].clone();
    let delay = Duration::from_secs_f64(a[3].parse::<f64>().unwrap() / 1e3);
    let bps: f64 = a.get(4).map(|m| m.parse::<f64>().unwrap() * 1e6).unwrap_or(0.0);
    let l = TcpListener::bind(("127.0.0.1", port)).unwrap();
    eprintln!("netem :{port} -> {target} delay {:?} each way, {} Mbit/s", delay, bps / 1e6);
    for c in l.incoming() {
        let Ok(c) = c else { continue };
        let Ok(s) = TcpStream::connect(&target) else { continue };
        let _ = c.set_nodelay(true);
        let _ = s.set_nodelay(true);
        let (c2, s2) = (c.try_clone().unwrap(), s.try_clone().unwrap());
        std::thread::spawn(move || pump(c, s, delay, bps));
        std::thread::spawn(move || pump(s2, c2, delay, bps));
    }
}
