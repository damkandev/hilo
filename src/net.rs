//! TCP + Noise. One reader and one writer thread per connection.
//! Messages above 60 KB go out in chunks; small changes to other files overtake them.
#![forbid(unsafe_code)]
use crate::noise::{Handshake, RecvCipher, SendCipher};
use crate::noise;
use crate::wire::{self, Msg, MAX_FRAME, MAX_MSG};
use std::collections::VecDeque;
use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::Duration;

const PROLOGUE: &[u8] = b"hilo/1";
pub const ZERO_PSK: [u8; 32] = [0; 32]; // known peers: identity comes from pinned static keys
const PING: Duration = Duration::from_secs(15);
pub const DEAD: Duration = Duration::from_secs(45);
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const CHUNK: usize = 60_000; // stays under Noise's 65535-byte message limit

fn bad(_: crate::noise::Error) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "handshake failed")
}

fn b64(b: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

/// The proxy to use for `host`, from HILO_PROXY, ALL_PROXY or HTTPS_PROXY (and NO_PROXY).
/// Cloud sandboxes usually only let traffic out through one of these.
pub fn proxy_for(host: &str) -> Option<String> {
    let var = |k: &str| std::env::var(k).ok().or_else(|| std::env::var(k.to_lowercase()).ok()).filter(|v| !v.is_empty());
    if let Some(p) = std::env::var("HILO_PROXY").ok().filter(|v| !v.is_empty()) {
        return (p != "none").then_some(p);
    }
    if let Some(np) = var("NO_PROXY") {
        let skip = np.split(',').map(str::trim).filter(|d| !d.is_empty()).any(|d| {
            d == "*" || host == d.trim_start_matches('.') || host.ends_with(&format!(".{}", d.trim_start_matches('.')))
        });
        if skip {
            return None;
        }
    }
    var("ALL_PROXY").or_else(|| var("HTTPS_PROXY"))
}

/// (scheme, user:password, host:port)
type Proxy<'a> = (&'a str, Option<(String, String)>, &'a str);

fn split_proxy(p: &str) -> Option<Proxy<'_>> {
    let (scheme, rest) = p.split_once("://").unwrap_or(("http", p));
    let rest = rest.trim_end_matches('/');
    let (auth, hostport) = match rest.rsplit_once('@') {
        Some((a, h)) => {
            let (u, pw) = a.split_once(':').unwrap_or((a, ""));
            (Some((u.to_string(), pw.to_string())), h)
        }
        None => (None, rest),
    };
    Some((scheme, auth, hostport))
}

/// TCP to `addr` ("host:port"), directly or through the configured proxy.
pub fn dial(addr: &str) -> io::Result<TcpStream> {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h).trim_matches(['[', ']']);
    let connect = |a: &str| -> io::Result<TcpStream> {
        let mut err = io::Error::other(format!("{a}: no address"));
        for sa in std::net::ToSocketAddrs::to_socket_addrs(a)? {
            match TcpStream::connect_timeout(&sa, HANDSHAKE_TIMEOUT) {
                Ok(s) => return Ok(s),
                Err(e) => err = e,
            }
        }
        Err(err)
    };
    let Some(p) = proxy_for(host) else { return connect(addr) };
    let (scheme, auth, hostport) = split_proxy(&p).ok_or_else(|| io::Error::other("bad proxy"))?;
    let mut s = connect(hostport)?;
    s.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let bad = |m: &str| io::Error::other(format!("proxy {hostport}: {m}"));
    use std::io::{Read, Write};
    if scheme.starts_with("socks5") {
        match &auth {
            Some(_) => s.write_all(&[5, 2, 0, 2])?,
            None => s.write_all(&[5, 1, 0])?,
        }
        let mut r = [0u8; 2];
        s.read_exact(&mut r)?;
        if r[1] == 2 {
            let (u, pw) = auth.clone().unwrap_or_default();
            let mut m = vec![1, u.len() as u8];
            m.extend(u.as_bytes());
            m.push(pw.len() as u8);
            m.extend(pw.as_bytes());
            s.write_all(&m)?;
            s.read_exact(&mut r)?;
            if r[1] != 0 {
                return Err(bad("authentication refused"));
            }
        } else if r[1] != 0 {
            return Err(bad("no acceptable auth method"));
        }
        let (h, port) = addr.rsplit_once(':').ok_or_else(|| bad("address needs a port"))?;
        let port: u16 = port.parse().map_err(|_| bad("bad port"))?;
        let mut m = vec![5, 1, 0, 3, h.len() as u8];
        m.extend(h.as_bytes());
        m.extend(port.to_be_bytes());
        s.write_all(&m)?;
        let mut head = [0u8; 4];
        s.read_exact(&mut head)?;
        if head[1] != 0 {
            return Err(bad(&format!("connect refused ({})", head[1])));
        }
        let skip = match head[3] {
            1 => 4,
            4 => 16,
            3 => {
                let mut n = [0u8; 1];
                s.read_exact(&mut n)?;
                n[0] as usize
            }
            _ => return Err(bad("bad reply")),
        } + 2;
        s.read_exact(&mut vec![0u8; skip])?;
    } else {
        let mut req = format!("CONNECT {addr} HTTP/1.1\r\nHost: {addr}\r\n");
        if let Some((u, pw)) = &auth {
            req += &format!("Proxy-Authorization: Basic {}\r\n", b64(format!("{u}:{pw}").as_bytes()));
        }
        req += "\r\n";
        s.write_all(req.as_bytes())?;
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() > 8192 || s.read(&mut b)? == 0 {
                return Err(bad("closed during CONNECT"));
            }
            head.push(b[0]);
        }
        let status = String::from_utf8_lossy(&head);
        if !status.split_whitespace().nth(1).is_some_and(|c| c == "200") {
            return Err(bad(status.lines().next().unwrap_or("refused")));
        }
    }
    s.set_read_timeout(None)?;
    Ok(s)
}

