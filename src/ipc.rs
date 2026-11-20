//! Unix socket between the daemon and the CLI, hooks and MCP (Windows: loopback TCP, see `listen`).
//! Requests are lists of strings; replies are (code, text). Lease checks are answered right here from the daemon's copy of the
//! table, without waiting for the main loop. Nothing on this socket ever returns a secret value.
#![forbid(unsafe_code)]
use crate::daemon::{Ev, Shared};
use crate::lease;
use crate::wire::{read_frame, write_frame, Msg, Rd, W};
use std::io;
#[cfg(windows)]
use std::net::{TcpListener as Listener, TcpStream as Stream};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::time::Duration;

pub fn socket_path() -> PathBuf {
    let p = crate::config::path("hilo.sock");
    if cfg!(windows) || p.as_os_str().len() < 100 {
        return p;
    }
    // sun_path is ~104 bytes on macOS: deep HILO_HOMEs get a short, stable name instead
    let h = crate::crypto::hex(&crate::crypto::sha256(p.as_os_str().as_encoded_bytes()));
    std::env::temp_dir().join(format!("hilo-{}", &h[..12])).join("hilo.sock")
}

/// The socket path. A fallback in the temp dir (which others may write to) must sit in a 0700
/// folder owned by whoever owns the hilo home, or it is refused.
#[cfg(unix)]
fn checked_path() -> io::Result<PathBuf> {
    let p = socket_path();
    let (Some(d), home) = (p.parent(), crate::config::home()) else { return Ok(p) };
    if d == home {
        return Ok(p);
    }
    let _ = std::fs::DirBuilder::new().mode(0o700).create(d);
    let (m, owner) = (std::fs::symlink_metadata(d)?, std::fs::metadata(&home)?.uid());
    if !m.is_dir() || m.uid() != owner || m.mode() & 0o077 != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is not a private folder of yours", d.display())));
    }
    Ok(p)
}

fn encode(args: &[String]) -> Vec<u8> {
    let mut w = W::default();
    w.u32(args.len() as u32);
    for a in args {
        w.str(a);
    }
    w.0
}

fn decode(b: &[u8]) -> Option<Vec<String>> {
    let mut r = Rd(b);
    let n = r.u32()? as usize;
    (0..n.min(64)).map(|_| r.str()).collect()
}

#[cfg(unix)]
fn listen() -> io::Result<Listener> {
    let p = checked_path()?;
    let _ = std::fs::remove_file(&p);
    let l = Listener::bind(&p)?;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

// Windows: std has no AF_UNIX there, so the daemon listens on a random loopback port and writes
// "port token" to hilo.sock, which only this user can read (the profile's ACL). Any local process can
// connect, so a client must send the token; and since a dead daemon's port can be taken by anyone,
// the daemon first proves it has the token too (sha256 over the client's nonce), before a request
// (which may carry an env value) leaves the client.
#[cfg(windows)]
static TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

#[cfg(windows)]
fn proof(token: &str, nonce: &[u8]) -> [u8; 32] {
    crate::crypto::sha256(&[token.as_bytes(), nonce].concat())
}

#[cfg(windows)]
fn listen() -> io::Result<Listener> {
    let l = Listener::bind("127.0.0.1:0")?;
    let t = TOKEN.get_or_init(|| crate::crypto::hex(&crate::crypto::random::<32>()));
    crate::config::write_private("hilo.sock", format!("{} {t}", l.local_addr()?.port()).as_bytes())?;
    Ok(l)
}

pub fn serve(sh: Arc<Shared>, tx: Sender<Ev>) -> io::Result<()> {
    let l = listen()?;
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let (sh, tx) = (sh.clone(), tx.clone());
            std::thread::spawn(move || {
                let _ = handle(s, &sh, &tx);
            });
        }
    });
    Ok(())
}

fn with_notices(sh: &Shared, session: &str, code: u8, text: String) -> (u8, String) {
    let n = sh.notices_for(session);
    if n.is_empty() {
        return (code, text);
    }
    let mut all: Vec<String> = n;
    if !text.is_empty() {
        all.insert(0, text);
    }
    (2, all.join("\n\n"))
}

