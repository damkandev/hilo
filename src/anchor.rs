//! The anchor: orders every change, keeps the latest version of each key, owns the lease table,
//! hands out vault keys, and serves catch-up as a list of hashes (machines fetch only what differs).
#![forbid(unsafe_code)]
use crate::config::{self, Peer};
use crate::crypto::{ct_eq, hex};
use crate::fsops::{excluded, is_env_name, valid_path, MAX_FILE};
use crate::lease;
use crate::log;
use crate::net;
use crate::sync::{hash_of, meta_hash, parse_meta, parse_ref, ref_hash, Data, Hash, Key, Op, Reject, Sequencer, META, NONE, REF};
use crate::vault;
use crate::wire::{Msg, Rd, MAX_MSG, W};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

pub enum In {
    Connect { id: u64, tx: Sender<Msg>, key: [u8; 32], ip: String, stream: Option<TcpStream> },
    Msg(u64, Msg),
    Gone(u64),
    /// From the local daemon: unpair a machine; replies with the shared keys it had seen.
    Unpair(String, Sender<Result<Vec<String>, String>>),
    /// Cut off every machine that joined with this cloud token.
    Kick(Vec<[u8; 32]>),
    /// Every 30 s: cut off sandboxes whose token expired or was revoked.
    Tick,
}

/// Cloud sandboxes that haven't connected for this long are forgotten.
const CLOUD_FORGET_SECS: u64 = 7 * 24 * 3600;

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

const MAX_UNAUTH: usize = 16;
const DELTA_MIN: usize = 4096;

struct Client {
    tx: Sender<Msg>,
    key: [u8; 32],
    name: String,
    ready: bool, // Hello seen and caught up: receives broadcasts
    stream: Option<TcpStream>,
    env: bool,             // may receive the vault keys
    token: Option<String>, // cloud token it joined with: checked again every 30 s
    scope: Option<config::Scope>,
    ro: bool,
}

impl Client {
    fn sees(&self, wt: &str, path: &str) -> bool {
        self.scope.as_ref().is_none_or(|s| config::in_scope(s, wt, path))
    }

    /// Every message to a machine goes through here: a scoped sandbox never hears of a key outside
    /// its scope, and only machines allowed `.env` values get vault keys. False = the machine is gone.
    fn send(&self, m: Msg) -> bool {
        let key = match &m {
            Msg::Op(o) => Some((&o.wt, &o.path)),
            Msg::Reject(r) => Some((&r.wt, &r.path)),
            Msg::Lease(l) => Some((&l.wt, &l.path)),
            Msg::Claim { wt, path, .. } | Msg::Release { wt, path, .. } | Msg::NeedFull { wt, path } => Some((wt, path)),
            Msg::Resync { .. } if self.scope.is_some() => return true, // never sent; no path to check
            Msg::VaultKey { .. } if !self.env => return true,
            Msg::Resync { .. } | Msg::VaultKey { .. } | Msg::Hello { .. } | Msg::CaughtUp | Msg::Ping | Msg::Time { .. } | Msg::Peers(_) => None,
        };
        if key.is_some_and(|(w, p)| !self.sees(w, p)) {
            return true;
        }
        self.tx.send(m).is_ok()
    }
}

struct Core {
    seq: Sequencer,
    origins: HashMap<Key, String>,
    log: File,
    clients: HashMap<u64, Client>,
    leases: lease::Table,
    sessions: HashMap<String, [u8; 32]>, // lease session -> the machine that holds it
}

fn blob_path(h: &Hash) -> std::path::PathBuf {
    config::path(&format!("anchor/blobs/{}", hex(h)))
}

// ponytail: blobs are never collected and the log never compacts; add both when ~/.hilo/anchor grows.
fn read_blob(h: &Hash) -> io::Result<Option<Vec<u8>>> {
    if *h == NONE { Ok(None) } else { std::fs::read(blob_path(h)).map(Some) }
}

fn write_blob(h: &Hash, d: &[u8]) -> io::Result<()> {
    let p = blob_path(h);
    if p.exists() {
        return Ok(());
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, d)?;
    std::fs::rename(tmp, p)
}

/// Replays the log; a torn tail from a crash is cut off.
fn open_log() -> io::Result<(Sequencer, HashMap<Key, String>, File)> {
    let p = config::path("anchor/log2");
    let bytes = std::fs::read(&p).unwrap_or_default();
    let mut seq = Sequencer::default();
    let mut origins = HashMap::new();
    let mut good = 0;
    let mut r = Rd(&bytes);
    while let Some((s, wt, path, h, origin)) = (|| Some((r.u64()?, r.str()?, r.str()?, r.hash()?, r.str()?)))() {
        seq.seq = s;
        seq.state.insert((wt.clone(), path.clone()), (s, h));
        origins.insert((wt, path), origin);
        good = bytes.len() - r.0.len();
    }
    let mut f = OpenOptions::new().create(true).truncate(false).write(true).open(&p)?;
    f.set_len(good as u64)?;
    f.seek(io::SeekFrom::End(0))?;
    Ok((seq, origins, f))
}