/// The receiving side of a connection: the cipher and the message being reassembled from chunks
/// (small messages may arrive between the chunks of a big one).
pub struct Rx {
    c: RecvCipher,
    part: Vec<u8>,
}

/// Shuts the connection down unless the handshake ends within HANDSHAKE_TIMEOUT in total
/// (the read timeout alone would let a slow peer trickle bytes forever). Done when the guard drops.
fn deadline(s: &TcpStream) -> io::Result<Sender<()>> {
    let (done, wait) = channel::<()>();
    let s = s.try_clone()?;
    std::thread::spawn(move || {
        if wait.recv_timeout(HANDSHAKE_TIMEOUT) == Err(RecvTimeoutError::Timeout) {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    });
    Ok(done)
}

/// Returns (send, recv, remote static key).
pub fn initiate(s: &mut TcpStream, key: [u8; 32], psk: &[u8; 32]) -> io::Result<(SendCipher, Rx, [u8; 32])> {
    s.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let _guard = deadline(s)?;
    let mut hs = Handshake::new(key, PROLOGUE, true, None).map_err(bad)?;
    wire::write_frame(s, &hs.write1(&[]).map_err(bad)?)?;
    hs.read2(&wire::read_frame(s, noise::MAX)?).map_err(bad)?;
    wire::write_frame(s, &hs.write3(&[], psk).map_err(bad)?)?;
    let (tx, c, rs) = hs.split().map_err(bad)?;
    Ok((tx, Rx { c, part: vec![] }, rs))
}

/// Returns (send, recv, remote static key, index of the psk that matched).
pub fn respond(s: &mut TcpStream, key: [u8; 32], psks: &[[u8; 32]]) -> io::Result<(SendCipher, Rx, [u8; 32], usize)> {
    s.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let _guard = deadline(s)?;
    let mut hs = Handshake::new(key, PROLOGUE, false, None).map_err(bad)?;
    hs.read1(&wire::read_frame(s, noise::MAX)?).map_err(bad)?;
    wire::write_frame(s, &hs.write2(&[]).map_err(bad)?)?;
    let (i, _) = hs.read3(&wire::read_frame(s, noise::MAX)?, psks).map_err(bad)?;
    let (tx, c, rs) = hs.split().map_err(bad)?;
    Ok((tx, Rx { c, part: vec![] }, rs, i))
}

// Plaintext of each frame: [kind][payload]. 0 = whole message, 1 = chunk with more to come, 2 = last chunk.
fn send_part(s: &mut TcpStream, c: &mut SendCipher, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut pt = Vec::with_capacity(payload.len() + 1);
    pt.push(kind);
    pt.extend_from_slice(payload);
    let mut f = vec![0; 4]; // length, then the ciphertext right behind it: one buffer, one write
    c.encrypt(&pt, &mut f).map_err(|_| io::Error::other("nonce space exhausted: reconnect"))?;
    let n = (f.len() - 4) as u32;
    f[..4].copy_from_slice(&n.to_be_bytes());
    s.write_all(&f)
}

/// Blocking send of one message (used before the writer thread exists).
pub fn send(s: &mut TcpStream, c: &mut SendCipher, m: &Msg) -> io::Result<()> {
    let enc = wire::encode(m);
    if enc.len() <= CHUNK {
        return send_part(s, c, 0, &enc);
    }
    let n = enc.len().div_ceil(CHUNK);
    for (i, p) in enc.chunks(CHUNK).enumerate() {
        send_part(s, c, if i + 1 == n { 2 } else { 1 }, p)?;
    }
    Ok(())
}

/// Key of messages that may be reordered relative to other keys. Branch moves never overtake files.
fn key(m: &Msg) -> Option<(String, String)> {
    match m {
        Msg::Op(o) if !o.path.starts_with('\0') => Some((o.wt.clone(), o.path.clone())),
        Msg::Reject(r) if !r.path.starts_with('\0') => Some((r.wt.clone(), r.path.clone())),
        _ => None,
    }
}

struct Out {
    key: Option<(String, String)>,
    enc: Vec<u8>,
    off: usize,
}

pub fn spawn_writer(mut s: TcpStream, mut c: SendCipher, rx: Receiver<Msg>) {
    std::thread::spawn(move || {
        let mut q: VecDeque<Out> = VecDeque::new();
        let mut closed = false;
        let res = (|| -> io::Result<()> {
            loop {
                // Take everything already queued; small messages for untouched keys go first.
                loop {
                    let m = if q.is_empty() && !closed {
                        match rx.recv_timeout(PING) {
                            Ok(m) => m,
                            Err(RecvTimeoutError::Timeout) => Msg::Ping,
                            Err(RecvTimeoutError::Disconnected) => return Ok(()),
                        }
                    } else {
                        match rx.try_recv() {
                            Ok(m) => m,
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => {
                                closed = true;
                                break;
                            }
                        }
                    };
                    let enc = wire::encode(&m);
                    let control = matches!(m, Msg::Ping | Msg::Lease(_) | Msg::Claim { .. } | Msg::Release { .. } | Msg::Time { .. } | Msg::Peers(_));
                    let k = key(&m);
                    let overtakes = control || k.as_ref().is_some_and(|k| !q.iter().any(|o| o.key.as_ref() == Some(k)));
                    if enc.len() <= CHUNK && (q.is_empty() || overtakes) {
                        send_part(&mut s, &mut c, 0, &enc)?;
                    } else {
                        // ponytail: a non-overtaking small message waits behind the whole big one.
                        q.push_back(Out { key: if control { None } else { k.or(Some((String::new(), "\0".into()))) }, enc, off: 0 });
                    }
                }
                let Some(o) = q.front_mut() else {
                    if closed {
                        return Ok(());
                    }
                    continue;
                };
                if o.off == 0 && o.enc.len() <= CHUNK {
                    send_part(&mut s, &mut c, 0, &o.enc)?;
                    q.pop_front();
                    continue;
                }
                let end = (o.off + CHUNK).min(o.enc.len());
                let last = end == o.enc.len();
                send_part(&mut s, &mut c, if last { 2 } else { 1 }, &o.enc[o.off..end])?;
                o.off = end;
                if last {
                    q.pop_front();
                }
            }
        })();
        if res.is_err() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    });
}

/// Reads one whole message. A big one comes in chunks, and whole small ones may arrive between them.
pub fn recv(s: &mut TcpStream, rx: &mut Rx) -> io::Result<Msg> {
    let bad = |m| io::Error::new(io::ErrorKind::InvalidData, m);
    loop {
        let f = wire::read_frame(s, MAX_FRAME)?;
        let pt = rx.c.decrypt(&f).map_err(|_| bad("bad mac"))?;
        let (&kind, payload) = pt.split_first().ok_or_else(|| bad("empty frame"))?;
        if kind == 0 {
            return wire::decode(payload).ok_or_else(|| bad("bad message"));
        }
        if !(kind == 1 || kind == 2) || rx.part.len() + payload.len() > MAX_MSG {
            return Err(bad("bad chunk"));
        }
        rx.part.extend_from_slice(payload);
        if kind == 2 {
            return wire::decode(&std::mem::take(&mut rx.part)).ok_or_else(|| bad("bad message"));
        }
    }
}

/// Calls `f` for every message until the connection dies or `f` returns false.
pub fn read_loop(s: &mut TcpStream, mut c: Rx, mut f: impl FnMut(Msg) -> bool) -> io::Result<()> {
    s.set_read_timeout(Some(DEAD))?;
    loop {
        match recv(s, &mut c)? {
            Msg::Ping => {}
            m => {
                if !f(m) {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxies() {
        assert_eq!(b64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(b64(b"ab"), "YWI=");
        let (scheme, auth, hp) = split_proxy("http://u:p@proxy.local:3128/").unwrap();
        assert_eq!((scheme, auth, hp), ("http", Some(("u".into(), "p".into())), "proxy.local:3128"));
        assert_eq!(split_proxy("socks5h://127.0.0.1:1055").unwrap().2, "127.0.0.1:1055");
    }

    /// A tiny CONNECT proxy in a thread: dial() must go through it.
    #[test]
    fn dials_through_http_connect() {
        use std::io::{Read, Write};
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let taddr = target.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = target.accept().unwrap();
            s.write_all(b"hello through the proxy").unwrap();
        });
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let paddr = proxy.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut c, _) = proxy.accept().unwrap();
            let mut head = vec![];
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                c.read_exact(&mut b).unwrap();
                head.push(b[0]);
            }
            let line = String::from_utf8(head).unwrap();
            assert!(line.starts_with(&format!("CONNECT {taddr} ")));
            let mut up = TcpStream::connect(taddr).unwrap();
            c.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").unwrap();
            let mut buf = [0u8; 64];
            let n = up.read(&mut buf).unwrap();
            c.write_all(&buf[..n]).unwrap();
        });
        std::env::set_var("HILO_PROXY", format!("http://{paddr}"));
        let mut s = dial(&taddr.to_string()).unwrap();
        std::env::remove_var("HILO_PROXY");
        let mut got = String::new();
        s.read_to_string(&mut got).unwrap();
        assert_eq!(got, "hello through the proxy");
    }
    use crate::sync::{Data, Op};
    use std::net::TcpListener;
    use std::sync::mpsc::channel;

    #[test]
    fn chunked_and_small_overtakes() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let (tx, rx, _, _) = respond(&mut s, [1; 32], &[ZERO_PSK]).unwrap();
            let (otx, orx) = channel();
            spawn_writer(s.try_clone().unwrap(), tx, orx);
            let op = |path: &str, n: usize| {
                Msg::Op(Op { seq: 1, wt: "w".into(), path: path.into(), base: [0; 32], new: [1; 32], data: Data::Full(vec![7; n]), origin: String::new(), t_event: 0, t_seq: 0 })
            };
            otx.send(op("big", 1_000_000)).unwrap();
            otx.send(op("small", 10)).unwrap();
            otx.send(op("big", 5)).unwrap(); // same key: must stay behind
            drop(otx);
            (s, rx)
        });
        let mut c = TcpStream::connect(addr).unwrap();
        let (_tx, mut rx, _) = initiate(&mut c, [2; 32], &ZERO_PSK).unwrap();
        let paths: Vec<(String, usize)> = (0..3)
            .map(|_| match recv(&mut c, &mut rx).unwrap() {
                Msg::Op(o) => (o.path, if let Data::Full(d) = o.data { d.len() } else { 0 }),
                m => panic!("{m:?}"),
            })
            .collect();
        srv.join().unwrap();
        assert_eq!(paths, [("small".into(), 10), ("big".into(), 1_000_000), ("big".into(), 5)]);
    }

    /// The writer sends small messages between the chunks of a big one: the reader must keep the big one going.
    #[test]
    fn small_between_chunks() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let (mut tx, _, _, _) = respond(&mut s, [1; 32], &[ZERO_PSK]).unwrap();
            let big = wire::encode(&Msg::Peers(vec!["x".repeat(100_000)]));
            send_part(&mut s, &mut tx, 1, &big[..CHUNK]).unwrap();
            send_part(&mut s, &mut tx, 0, &wire::encode(&Msg::CaughtUp)).unwrap();
            send_part(&mut s, &mut tx, 2, &big[CHUNK..]).unwrap();
            s
        });
        let mut c = TcpStream::connect(addr).unwrap();
        let (_tx, mut rx, _) = initiate(&mut c, [2; 32], &ZERO_PSK).unwrap();
        assert!(matches!(recv(&mut c, &mut rx).unwrap(), Msg::CaughtUp));
        assert!(matches!(recv(&mut c, &mut rx).unwrap(), Msg::Peers(v) if v[0].len() == 100_000));
        srv.join().unwrap();
    }

    /// A peer that opens a connection and says nothing is cut off after HANDSHAKE_TIMEOUT.
    #[test]
    fn handshake_has_a_total_deadline() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let _c = TcpStream::connect(addr).unwrap();
        let (s, _) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap(); // only the deadline can end this
        let t = std::time::Instant::now();
        let mut s2 = s.try_clone().unwrap();
        let _guard = deadline(&s).unwrap();
        assert!(wire::read_frame(&mut s2, noise::MAX).is_err());
        assert!(t.elapsed() < HANDSHAKE_TIMEOUT * 2);
    }
}