fn handle(mut s: Stream, sh: &Shared, tx: &Sender<Ev>) -> io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    #[cfg(windows)]
    let token = TOKEN.get().ok_or_else(|| io::Error::other("no token"))?;
    #[cfg(windows)]
    {
        let nonce = read_frame(&mut s, 64)?;
        write_frame(&mut s, &proof(token, &nonce))?;
    }
    let req = decode(&read_frame(&mut s, 1 << 20)?).ok_or_else(|| io::Error::other("bad request"))?;
    #[cfg(windows)]
    let req = match req.split_first() {
        Some((t, rest)) if crate::crypto::ct_eq(t.as_bytes(), token.as_bytes()) => rest.to_vec(),
        _ => return Err(io::Error::other("bad token")),
    };
    let a: Vec<&str> = req.iter().map(String::as_str).collect();
    let (code, text) = match a.as_slice() {
        // PreToolUse: may this session edit that file?
        ["claim", session, path] => match sh.locate(Path::new(path)) {
            None => with_notices(sh, session, 0, String::new()),
            Some((wt, rel)) => {
                let mut t = sh.leases.lock().unwrap();
                match t.holder(&wt, &rel) {
                    Some(e) if e.session != *session => (2, lease::busy_message(&rel, e)),
                    _ => {
                        t.claim(&wt, &rel, session, &sh.name); // optimistic; the anchor has the last word
                        drop(t);
                        sh.send(Msg::Claim { wt, path: rel, session: session.to_string() });
                        with_notices(sh, session, 0, String::new())
                    }
                }
            }
        },
        // PostToolUse: renew, remember who wrote it, and sync it now (no watcher delay)
        ["touch", session, path] => {
            if let Some((wt, rel)) = sh.locate(Path::new(path)) {
                sh.leases.lock().unwrap().claim(&wt, &rel, session, &sh.name);
                sh.authors.lock().unwrap().insert((wt.clone(), rel.clone()), session.to_string());
                sh.send(Msg::Claim { wt, path: rel, session: session.to_string() });
                let _ = tx.send(Ev::Touched(PathBuf::from(path)));
            }
            with_notices(sh, session, 0, String::new())
        }
        ["release", session, rest @ ..] => {
            let (wt, rel) = match rest.first().and_then(|p| sh.locate(Path::new(p))) {
                Some(x) => x,
                None if rest.is_empty() => (String::new(), String::new()),
                None => return reply(&mut s, 0, String::new()),
            };
            sh.leases.lock().unwrap().release(&wt, &rel, session);
            sh.send(Msg::Release { wt, path: rel, session: session.to_string() });
            with_notices(sh, session, 0, String::new())
        }
        // MCP file tools: is this path in a synced unit? "unit\0root\0rel" (rel "" = the root)
        ["locate", path] => match sh.unit_of(Path::new(path)) {
            Some((id, root, rel)) => (0, format!("{id}\0{}\0{rel}", root.display())),
            None => (1, format!("{path} is not in a synced folder")),
        },
        ["who", path] => match sh.locate(Path::new(path)) {
            None => (0, format!("{path} is not in a synced worktree")),
            Some((wt, rel)) => match sh.leases.lock().unwrap().holder(&wt, &rel) {
                None => (0, format!("{rel}: free")),
                Some(e) => (0, lease::busy_message(&rel, e)),
            },
        },
        _ => {
            let (rtx, rrx) = channel();
            tx.send(Ev::Ipc(req.clone(), rtx)).map_err(|_| io::Error::other("daemon stopping"))?;
            rrx.recv_timeout(Duration::from_secs(60)).unwrap_or((1, "daemon busy".into()))
        }
    };
    reply(&mut s, code, text)
}

fn reply(s: &mut Stream, code: u8, text: String) -> io::Result<()> {
    let mut w = W::default();
    w.u8(code).str(&text);
    write_frame(s, &w.0)
}

/// Sends a request to the local daemon.
pub fn call(args: &[String], timeout: Duration) -> io::Result<(u8, String)> {
    let (mut s, args) = dial(args, timeout)?;
    write_frame(&mut s, &encode(&args))?;
    let f = read_frame(&mut s, 64 << 20)?;
    let mut r = Rd(&f);
    match (r.u8(), r.str()) {
        (Some(c), Some(t)) => Ok((c, t)),
        _ => Err(io::Error::other("bad reply")),
    }
}

#[cfg(unix)]
fn dial(args: &[String], timeout: Duration) -> io::Result<(Stream, Vec<String>)> {
    let s = Stream::connect(checked_path()?)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    Ok((s, args.to_vec()))
}

/// The other half of `listen`: check the daemon's proof, then send the token ahead of the request.
#[cfg(windows)]
fn dial(args: &[String], timeout: Duration) -> io::Result<(Stream, Vec<String>)> {
    let f = std::fs::read_to_string(socket_path())?;
    let (port, token) = f.trim().split_once(' ').ok_or_else(|| io::Error::other("bad hilo.sock"))?;
    let port: u16 = port.parse().map_err(io::Error::other)?;
    let mut s = Stream::connect_timeout(&([127, 0, 0, 1], port).into(), timeout)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    let nonce = crate::crypto::random::<16>();
    write_frame(&mut s, &nonce)?;
    if !crate::crypto::ct_eq(&read_frame(&mut s, 64)?, &proof(token, &nonce)) {
        return Err(io::Error::new(io::ErrorKind::ConnectionRefused, "the process on hilo's port is not hilo's daemon"));
    }
    Ok((s, [vec![token.to_string()], args.to_vec()].concat()))
}

pub fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}
