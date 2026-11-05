//! agentbox-guest: runs inside agentbox VMs (static Linux binary, std only).
//!
//!   agentbox-guest fwd LISTEN=TARGET...    e.g. 127.0.1.1:5432=127.0.0.1:20432
//!
//! Compose gives every service its own loopback address inside each container; this
//! forwards those addresses to the ports the services are published on via the host.
//!
//! Each connection is relayed by one thread with poll(2): libkrun's proxied (TSI)
//! sockets don't cope with one thread blocking in read while another writes.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::io::AsRawFd;
use std::thread;
use std::time::Duration;

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

extern "C" {
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
}

const POLLIN: i16 = 0x001;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;

fn relay(mut a: TcpStream, mut b: TcpStream) {
    let mut buf = vec![0u8; 64 * 1024];
    // open[i]: we still read from side i
    let mut open = [true, true];
    loop {
        let mut fds = [
            PollFd { fd: if open[0] { a.as_raw_fd() } else { -1 }, events: POLLIN, revents: 0 },
            PollFd { fd: if open[1] { b.as_raw_fd() } else { -1 }, events: POLLIN, revents: 0 },
        ];
        let n = unsafe { poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        for i in 0..2 {
            if !open[i] || fds[i].revents & (POLLIN | POLLHUP | POLLERR) == 0 {
                continue;
            }
            let (src, dst) = if i == 0 { (&mut a, &mut b) } else { (&mut b, &mut a) };
            match src.read(&mut buf) {
                Ok(0) | Err(_) => {
                    open[i] = false;
                    let _ = dst.shutdown(Shutdown::Write);
                }
                Ok(n) => {
                    if dst.write_all(&buf[..n]).is_err() {
                        return;
                    }
                }
            }
        }
        if !open[0] && !open[1] {
            return;
        }
    }
}

fn forward(listen: String, target: String) {
    let l = loop {
        match TcpListener::bind(&listen) {
            Ok(l) => break l,
            Err(e) => {
                eprintln!("agentbox-guest: listen {listen}: {e}; retrying");
                thread::sleep(Duration::from_secs(2));
            }
        }
    };
    for c in l.incoming().flatten() {
        let target = target.clone();
        thread::spawn(move || {
            let _ = c.set_nodelay(true);
            // On failure, dropping `c` closes the client, like a closed port.
            if let Ok(up) = TcpStream::connect(&target) {
                let _ = up.set_nodelay(true);
                relay(c, up);
            }
        });
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("fwd") => {
            let mut handles = vec![];
            for spec in &args[1..] {
                let Some((l, t)) = spec.split_once('=') else {
                    eprintln!("agentbox-guest: bad spec {spec}");
                    continue;
                };
                let (l, t) = (l.to_string(), t.to_string());
                handles.push(thread::spawn(move || forward(l, t)));
            }
            for h in handles {
                let _ = h.join();
            }
            Ok(())
        }
        Some("version") => {
            println!("agentbox-guest {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            eprintln!("usage: agentbox-guest fwd LISTEN=TARGET...");
            std::process::exit(2)
        }
    }
}