/// Checks an op from the network and resolves deltas (the anchor always has the base).
fn validate(mut op: Op) -> Option<Op> {
    if op.wt.starts_with("repo:") {
        let Data::Full(p) = &op.data else { return None };
        let m = parse_meta(p)?;
        return (op.path == META && p.len() <= MAX_MSG && op.new == meta_hash(&m.label, &m.name, &m.origin)).then_some(op);
    }
    if op.path == REF {
        let Data::Full(p) = &op.data else { return None };
        let (sha, _, _) = parse_ref(p)?;
        return (p.len() <= MAX_MSG && op.new == ref_hash(&sha) && !op.wt.starts_with("vault:") && !op.wt.starts_with("drive:")).then_some(op);
    }
    if op.wt.starts_with("vault:") {
        let (file, key) = vault::split_path(&op.path)?;
        let name = file.rsplit('/').next()?;
        let ok = valid_path(file) && is_env_name(name) && vault::valid_key(key);
        let content = op.data.content()?;
        return (ok && content.map_or(0, |d| d.len()) <= 64 * 1024 && hash_of(content) == op.new).then_some(op);
    }
    if !valid_path(&op.path) || excluded(&op.path) {
        return None;
    }
    if let Data::Delta { .. } = op.data {
        let base = read_blob(&op.base).ok()??;
        op.data = Data::Full(op.data.apply_delta(&base)?);
    }
    let content = op.data.content()?;
    (content.map_or(0, |d| d.len() as u64) <= MAX_FILE && hash_of(content) == op.new).then_some(op)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Core {
    fn send_ready(&mut self, m: Msg) {
        self.clients.retain(|_, c| !c.ready || c.send(m.clone()));
    }

    fn to(&self, id: u64, m: Msg) {
        if let Some(c) = self.clients.get(&id) {
            c.send(m);
        }
    }

    /// Drops these machines' connections and tells the rest who is online.
    fn cut(&mut self, f: impl Fn(&Client) -> bool) {
        let ids: Vec<u64> = self.clients.iter().filter(|(_, c)| f(c)).map(|(i, _)| *i).collect();
        if ids.is_empty() {
            return;
        }
        for id in ids {
            if let Some(s) = self.clients.remove(&id).and_then(|c| c.stream) {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
        let m = self.peers_msg();
        self.send_ready(m);
    }

    /// A session's leases belong to the machine (by key, not by its self-chosen name) that holds
    /// them: session ids travel in every Lease, so others could otherwise use up its quota or free them.
    fn owns(&mut self, session: &str, key: &[u8; 32]) -> bool {
        let now = std::time::Instant::now();
        let live = self.leases.0.values().any(|e| e.session == session && e.expires > now);
        match self.sessions.get(session) {
            Some(k) if live => ct_eq(k, key),
            _ => {
                let t = &self.leases.0;
                self.sessions.retain(|s, _| t.values().any(|e| &e.session == s && e.expires > now));
                self.sessions.insert(session.to_string(), *key);
                true
            }
        }
    }

    /// Tells `to` its change lost: `cur` is the version that stands.
    fn reject(&self, to: u64, op: Op, cur: Hash) {
        match read_blob(&cur) {
            Ok(data) => {
                let origin = self.origins.get(&(op.wt.clone(), op.path.clone())).cloned().unwrap_or_default();
                self.to(to, Msg::Reject(Reject { wt: op.wt, path: op.path, rejected: op.new, current: cur, data: Data::from_opt(data), origin }));
            }
            Err(e) => log!("anchor: missing blob for {}: {e}", op.path),
        }
    }

    fn peers_msg(&self) -> Msg {
        let mut names: Vec<String> = self.clients.values().filter(|c| c.ready).map(|c| c.name.clone()).collect();
        names.sort();
        Msg::Peers(names)
    }

    /// An op for `key` with the latest version, as catch-up sends it.
    fn latest(&self, k: &Key, seq: u64, h: Hash, hashes_only: bool) -> io::Result<Op> {
        let data = if h == NONE {
            Data::Absent
        } else if hashes_only && k.1 != REF && !k.0.starts_with("vault:") && !k.0.starts_with("repo:") {
            Data::Omitted
        } else {
            Data::Full(read_blob(&h)?.unwrap_or_default())
        };
        let origin = self.origins.get(k).cloned().unwrap_or_default();
        Ok(Op { seq, wt: k.0.clone(), path: k.1.clone(), base: NONE, new: h, data, origin, t_event: 0, t_seq: 0 })
    }

    /// Orders an op and fans it out. `from` gets a thin copy (it already has the content).
    fn accept(&mut self, mut op: Op, from: u64) {
        let full = match op.data.content() {
            Some(c) => c.map(<[u8]>::to_vec),
            None => return,
        };
        match self.seq.submit(&mut op) {
            Ok(_) => {
                op.t_seq = now_ms();
                let stored = full.as_ref().map_or(Ok(()), |d| write_blob(&op.new, d)).and_then(|_| {
                    let mut w = W::default();
                    w.u64(op.seq).str(&op.wt).str(&op.path).hash(&op.new).str(&op.origin);
                    self.log.write_all(&w.0)
                });
                if let Err(e) = stored {
                    // ponytail: memory and disk now disagree; the anchor exits rather than serve a lie.
                    log!("anchor: cannot persist: {e}");
                    std::process::exit(1);
                }
                self.origins.insert((op.wt.clone(), op.path.clone()), op.origin.clone());
                let is_file = op.path != REF && !op.wt.starts_with("vault:") && !op.wt.starts_with("repo:");
                // The author already has the content: files go back as a hash, branch moves without
                // their bundle. Vault values are tiny and go back whole.
                let mut thin = op.clone();
                thin.data = match &op.data {
                    Data::Full(p) if op.path == REF => parse_ref(p).map_or(Data::Omitted, |(sha, f, _)| Data::Full(crate::sync::ref_payload(&sha, f, None))),
                    Data::Full(_) if is_file || op.path == META => Data::Omitted,
                    d => d.clone(),
                };
                let mut wide = op.clone();
                if let (true, Some(new), true) = (is_file, &full, op.base != NONE) {
                    if new.len() > DELTA_MIN {
                        if let Ok(Some(base)) = read_blob(&op.base) {
                            let d = Data::delta(&base, new);
                            if let Data::Delta { middle, .. } = &d {
                                if middle.len() < new.len() / 2 {
                                    wide.data = d;
                                }
                            }
                        }
                    }
                }
                self.clients.retain(|id, c| !c.ready || c.send(Msg::Op(if *id == from { thin.clone() } else { wide.clone() })));
            }
            Err(cur) => self.reject(from, op, cur),
        }
    }

    fn handle(&mut self, ev: In) {
        match ev {
            In::Connect { id, tx, key, ip, stream } => {
                let (mut env, mut token, mut scope, mut ro) = (true, None, None, false);
                if id != 0 {
                    let now = unix_now();
                    let mut ps = config::peers();
                    // forget cloud sandboxes that are long gone
                    ps.retain(|p| {
                        let seen = p.tag.rsplit(':').next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(now);
                        p.cloud_token().is_none() || ct_eq(&p.key, &key) || now.saturating_sub(seen) < CLOUD_FORGET_SECS
                    });
                    // unpaired or revoked between the handshake and here: nothing to say what it may see
                    let Some(p) = ps.iter_mut().find(|p| ct_eq(&p.key, &key)) else { return refuse(stream, "is no longer paired") };
                    if let Some(tid) = p.cloud_token() {
                        // a known sandbox reconnects without its token: the token's limits come from here
                        match config::cloud_tokens().into_iter().find(|t| t.id == tid) {
                            Some(t) if t.live(now) => (token, scope, ro) = (Some(t.id), t.scope, t.ro),
                            _ => return refuse(stream, &format!("joined with token {tid}, now expired or revoked ({})", p.name)),
                        }
                    }
                    if p.cloud_token().is_none() && !p.ip.is_empty() && p.ip != "-" && p.ip != ip {
                        let msg = format!("machine {} connected from a new address {ip} (was {})", p.name, p.ip);
                        log!("anchor: {msg}");
                        crate::notify(&msg);
                    }
                    p.ip = ip;
                    env = p.gets_env();
                    if let Some(t) = p.tag.strip_prefix("cloud:") {
                        let mut parts: Vec<&str> = t.split(':').collect();
                        parts.truncate(2);
                        p.tag = format!("cloud:{}:{now}", parts.join(":"));
                    }
                    let _ = config::save_peers(&ps);
                }
                self.clients.insert(id, Client { tx, key, name: String::new(), ready: false, stream, env, token, scope, ro });
            }
            In::Kick(keys) => self.cut(|c| keys.iter().any(|k| ct_eq(k, &c.key))),
            In::Tick => {
                let (ts, now) = (config::cloud_tokens(), unix_now());
                self.cut(|c| c.token.as_ref().is_some_and(|id| !ts.iter().any(|t| &t.id == id && t.live(now))));
            }
            In::Gone(id) => {
                if self.clients.remove(&id).is_some_and(|c| c.ready) {
                    let m = self.peers_msg();
                    self.send_ready(m);
                }
            }
            In::Unpair(name, reply) => {
                let _ = reply.send(self.unpair(&name));
            }
            In::Msg(id, m) => self.on_msg(id, m),
        }
    }

    fn on_msg(&mut self, id: u64, m: Msg) {
        let Some(c) = self.clients.get_mut(&id) else { return };
        match m {
            Msg::Hello { last_seq, name } => {
                if c.ready {
                    return;
                }
                c.name = name.chars().filter(|c| c.is_ascii_graphic()).take(64).collect();
                if id != 0 {
                    let mut ps = config::peers();
                    if let Some(p) = ps.iter_mut().find(|p| ct_eq(&p.key, &c.key)).filter(|p| p.name != c.name) {
                        p.name = c.name.clone();
                        let _ = config::save_peers(&ps);
                    }
                }
                for (e, k) in vault::keys() {
                    self.to(id, Msg::VaultKey { epoch: e, key: k });
                }
                for l in self.leases.snapshot() {
                    self.to(id, Msg::Lease(l));
                }
                for (k, seq, h) in self.seq.since(last_seq) {
                    match self.latest(&k, seq, h, true) {
                        Ok(op) => self.to(id, Msg::Op(op)),
                        Err(e) => log!("anchor: missing blob for {}: {e}", k.1),
                    }
                }
                self.to(id, Msg::CaughtUp);
                self.clients.get_mut(&id).unwrap().ready = true;
                let m = self.peers_msg();
                self.send_ready(m);
            }
            // A scoped sandbox's writes outside its scope are dropped without a word: a Reject would
            // carry the current content. A read-only one gets every write turned down, and its claims
            // and releases are ignored (a lease it can't use would only block the writers).
            Msg::Op(_) | Msg::Claim { .. } | Msg::Release { .. } if c.ro => {
                if let Msg::Op(op) = m {
                    if c.sees(&op.wt, &op.path) {
                        let cur = self.seq.current(&op.wt, &op.path);
                        self.reject(id, op, cur);
                    } else {
                        log!("anchor: dropped a change outside the scope of {}: {} {:?}", c.name, op.wt, op.path);
                    }
                }
            }
            // A scoped sandbox may not announce a repo either: META says where the others clone it from.
            Msg::Op(Op { ref wt, ref path, .. }) | Msg::Claim { ref wt, ref path, .. } if !c.sees(wt, path) || (c.scope.is_some() && wt.starts_with("repo:")) => {
                log!("anchor: dropped a change or claim outside the scope of {}: {wt} {path:?}", c.name);
            }
            Msg::Release { ref wt, ref path, .. } if !wt.is_empty() && !c.sees(wt, path) => {
                log!("anchor: dropped a release outside the scope of {}: {wt} {path:?}", c.name);
            }
            Msg::Op(mut op) => {
                op.origin = c.name.clone();
                // A delta on anything but the current version is turned down before its base is read:
                // otherwise the answer would tell whether the anchor holds a blob, from any folder.
                let cur = self.seq.current(&op.wt, &op.path);
                if matches!(op.data, Data::Delta { .. }) && op.base != cur {
                    return self.reject(id, op, cur);
                }
                let path = op.path.clone();
                match validate(op) {
                    Some(op) => self.accept(op, id),
                    None => log!("anchor: dropped invalid change to {path:?} from {}", self.clients[&id].name),
                }
            }
            Msg::NeedFull { wt, path } => {
                let k = (wt, path);
                if let Some((seq, h)) = self.seq.state.get(&k).copied() {
                    if let Ok(op) = self.latest(&k, seq, h, false) {
                        self.to(id, Msg::Op(op));
                    }
                }
            }
            Msg::Resync { wt } => {
                // "repo:*" = every repo announcement (a folder was just linked)
                let hit = |w: &str| w == wt || wt.strip_suffix('*').is_some_and(|p| w.starts_with(p));
                for (k, seq, h) in self.seq.since(0).into_iter().filter(|e| hit(&e.0 .0)) {
                    if let Ok(op) = self.latest(&k, seq, h, true) {
                        self.to(id, Msg::Op(op));
                    }
                }
            }
            Msg::Claim { wt, path, session } => {
                let (machine, key) = (c.name.clone(), c.key);
                if !self.owns(&session, &key) {
                    return log!("anchor: {machine} claimed for a session another machine holds");
                }
                match self.leases.claim(&wt, &path, &session, &machine) {
                    Some(l) => self.send_ready(Msg::Lease(l)),
                    None => {
                        // tell the loser who holds it, so its local copy is right
                        if let Some(l) = self.leases.snapshot().into_iter().find(|l| l.wt == wt && l.path == path) {
                            self.to(id, Msg::Lease(l));
                        }
                    }
                }
            }
            Msg::Release { wt, path, session } => {
                // "every lease of the session" from a scoped sandbox only reaches inside its scope
                let keys: Vec<Key> = if wt.is_empty() { self.leases.0.keys().filter(|k| c.sees(&k.0, &k.1)).cloned().collect() } else { vec![(wt, path)] };
                let (machine, key) = (c.name.clone(), c.key);
                if !self.owns(&session, &key) {
                    return log!("anchor: {machine} released for a session another machine holds");
                }
                for (w, p) in keys {
                    for l in self.leases.release(&w, &p, &session) {
                        self.send_ready(Msg::Lease(l));
                    }
                }
            }
            Msg::Time { t0, .. } => {
                c.send(Msg::Time { t0, t1: now_ms() });
            }
            _ => {}
        }
    }

    fn unpair(&mut self, name: &str) -> Result<Vec<String>, String> {
        let ps = config::peers();
        let (gone, keep): (Vec<Peer>, Vec<Peer>) = ps.into_iter().partition(|p| p.name == name);
        if gone.is_empty() {
            return Err(format!("no paired machine called {name}"));
        }
        config::save_peers(&keep).map_err(|e| e.to_string())?;
        // cut live connections at once
        self.cut(|c| gone.iter().any(|p| ct_eq(&p.key, &c.key)));
        // new vault key; re-seal every shared value with it
        let old = vault::keys();
        let (e, k) = vault::new_epoch().map_err(|e| e.to_string())?;
        self.send_ready(Msg::VaultKey { epoch: e, key: k });
        let keys = vault::keys();
        let mut seen = vec![];
        for ((wt, path), (_, h)) in self.seq.state.clone() {
            let (Some(repo), Some((file, key))) = (wt.strip_prefix("vault:"), vault::split_path(&path)) else { continue };
            let Ok(Some(ct)) = read_blob(&h) else { continue };
            let Some(pt) = vault::open(&old, repo, file, key, &ct) else { continue };
            if !vault::is_mark(&pt) {
                seen.push(key.to_string());
            }
            let Some(sealed) = vault::seal(&keys, repo, file, key, &pt) else { continue };
            let new = crate::crypto::sha256(&sealed);
            let me = self.clients.get(&0).map(|c| c.name.clone()).unwrap_or_default();
            let op = Op { seq: 0, wt: wt.clone(), path: path.clone(), base: h, new, data: Data::Full(sealed), origin: me, t_event: 0, t_seq: 0 };
            self.accept(op, u64::MAX);
        }
        seen.sort();
        seen.dedup();
        log!("anchor: unpaired {name}; vault key rotated to epoch {e}");
        Ok(seen)
    }
}

/// Turns a connection away after the handshake: dropping its channel ends its writer.
fn refuse(stream: Option<TcpStream>, why: &str) {
    log!("anchor: refused a machine that {why}");
    if let Some(s) = stream {
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
}

pub fn start(listen: &str, key: [u8; 32]) -> io::Result<Sender<In>> {
    let (seq, origins, log) = open_log()?;
    log!("anchor: {} changes on record, listening on {listen}", seq.seq);
    let listener = TcpListener::bind(listen)?;
    let (tx, rx): (Sender<In>, Receiver<In>) = channel();
    std::thread::spawn(move || {
        let mut core = Core { seq, origins, log, clients: HashMap::new(), leases: lease::Table::default(), sessions: HashMap::new() };
        for ev in rx {
            core.handle(ev);
        }
    });
    let ttx = tx.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        if ttx.send(In::Tick).is_err() {
            break;
        }
    });
    let atx = tx.clone();
    std::thread::spawn(move || {
        let unauth = Arc::new(AtomicUsize::new(0));
        let ids = Arc::new(AtomicU64::new(1));
        for s in listener.incoming().flatten() {
            if unauth.fetch_add(1, Ordering::SeqCst) >= MAX_UNAUTH {
                unauth.fetch_sub(1, Ordering::SeqCst);
                continue;
            }
            let (atx, unauth, ids) = (atx.clone(), unauth.clone(), ids.clone());
            // each connection is its own thread: a panic there doesn't take the daemon down
            std::thread::spawn(move || serve(s, key, atx, unauth, ids));
        }
    });
    Ok(tx)
}

fn serve(mut s: TcpStream, key: [u8; 32], atx: Sender<In>, unauth: Arc<AtomicUsize>, ids: Arc<AtomicU64>) {
    let peer = s.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    let _ = s.set_nodelay(true);
    let _ = s.set_write_timeout(Some(net::HANDSHAKE_TIMEOUT));
    let tokens = config::active_tokens();
    let now = unix_now();
    let clouds: Vec<config::CloudToken> = config::cloud_tokens().into_iter().filter(|t| t.live(now)).collect();
    let psks: Vec<[u8; 32]> = std::iter::once(net::ZERO_PSK)
        .chain(tokens.iter().map(|t| config::token_psk(t)))
        .chain(clouds.iter().map(|t| config::cloud_psk(&t.secret)))
        .collect();
    let hs = net::respond(&mut s, key, &psks);
    unauth.fetch_sub(1, Ordering::SeqCst);
    let Ok((tx, rx, remote, i)) = hs else {
        log!("anchor: rejected handshake from {peer}");
        return;
    };
    if i == 0 {
        if !config::peers().iter().any(|p| ct_eq(&p.key, &remote)) {
            log!("anchor: unknown machine from {peer}");
            return;
        }
    } else if i <= tokens.len() {
        // the token is burned before anything else happens
        let mut ps = config::peers();
        ps.push(Peer { key: remote, name: "new".into(), ip: peer.clone(), tag: String::new() });
        if config::use_token(&tokens[i - 1]).and_then(|_| config::save_peers(&ps)).is_err() {
            return;
        }
        log!("anchor: paired new machine {} from {peer}", &hex(&remote)[..8]);
    } else {
        // a cloud sandbox: the token stays valid for the next one
        let t = &clouds[i - 1 - tokens.len()];
        let mut ps = config::peers();
        if !ps.iter().any(|p| ct_eq(&p.key, &remote)) {
            let tag = format!("cloud:{}:{}:{}", t.id, t.env as u8, unix_now());
            ps.push(Peer { key: remote, name: format!("cloud-{}", &hex(&remote)[..6]), ip: peer.clone(), tag });
            if config::save_peers(&ps).is_err() {
                return;
            }
        }
        log!("anchor: cloud machine {} joined with token {} ({})", &hex(&remote)[..8], t.id, t.name);
    }
    let _ = s.set_write_timeout(None);
    let id = ids.fetch_add(1, Ordering::SeqCst);
    let (ctx, crx) = channel();
    let (Ok(w), Ok(k)) = (s.try_clone(), s.try_clone()) else { return };
    net::spawn_writer(w, tx, crx);
    let _ = atx.send(In::Connect { id, tx: ctx, key: remote, ip: peer.clone(), stream: Some(k) });
    let _ = net::read_loop(&mut s, rx, |m| atx.send(In::Msg(id, m)).is_ok());
    let _ = atx.send(In::Gone(id));
    let _ = s.shutdown(std::net::Shutdown::Both);
    log!("anchor: {} ({peer}) disconnected", &hex(&remote)[..8]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CloudToken, Scope};

    fn op(wt: &str, path: &str, body: &str) -> Op {
        let d = body.as_bytes().to_vec();
        Op { seq: 0, wt: wt.into(), path: path.into(), base: NONE, new: hash_of(Some(&d)), data: Data::Full(d), origin: String::new(), t_event: 0, t_seq: 0 }
    }

    fn keys(rx: &Receiver<Msg>) -> Vec<(&'static str, String, String)> {
        rx.try_iter()
            .filter_map(|m| match m {
                Msg::Op(o) => Some(("op", o.wt, o.path)),
                Msg::Lease(l) => Some(("lease", l.wt, l.path)),
                Msg::Reject(r) => Some(("reject", r.wt, r.path)),
                _ => None,
            })
            .collect()
    }

    fn has(v: &[(&str, String, String)], kind: &str, wt: &str, path: &str) -> bool {
        v.iter().any(|(k, w, p)| *k == kind && w == wt && p == path)
    }

    /// Phase 2's acceptance test: a sandbox works in one folder without seeing the others.
    /// The only test that touches HILO_HOME in-process; it points it at a fresh temp dir first.
    #[test]
    fn scoped_sandbox_sees_only_its_folder() {
        let home = std::env::temp_dir().join(format!("hilo-anchor-test-{}-{}", std::process::id(), hex(&crate::crypto::random::<4>())));
        for d in ["anchor/blobs", "tmp", "secrets", "vault", "index"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        std::env::set_var("HILO_HOME", &home);
        let now = unix_now();
        let notes = |p: &str| Some(Scope { unit: "drive:notes".into(), prefix: p.into() });
        let tok = |id: &str, scope: Option<Scope>, expires: u64, ro: bool| CloudToken { id: id.into(), secret: id.repeat(32)[..32].into(), name: id.into(), env: false, created: now, revoked: false, scope, expires, ro };
        let tokens = vec![tok("W", None, 0, false), tok("S", notes("a"), now + 3600, false), tok("R", notes("a"), 0, true), tok("X", notes(""), now - 1, false), tok("P", Some(Scope { unit: "repo:R".into(), prefix: String::new() }), 0, false)];
        config::save_cloud_tokens(&tokens).unwrap();
        let peers: Vec<Peer> = ["W", "S", "R", "X", "P"]
            .iter()
            .enumerate()
            .map(|(i, t)| Peer { key: [i as u8 + 1; 32], name: format!("m{t}"), ip: "-".into(), tag: format!("cloud:{t}:0:{now}") })
            .collect();
        config::save_peers(&peers).unwrap();

        let (seq, origins, log) = open_log().unwrap();
        let mut core = Core { seq, origins, log, clients: HashMap::new(), leases: lease::Table::default(), sessions: HashMap::new() };
        let connect = |core: &mut Core, id: u64| {
            let (tx, rx) = channel();
            core.handle(In::Connect { id, tx, key: [id as u8; 32], ip: "-".into(), stream: None });
            rx
        };
        let (w, s, r, x, p) = (1, 2, 3, 4, 5); // writer (no scope), scoped, scoped read-only, expired, one repo
        let wrx = connect(&mut core, w);
        core.handle(In::Msg(w, Msg::Hello { last_seq: 0, name: "w".into() }));
        keys(&wrx);

        // history the sandbox's catch-up must filter, and a lease outside its scope
        for (wt, p) in [("drive:notes", "a/1"), ("drive:notes", "ab/2"), ("drive:notes", "a"), ("drive:other", "a/3"), ("R:main", "a/4"), ("vault:R", "a/.env\0K")] {
            core.handle(In::Msg(w, Msg::Op(op(wt, p, p))));
        }
        core.handle(In::Msg(w, Msg::Claim { wt: "drive:notes".into(), path: "b/secret".into(), session: "s1".into() }));
        core.handle(In::Msg(w, Msg::Claim { wt: "drive:notes".into(), path: "a/1".into(), session: "s1".into() }));

        let srx = connect(&mut core, s);
        core.handle(In::Msg(s, Msg::Hello { last_seq: 0, name: "s".into() }));
        let mut got = keys(&srx);
        assert!(has(&got, "op", "drive:notes", "a/1") && has(&got, "op", "drive:notes", "a") && has(&got, "lease", "drive:notes", "a/1"), "{got:?}");

        // live traffic: broadcasts (thin and wide), leases, NeedFull, Resync, re-seal fan-out
        let mut big = "x".repeat(DELTA_MIN * 2);
        core.handle(In::Msg(w, Msg::Op(op("drive:notes", "a/b/5", &big))));
        big.push('y');
        let mut o = op("drive:notes", "a/b/5", &big);
        o.base = hash_of(Some("x".repeat(DELTA_MIN * 2).as_bytes()));
        core.handle(In::Msg(w, Msg::Op(o))); // goes out as a delta
        core.handle(In::Msg(w, Msg::Op(op("drive:notes", "b/6", "b"))));
        core.handle(In::Msg(w, Msg::Claim { wt: "drive:notes".into(), path: "b/6".into(), session: "s1".into() }));
        core.handle(In::Msg(w, Msg::Release { wt: "drive:notes".into(), path: "b/6".into(), session: "s1".into() }));
        core.handle(In::Msg(s, Msg::NeedFull { wt: "drive:notes".into(), path: "b/6".into() }));
        core.handle(In::Msg(s, Msg::Resync { wt: "drive:*".into() }));
        core.handle(In::Msg(s, Msg::Resync { wt: "repo:*".into() }));
        core.accept(op("drive:notes", "z", "z"), u64::MAX);
        core.accept(op("drive:notes", "a/10", "z"), u64::MAX);
        // the sandbox's own writes: outside its scope dropped without a word, inside accepted
        core.handle(In::Msg(s, Msg::Op(op("drive:notes", "b/7", "evil"))));
        core.handle(In::Msg(s, Msg::Op(op("drive:other", "a/1", "evil"))));
        core.handle(In::Msg(s, Msg::Claim { wt: "drive:notes".into(), path: "b/8".into(), session: "s2".into() }));
        core.handle(In::Msg(s, Msg::Release { wt: String::new(), path: String::new(), session: "s1".into() }));
        core.handle(In::Msg(s, Msg::Op(op("drive:notes", "a/8", "ok"))));

        got.extend(keys(&srx));
        for (_, wt, p) in &got {
            assert!(config::in_scope(&notes("a").unwrap(), wt, p), "scoped sandbox saw {wt} {p:?}");
        }
        for (k, p) in [("op", "a/b/5"), ("op", "a/10"), ("op", "a/8"), ("lease", "a/1")] {
            assert!(has(&got, k, "drive:notes", p), "missing {k} {p}: {got:?}");
        }
        assert!(!got.iter().any(|e| e.0 == "reject"));
        let wgot = keys(&wrx);
        assert!(has(&wgot, "op", "drive:notes", "a/8") && has(&wgot, "op", "drive:notes", "z"));
        assert!(!has(&wgot, "op", "drive:notes", "b/7") && !has(&wgot, "lease", "drive:notes", "b/8"));
        assert_eq!(core.seq.current("drive:notes", "b/7"), NONE);
        assert_eq!(core.seq.current("drive:other", "a/1"), NONE);
        assert!(core.leases.holder("drive:notes", "b/secret").is_some(), "a scoped release reached outside its scope");
        assert!(core.leases.holder("drive:notes", "a/1").is_some_and(|e| e.session == "s1"), "released another machine's session");

        // review: a delta on a guessed base must answer the same whether the anchor holds that blob
        // ("b" is b/6, outside the scope) or not, or a sandbox can read any file a guess at a time
        for guess in ["b", "not on the anchor"] {
            let mut o = op("drive:notes", "a/1", "x");
            o.base = hash_of(Some(guess.as_bytes()));
            o.data = Data::Delta { prefix: 0, suffix: 0, middle: b"x".to_vec() };
            core.handle(In::Msg(s, Msg::Op(o)));
            assert_eq!(keys(&srx), vec![("reject", "drive:notes".to_string(), "a/1".to_string())], "{guess}");
        }

        // review: a sandbox can't use up or free the leases of another machine's session
        let claim = |core: &mut Core, id: u64, path: &str, session: &str| core.handle(In::Msg(id, Msg::Claim { wt: "drive:notes".into(), path: path.into(), session: session.into() }));
        claim(&mut core, w, "b/v", "victim");
        claim(&mut core, w, "a/v", "victim");
        for i in 0..lease::MAX_PER_SESSION {
            claim(&mut core, s, &format!("a/h{i}"), "victim");
        }
        core.handle(In::Msg(s, Msg::Release { wt: String::new(), path: String::new(), session: "victim".into() }));
        core.handle(In::Msg(s, Msg::Release { wt: "drive:notes".into(), path: "a/v".into(), session: "victim".into() }));
        claim(&mut core, w, "c/next", "victim");
        claim(&mut core, s, "a/mine", "own");
        let held = |core: &Core, p: &str| core.leases.holder("drive:notes", p).map(|e| e.session.clone());
        assert_eq!((held(&core, "a/h0"), held(&core, "a/v"), held(&core, "c/next")), (None, Some("victim".into()), Some("victim".into())));
        assert_eq!(held(&core, "a/mine"), Some("own".into()));
        keys(&srx);
        keys(&wrx);

        // a scoped sandbox can't announce its repo (META says where the others clone it from); its files go through
        let prx = connect(&mut core, p);
        core.handle(In::Msg(p, Msg::Hello { last_seq: 0, name: "p".into() }));
        let mut meta = op("repo:R", META, "");
        meta.new = crate::sync::meta_hash("r", "r", "github.com/evil/r");
        meta.data = Data::Full(crate::sync::meta_payload("r", "r", "github.com/evil/r", "main", None));
        core.handle(In::Msg(p, Msg::Op(meta.clone())));
        core.handle(In::Msg(p, Msg::Op(op("R:main", "p/file", "p"))));
        assert_eq!(core.seq.current("repo:R", META), NONE);
        assert_eq!(core.seq.current("R:main", "p/file"), hash_of(Some(b"p".as_slice())));
        core.handle(In::Msg(w, Msg::Op(meta))); // the same op is valid from an unscoped machine
        assert_ne!(core.seq.current("repo:R", META), NONE);
        assert!(keys(&prx).iter().all(|(k, wt, _)| *k == "op" && (wt == "R:main" || wt == "repo:R" || wt == "vault:R")));

        // read-only: an in-scope write is turned down with the current version, never accepted
        let rrx = connect(&mut core, r);
        core.handle(In::Msg(r, Msg::Hello { last_seq: 0, name: "r".into() }));
        keys(&rrx);
        let mut edit = op("drive:notes", "a/1", "ro edit");
        edit.base = core.seq.current("drive:notes", "a/1");
        core.handle(In::Msg(r, Msg::Op(edit)));
        core.handle(In::Msg(r, Msg::Op(op("drive:notes", "a/new", "ro new"))));
        core.handle(In::Msg(r, Msg::Op(op("drive:notes", "b/6", "ro out"))));
        core.handle(In::Msg(r, Msg::Claim { wt: "drive:notes".into(), path: "a/1".into(), session: "s3".into() }));
        let got = keys(&rrx);
        assert_eq!(got, vec![("reject", "drive:notes".to_string(), "a/1".to_string()), ("reject", "drive:notes".to_string(), "a/new".to_string())]);
        assert_eq!(core.seq.current("drive:notes", "a/1"), hash_of(Some(b"a/1".as_slice())));
        assert_eq!(core.seq.current("drive:notes", "a/new"), NONE);
        assert!(core.leases.holder("drive:notes", "a/1").is_some_and(|e| e.session == "s1"));

        // an expired token can't come back over its pinned key; a revoked one is cut on the next tick
        let xrx = connect(&mut core, x);
        assert!(!core.clients.contains_key(&x) && xrx.recv().is_err());
        let mut ts = config::cloud_tokens();
        ts[1].revoked = true;
        config::save_cloud_tokens(&ts).unwrap();
        core.handle(In::Tick);
        assert!(!core.clients.contains_key(&s) && core.clients.contains_key(&w) && core.clients.contains_key(&r));
        let _ = std::fs::remove_dir_all(&home);
    }
}
