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
    env: bool, // may receive the vault keys
}

struct Core {
    seq: Sequencer,
    origins: HashMap<Key, String>,
    log: File,
    clients: HashMap<u64, Client>,
    leases: lease::Table,
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
        return (p.len() <= MAX_MSG && op.new == ref_hash(&sha) && !op.wt.starts_with("vault:")).then_some(op);
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
        self.clients.retain(|_, c| !c.ready || c.tx.send(m.clone()).is_ok());
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
                self.clients.retain(|id, c| !c.ready || c.tx.send(Msg::Op(if *id == from { thin.clone() } else { wide.clone() })).is_ok());
            }
            Err(cur) => {
                let Some(c) = self.clients.get(&from) else { return };
                match read_blob(&cur) {
                    Ok(data) => {
                        let origin = self.origins.get(&(op.wt.clone(), op.path.clone())).cloned().unwrap_or_default();
                        let r = Reject { wt: op.wt, path: op.path, rejected: op.new, current: cur, data: Data::from_opt(data), origin };
                        let _ = c.tx.send(Msg::Reject(r));
                    }
                    Err(e) => log!("anchor: missing blob for {}: {e}", op.path),
                }
            }
        }
    }

    fn handle(&mut self, ev: In) {
        match ev {
            In::Connect { id, tx, key, ip, stream } => {
                let mut env = true;
                if id != 0 {
                    let now = unix_now();
                    let mut ps = config::peers();
                    // forget cloud sandboxes that are long gone
                    ps.retain(|p| {
                        let seen = p.tag.rsplit(':').next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(now);
                        p.cloud_token().is_none() || ct_eq(&p.key, &key) || now.saturating_sub(seen) < CLOUD_FORGET_SECS
                    });
                    if let Some(p) = ps.iter_mut().find(|p| ct_eq(&p.key, &key)) {
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
                    }
                    let _ = config::save_peers(&ps);
                }
                self.clients.insert(id, Client { tx, key, name: String::new(), ready: false, stream, env });
            }
            In::Kick(keys) => {
                let ids: Vec<u64> = self.clients.iter().filter(|(_, c)| keys.iter().any(|k| ct_eq(k, &c.key))).map(|(i, _)| *i).collect();
                for id in ids {
                    if let Some(s) = self.clients.remove(&id).and_then(|c| c.stream) {
                        let _ = s.shutdown(std::net::Shutdown::Both);
                    }
                }
                let m = self.peers_msg();
                self.send_ready(m);
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
                let tx = c.tx.clone();
                if c.env {
                    for (e, k) in vault::keys() {
                        let _ = tx.send(Msg::VaultKey { epoch: e, key: k });
                    }
                }
                for l in self.leases.snapshot() {
                    let _ = tx.send(Msg::Lease(l));
                }
                for (k, seq, h) in self.seq.since(last_seq) {
                    match self.latest(&k, seq, h, true) {
                        Ok(op) => {
                            let _ = tx.send(Msg::Op(op));
                        }
                        Err(e) => log!("anchor: missing blob for {}: {e}", k.1),
                    }
                }
                let _ = tx.send(Msg::CaughtUp);
                self.clients.get_mut(&id).unwrap().ready = true;
                let m = self.peers_msg();
                self.send_ready(m);
            }
            Msg::Op(mut op) => {
                op.origin = c.name.clone();
                let path = op.path.clone();
                match validate(op) {
                    Some(op) => self.accept(op, id),
                    None => log!("anchor: dropped invalid change to {path:?} from {}", self.clients[&id].name),
                }
            }
            Msg::NeedFull { wt, path } => {
                let tx = c.tx.clone();
                let k = (wt, path);
                if let Some((seq, h)) = self.seq.state.get(&k).copied() {
                    if let Ok(op) = self.latest(&k, seq, h, false) {
                        let _ = tx.send(Msg::Op(op));
                    }
                }
            }
            Msg::Resync { wt } => {
                let tx = c.tx.clone();
                // "repo:*" = every repo announcement (a folder was just linked)
                let hit = |w: &str| w == wt || wt.strip_suffix('*').is_some_and(|p| w.starts_with(p));
                for (k, seq, h) in self.seq.since(0).into_iter().filter(|e| hit(&e.0 .0)) {
                    if let Ok(op) = self.latest(&k, seq, h, true) {
                        let _ = tx.send(Msg::Op(op));
                    }
                }
            }
            Msg::Claim { wt, path, session } => {
                let machine = c.name.clone();
                let tx = c.tx.clone();
                match self.leases.claim(&wt, &path, &session, &machine) {
                    Some(l) => self.send_ready(Msg::Lease(l)),
                    None => {
                        // tell the loser who holds it, so its local copy is right
                        if let Some(l) = self.leases.snapshot().into_iter().find(|l| l.wt == wt && l.path == path) {
                            let _ = tx.send(Msg::Lease(l));
                        }
                    }
                }
            }
            Msg::Release { wt, path, session } => {
                for l in self.leases.release(&wt, &path, &session) {
                    self.send_ready(Msg::Lease(l));
                }
            }
            Msg::Time { t0, .. } => {
                let _ = c.tx.send(Msg::Time { t0, t1: now_ms() });
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
        let ids: Vec<u64> = self.clients.iter().filter(|(_, c)| gone.iter().any(|p| ct_eq(&p.key, &c.key))).map(|(i, _)| *i).collect();
        for id in ids {
            if let Some(c) = self.clients.remove(&id) {
                if let Some(s) = c.stream {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
            }
        }
        let m = self.peers_msg();
        self.send_ready(m);
        // new vault key; re-seal every shared value with it
        let old = vault::keys();
        let (e, k) = vault::new_epoch().map_err(|e| e.to_string())?;
        self.clients.retain(|_, c| !c.ready || !c.env || c.tx.send(Msg::VaultKey { epoch: e, key: k }).is_ok());
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

pub fn start(listen: &str, key: [u8; 32]) -> io::Result<Sender<In>> {
    let (seq, origins, log) = open_log()?;
    log!("anchor: {} changes on record, listening on {listen}", seq.seq);
    let listener = TcpListener::bind(listen)?;
    let (tx, rx): (Sender<In>, Receiver<In>) = channel();
    std::thread::spawn(move || {
        let mut core = Core { seq, origins, log, clients: HashMap::new(), leases: lease::Table::default() };
        for ev in rx {
            core.handle(ev);
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
    let clouds: Vec<config::CloudToken> = config::cloud_tokens().into_iter().filter(|t| !t.revoked).collect();
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
