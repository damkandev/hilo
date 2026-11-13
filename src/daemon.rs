//! The per-machine loop: watch worktrees, send changes, apply what the anchor orders,
//! keep commits, envs and leases in step, and answer the CLI, hooks and MCP over a Unix socket.
use crate::anchor::{self, now_ms};
use crate::config::{self, Config};
use crate::crypto::{ct_eq, sha256, unhex};
use crate::fsops::{self, excluded, is_env_name, runs_code, scripts_of, valid_path, MAX_FILE};
use crate::git;
use crate::lease;
use crate::log;
use crate::net;
use crate::scan::{Changes, Scanner};
use crate::sync::{hash_of, meta_hash, meta_payload, parse_meta, parse_ref, ref_hash, ref_payload, Data, Effect, Hash, Key, Op, Reject, Replica, META, NONE, REF};
use crate::vault::{self, Store};
use crate::watch::Watcher;
use crate::wire::{Msg, Rd, MAX_MSG, W};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

const TICK: Duration = Duration::from_millis(250);
const VERIFY: Duration = Duration::from_secs(30); // full walk even with a watcher (FSEvents can drop)
const POLL: Duration = Duration::from_secs(1); // plan B without a watcher
const SAVE: Duration = Duration::from_secs(2);
const TIME_SYNC: Duration = Duration::from_secs(30);

pub enum Ev {
    Up(Sender<Msg>),
    Down,
    Net(Msg),
    Fs(Option<Vec<PathBuf>>),
    Touched(PathBuf),
    Ipc(Vec<String>, Sender<(u8, String)>),
}

/// What the socket threads read without waiting for the main loop (hooks must stay < 5 ms).
pub struct Shared {
    pub name: String,
    pub leases: Mutex<lease::Table>,
    pub roots: RwLock<Vec<(PathBuf, String)>>,
    pub out: Mutex<Option<Sender<Msg>>>,
    pub notices: Mutex<HashMap<String, Vec<String>>>,
    pub authors: Mutex<HashMap<Key, String>>,
    /// messages for every session + how many each session has read
    pub broadcast: Mutex<(Vec<String>, HashMap<String, usize>)>,
}

impl Shared {
    /// (worktree id, relative path) for an absolute path inside a synced worktree.
    pub fn locate(&self, p: &Path) -> Option<(String, String)> {
        let abs = canonical_maybe_missing(p);
        let roots = self.roots.read().unwrap();
        let (root, id) = roots.iter().filter(|(r, _)| abs.starts_with(r)).max_by_key(|(r, _)| r.as_os_str().len())?;
        let rel = abs.strip_prefix(root).ok()?.to_str()?.to_string();
        valid_path(&rel).then(|| (id.clone(), rel))
    }
    pub fn send(&self, m: Msg) {
        if let Some(o) = self.out.lock().unwrap().as_ref() {
            let _ = o.send(m);
        }
    }
    pub fn notices_for(&self, session: &str) -> Vec<String> {
        let mut v = self.notices.lock().unwrap().remove(session).unwrap_or_default();
        let mut b = self.broadcast.lock().unwrap();
        let (msgs, seen) = &mut *b;
        let n = seen.entry(session.to_string()).or_insert(0);
        v.extend(msgs[*n..].iter().cloned());
        *n = msgs.len();
        v
    }
    fn notify_session(&self, session: Option<String>, msg: String) {
        match session {
            Some(s) => self.notices.lock().unwrap().entry(s).or_default().push(msg),
            None => self.broadcast.lock().unwrap().0.push(msg),
        }
    }
}

/// Canonical path even if the file itself doesn't exist yet.
pub fn canonical_maybe_missing(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    match (p.parent(), p.file_name()) {
        (Some(d), Some(f)) => canonical_maybe_missing(d).join(f),
        _ => p.to_path_buf(),
    }
}

struct Repo {
    root: PathBuf,
    id: String,
    common: PathBuf,
    name: String,
    wt_stamp: Option<Option<SystemTime>>,
    label: Option<String>, // the linked folder it lives in, if any
}

/// A linked folder: every repo directly inside it syncs, new ones included.
/// Machines match folders by label (the folder's name, lowercased).
pub fn folder_label(p: &Path) -> String {
    p.file_name().map_or(String::new(), |n| n.to_string_lossy().to_lowercase())
}

struct Wt {
    root: PathBuf,
    gitdir: PathBuf,
    common: PathBuf,
    repo: String,
    branch: Option<String>, // None = detached HEAD: not synced in v1
    scanner: Scanner,
}

impl Wt {
    fn id(&self) -> Option<String> {
        self.branch.as_ref().map(|b| format!("{}:{b}", self.repo))
    }
}

struct Held {
    wt: String,
    path: String,
    data: Data,
    origin: String,
    reason: String,
}

/// Recent contents by hash, so edits travel as deltas.
#[derive(Default)]
struct Cache {
    map: HashMap<Hash, Vec<u8>>,
    order: VecDeque<Hash>,
    size: usize,
}
impl Cache {
    const MAX: usize = 8 << 20;
    fn put(&mut self, h: Hash, d: &[u8]) {
        if d.len() > 1 << 20 || self.map.contains_key(&h) {
            return;
        }
        self.map.insert(h, d.to_vec());
        self.order.push_back(h);
        self.size += d.len();
        while self.size > Self::MAX {
            let Some(old) = self.order.pop_front() else { break };
            self.size -= self.map.remove(&old).map_or(0, |v| v.len());
        }
    }
}

struct D {
    sh: Arc<Shared>,
    r: Replica,
    repos: Vec<Repo>,
    folders: Vec<(PathBuf, String)>,
    last_folders: Instant,
    wts: Vec<Wt>,
    vaults: HashMap<String, Store>,
    vkeys: BTreeMap<u32, [u8; 32]>,
    held: Vec<Held>,
    cache: Cache,
    outstanding: HashMap<Key, u64>,
    needs_resync: HashSet<String>,
    ref_sha: HashMap<String, String>,
    out: Option<Sender<Msg>>,
    caught: bool,
    offset: i64,
    lat: VecDeque<(u64, u64)>,
    peers: Vec<String>,
    readonly: Vec<String>,
    strict: bool,
    mirrors: Option<PathBuf>,
    mirror_on: bool,
    watcher: Option<Watcher>,
    fs_tx: Sender<Option<Vec<PathBuf>>>,
    role: String,
    anchor_addr: String,
    atx: Option<Sender<anchor::In>>,
    dirty: bool,
    warned: HashSet<String>,
    tmp: PathBuf,
}

fn read_disk(root: &Path, rel: &str) -> Option<Vec<u8>> {
    let p = root.join(rel);
    std::fs::symlink_metadata(&p).ok().filter(|m| m.is_file())?;
    std::fs::read(p).ok()
}

fn safe_dir(b: &str) -> String {
    b.replace('/', "__").chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect()
}

fn load_state() -> Replica {
    let bytes = std::fs::read(config::path("state")).unwrap_or_default();
    let mut r = Replica::default();
    let mut rd = Rd(&bytes);
    if let Some(seq) = rd.u64() {
        r.last_seq = seq;
        while let (Some(wt), Some(path), Some(h)) = (rd.str(), rd.str(), rd.hash()) {
            r.confirmed.insert((wt, path), h);
        }
    }
    r
}

fn load_held() -> Vec<Held> {
    let raw = std::fs::read(config::path("held")).unwrap_or_default();
    let mut r = Rd(&raw);
    let mut v = vec![];
    while let Some(h) = (|| Some(Held { wt: r.str()?, path: r.str()?, data: r.data()?, origin: r.str()?, reason: r.str()? }))() {
        v.push(h);
    }
    v
}

impl D {
    fn log_once(&mut self, key: String, msg: String) {
        if self.warned.insert(key) {
            log!("{msg}");
        }
    }

    fn wt_index(&self, id: &str) -> Option<usize> {
        self.wts.iter().position(|w| w.id().as_deref() == Some(id))
    }

    // ---------- linked folders ----------

    fn add_repo(&mut self, root: &Path, label: Option<String>) -> Option<usize> {
        let id = git::repo_id(root).ok()?;
        let common = git::common_dir(root).ok()?;
        if let Some(i) = self.repos.iter().position(|r| r.id == id) {
            return Some(i);
        }
        let name = root.file_name().map_or("repo".into(), |n| n.to_string_lossy().into_owned());
        log!("syncing repo {name} ({})", root.display());
        self.repos.push(Repo { root: root.into(), id, common, name, wt_stamp: None, label });
        let ri = self.repos.len() - 1;
        self.announce(ri);
        self.discover();
        Some(ri)
    }

    /// Picks up new repos in linked folders (one level deep; they need a first commit).
    fn scan_folders(&mut self) {
        self.last_folders = Instant::now();
        // repos deleted from disk: stop watching them (their history stays on the anchor)
        let gone: Vec<String> = self.repos.iter().filter(|r| !r.root.join(".git").exists()).map(|r| r.id.clone()).collect();
        if !gone.is_empty() {
            for r in self.repos.iter().filter(|r| gone.contains(&r.id)) {
                log!("{} is gone; no longer syncing it here", r.root.display());
            }
            self.repos.retain(|r| !gone.contains(&r.id));
            self.wts.retain(|w| !gone.contains(&w.repo));
            self.publish_roots();
            self.rebuild_watcher();
        }
        for (dir, label) in self.folders.clone() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                let p = e.path();
                if !p.join(".git").is_dir() || e.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let Ok(p) = std::fs::canonicalize(&p) else { continue };
                if self.repos.iter().any(|r| r.root == p) {
                    continue;
                }
                if git::repo_id(&p).is_err() {
                    self.log_once(format!("nocommit {}", p.display()), format!("{}: will sync after its first commit", p.display()));
                    continue;
                }
                self.add_repo(&p, Some(label.clone()));
            }
        }
    }

    /// Tells the other machines about a repo in a linked folder, once.
    fn announce(&mut self, ri: usize) {
        if !self.caught {
            return; // after catch-up we know whether someone already announced it
        }
        let Some(label) = self.repos[ri].label.clone() else { return };
        let (root, name, id) = (self.repos[ri].root.clone(), self.repos[ri].name.clone(), self.repos[ri].id.clone());
        let origin = git::origin_url(&root);
        let h = meta_hash(&label, &name, &origin);
        let key = (format!("repo:{id}"), META.to_string());
        // once anyone has announced a repo, nobody re-announces it (folder names may differ per machine)
        if self.r.expected(&key) != NONE || !crate::sync::safe_name(&name) {
            return;
        }
        let branch = git::branch(&git::gitdir(&root).unwrap_or_default()).unwrap_or_else(|| "main".into());
        let bundle = git::bundle_all(&root, &self.tmp, 48 << 20);
        if let Some(op) = self.r.local(&key.0, META, h, Data::Full(meta_payload(&label, &name, &origin, &branch, bundle.as_deref()))) {
            log!("announcing {name} to the other machines");
            self.send_op(op);
        }
    }

    fn on_meta_op(&mut self, op: Op) {
        let id = op.wt.trim_start_matches("repo:").to_string();
        if self.repos.iter().any(|r| r.id == id) {
            let fx = self.r.applied(&op, op.new, Data::Omitted); // we have it: just agree
            debug_assert!(fx.is_empty());
            return;
        }
        let Some(Some(m)) = op.data.content().map(|c| c.and_then(parse_meta)) else { return self.bump(op.seq) };
        if op.new != meta_hash(&m.label, &m.name, &m.origin) {
            return self.bump(op.seq);
        }
        // same folder name, or this machine's only linked folder (e.g. ~/code here, ~/codigo there)
        let only = if self.folders.len() == 1 { self.folders.first() } else { None };
        let Some((dir, _)) = self.folders.iter().find(|f| f.1 == m.label).or(only).cloned() else {
            self.log_once(format!("nofolder {}", m.label), format!("{} has a linked folder \"{}\" this machine doesn't: `hilo add <folder>` to get its repos", op.origin, m.label));
            return self.bump(op.seq);
        };
        let target = dir.join(&m.name);
        if target.exists() {
            if git::repo_id(&target).ok().as_deref() == Some(&id) {
                let _ = self.r.applied(&op, op.new, Data::Omitted);
                let t = std::fs::canonicalize(&target).unwrap_or(target);
                let label = folder_label(&dir);
                self.add_repo(&t, Some(label));
            } else {
                self.log_once(format!("exists {}", target.display()), format!("not cloning {}: {} already exists and is something else", m.name, target.display()));
                self.bump(op.seq);
            }
            return;
        }
        match git::clone_into(&target, &m.branch, &m.origin, m.bundle.as_deref(), &self.tmp) {
            Ok(()) => {
                log!("cloned {} from {} into {}", m.name, op.origin, target.display());
                let _ = self.r.applied(&op, op.new, Data::Omitted);
                let t = std::fs::canonicalize(&target).unwrap_or(target);
                self.add_repo(&t, Some(folder_label(&dir)));
            }
            Err(e) => {
                log!("could not clone {}: {e}", m.name);
                self.bump(op.seq);
            }
        }
    }

    /// `hilo add <path>`: a folder of repos, or one repo.
    fn add_path(&mut self, p: &str) -> (u8, String) {
        let Ok(path) = std::fs::canonicalize(p) else { return (1, format!("{p}: no such folder")) };
        let mut cfg = Config::load();
        if git::toplevel(&path).is_some_and(|t| std::fs::canonicalize(t).ok() == Some(path.clone())) {
            if git::repo_id(&path).is_err() {
                return (1, "that repo needs at least one commit first".into());
            }
            cfg.add("repo", &path.to_string_lossy());
            let _ = cfg.save();
            let label = self.folders.iter().find(|f| path.parent() == Some(f.0.as_path())).map(|f| f.1.clone());
            self.add_repo(&path, label);
            return (0, format!("syncing {}", crate::tui::tilde(&path)));
        }
        if self.folders.iter().any(|f| f.0 == path) {
            return (0, format!("{} is already linked", crate::tui::tilde(&path)));
        }
        let label = folder_label(&path);
        if self.folders.iter().any(|f| f.1 == label) {
            return (1, format!("another linked folder is already called \"{label}\""));
        }
        cfg.add("folder", &path.to_string_lossy());
        let _ = cfg.save();
        self.folders.push((path.clone(), label));
        let before = self.repos.len();
        self.scan_folders();
        self.send(Msg::Resync { wt: "repo:*".into() }); // repos the others already announced
        let _ = before;
        let n = self.repos.iter().filter(|r| r.root.parent() == Some(path.as_path())).count();
        (0, format!("linked {} ({n} repos; new ones sync on their own)", crate::tui::tilde(&path)))
    }

    fn publish_roots(&self) {
        *self.sh.roots.write().unwrap() = self.wts.iter().filter_map(|w| Some((w.root.clone(), w.id()?))).collect();
    }

    fn rebuild_watcher(&mut self) {
        let roots: Vec<PathBuf> = self.wts.iter().map(|w| w.root.clone()).collect();
        self.watcher = None; // drop the old stream first
        self.watcher = Watcher::new(&roots, self.fs_tx.clone());
        if self.watcher.is_none() {
            self.log_once("watcher".into(), "file watcher unavailable; polling every second".into());
        }
    }

    fn save(&mut self) {
        let mut w = W::default();
        // never persist past a change we still have to fetch
        let safe = self.outstanding.values().map(|s| s.saturating_sub(1)).min().unwrap_or(u64::MAX).min(self.r.last_seq);
        w.u64(safe);
        for ((wt, path), h) in &self.r.confirmed {
            w.str(wt).str(path).hash(h);
        }
        let mut h = W::default();
        for x in &self.held {
            h.str(&x.wt).str(&x.path).data(&x.data).str(&x.origin).str(&x.reason);
        }
        let res = config::write_private("state", &w.0).and_then(|_| config::write_private("held", &h.0));
        if let Err(e) = res {
            log!("cannot save state: {e}");
        }
        for wt in &mut self.wts {
            let _ = wt.scanner.save_index();
        }
        self.dirty = false;
    }

    // ---------- outgoing ----------

    fn send(&self, m: Msg) {
        if let Some(o) = &self.out {
            let _ = o.send(m);
        }
    }

    fn send_op(&mut self, mut op: Op) {
        if let Some(Some(d)) = op.data.content() {
            let d = d.to_vec();
            self.cache.put(op.new, &d);
            if op.path != REF && !op.wt.starts_with("vault:") && d.len() > 4096 {
                if let Some(base) = self.cache.map.get(&op.base) {
                    let delta = Data::delta(base, &d);
                    if matches!(&delta, Data::Delta { middle, .. } if middle.len() < d.len() / 2) {
                        op.data = delta;
                    }
                }
            }
        }
        if !self.caught {
            return; // stays pending; resent after catch-up
        }
        op.t_event = (now_ms() as i64 + self.offset) as u64;
        self.send(Msg::Op(op));
    }

    fn local_changes(&mut self, wi: usize, ch: Changes) {
        let Some(id) = self.wts[wi].id() else { return };
        for (path, data) in ch.files {
            if self.held.iter().any(|h| h.wt == id && h.path == path) {
                continue; // waiting for approval: don't overwrite it from here
            }
            let h = hash_of(data.as_deref());
            if let Some(op) = self.r.local(&id, &path, h, Data::from_opt(data)) {
                self.send_op(op);
            }
        }
        for (rel, data, ignored) in ch.env {
            self.on_env(wi, &rel, data, ignored);
        }
        if let Some(w) = &mut self.watcher {
            let root = self.wts[wi].root.clone();
            let dirs: Vec<PathBuf> = ch.dirs.iter().map(|d| root.join(d)).collect();
            let fresh = w.add_dirs(&dirs.iter().map(PathBuf::as_path).collect::<Vec<_>>());
            // inotify race: files created before the watch existed. Look at those dirs once more.
            let rels: Vec<String> = fresh.iter().filter_map(|p| p.strip_prefix(&root).ok()?.to_str().map(str::to_string)).filter(|r| !r.is_empty()).collect();
            if !rels.is_empty() {
                self.scan_wt(wi, Some(&rels));
            }
        }
    }

    fn scan_wt(&mut self, wi: usize, rels: Option<&[String]>) {
        if self.wts[wi].branch.is_none() {
            return;
        }
        let res = match rels {
            None => self.wts[wi].scanner.scan(),
            Some(r) => self.wts[wi].scanner.check(r),
        };
        match res {
            Ok(ch) => self.local_changes(wi, ch),
            Err(e) => {
                let root = self.wts[wi].root.display().to_string();
                self.log_once(format!("scan {root}"), format!("scan {root}: {e}"));
            }
        }
    }

    fn full_scan(&mut self) {
        for wi in 0..self.wts.len() {
            self.scan_wt(wi, None);
        }
    }

    fn on_fs(&mut self, paths: Option<Vec<PathBuf>>) {
        let Some(paths) = paths else { return self.full_scan() };
        let mut by_wt: HashMap<usize, Vec<String>> = HashMap::new();
        for p in paths {
            let best = self.wts.iter().enumerate().filter(|(_, w)| p.starts_with(&w.root)).max_by_key(|(_, w)| w.root.as_os_str().len());
            let Some((wi, w)) = best else { continue };
            let Some(rel) = p.strip_prefix(&w.root).ok().and_then(|r| r.to_str()) else { continue };
            if rel.split('/').next().is_some_and(fsops::is_dotgit) {
                continue; // refs and HEAD are checked every tick
            }
            by_wt.entry(wi).or_default().push(rel.to_string());
        }
        for (wi, rels) in by_wt {
            self.scan_wt(wi, Some(&rels));
        }
    }

    // ---------- worktrees ----------

    fn discover(&mut self) {
        let mut changed = false;
        for ri in 0..self.repos.len() {
            let stamp = std::fs::metadata(self.repos[ri].common.join("worktrees")).and_then(|m| m.modified()).ok();
            if self.repos[ri].wt_stamp == Some(stamp) {
                continue;
            }
            self.repos[ri].wt_stamp = Some(stamp);
            let (root, id) = (self.repos[ri].root.clone(), self.repos[ri].id.clone());
            let Ok(list) = git::worktrees(&root) else { continue };
            let roots: Vec<PathBuf> = list.iter().filter_map(|(p, _)| std::fs::canonicalize(p).ok()).collect();
            let before = self.wts.len();
            self.wts.retain(|w| w.repo != id || roots.contains(&w.root));
            changed |= before != self.wts.len();
            for r in roots {
                if !self.wts.iter().any(|w| w.root == r) {
                    self.add_wt(&r, &id);
                    changed = true;
                }
            }
        }
        if changed {
            self.publish_roots();
            self.rebuild_watcher();
        }
    }

    fn add_wt(&mut self, root: &Path, repo: &str) -> Option<usize> {
        let gitdir = git::gitdir(root).ok()?;
        let common = git::common_dir(root).ok()?;
        let branch = git::branch(&gitdir);
        let mut scanner = Scanner::new(root);
        let wt = Wt { root: root.into(), gitdir, common, repo: repo.into(), branch, scanner: Scanner::new(root) };
        if let Some(id) = wt.id() {
            // index entries the replica doesn't agree with get re-read
            let stale: Vec<String> =
                scanner.files.iter().filter(|(rel, (_, h))| self.r.confirmed.get(&(id.clone(), rel.to_string())) != Some(h)).map(|(r, _)| r.clone()).collect();
            stale.iter().for_each(|r| scanner.forget(r));
            if !self.r.confirmed.keys().any(|k| k.0 == id) {
                self.needs_resync.insert(id.clone());
            }
        }
        log!("watching {} ({})", root.display(), wt.branch.as_deref().unwrap_or("detached HEAD, paused"));
        self.wts.push(Wt { scanner, ..wt });
        let wi = self.wts.len() - 1;
        self.publish_roots();
        self.materialize_all(wi);
        self.resync_pending();
        self.scan_wt(wi, None);
        Some(wi)
    }

    fn resync_pending(&mut self) {
        if !self.caught {
            return;
        }
        for wt in std::mem::take(&mut self.needs_resync) {
            self.send(Msg::Resync { wt });
        }
    }

    /// Branch switches, new commits, new repos in linked folders.
    fn tick(&mut self) {
        if self.last_folders.elapsed() >= Duration::from_secs(2) {
            self.scan_folders();
        }
        if self.caught {
            for ri in 0..self.repos.len() {
                self.announce(ri);
            }
        }
        self.discover();
        for wi in 0..self.wts.len() {
            let b = git::branch(&self.wts[wi].gitdir);
            if b != self.wts[wi].branch {
                // ponytail: one tick of grace for a checkout to settle; watch index.lock if checkouts race scans.
                log!("{}: branch is now {}", self.wts[wi].root.display(), b.as_deref().unwrap_or("detached (paused)"));
                let root = self.wts[wi].root.clone();
                self.wts[wi].branch = b;
                self.wts[wi].scanner = Scanner::new(&root);
                if let Some(id) = self.wts[wi].id() {
                    if !self.r.confirmed.keys().any(|k| k.0 == id) {
                        self.needs_resync.insert(id);
                    }
                }
                self.publish_roots();
                self.resync_pending();
                continue;
            }
            self.check_ref(wi);
        }
    }

    fn check_ref(&mut self, wi: usize) {
        let (Some(id), Some(branch)) = (self.wts[wi].id(), self.wts[wi].branch.clone()) else { return };
        let Some(tip) = git::read_ref(&self.wts[wi].common, &branch) else { return };
        let h = ref_hash(&tip);
        if h == self.r.expected(&(id.clone(), REF.into())) {
            return;
        }
        // a commit never travels before its files
        self.scan_wt(wi, None);
        let root = self.wts[wi].root.clone();
        let old = self.ref_sha.get(&id).cloned();
        let exclude: Vec<String> = match &old {
            Some(o) => vec![o.clone()],
            None => self.wts.iter().filter(|w| w.repo == self.wts[wi].repo).filter_map(|w| self.ref_sha.get(&w.id()?).cloned()).collect(),
        };
        let bundle = match git::bundle(&root, &tip, &exclude, &self.tmp) {
            Ok(b) => b.filter(|b| b.len() < MAX_MSG - (1 << 20)),
            Err(e) => {
                log!("bundle for {branch}: {e}");
                None
            }
        };
        let forced = old.as_ref().is_some_and(|o| !git::is_ancestor(&root, o, &tip));
        if let Some(op) = self.r.local(&id, REF, h, Data::Full(ref_payload(&tip, forced, bundle.as_deref()))) {
            log!("{branch}: sending {} {}", &tip[..7], if forced { "(rewritten)" } else { "" });
            self.send_op(op);
        }
    }

    /// A branch another machine has and this one doesn't: create a worktree for it.
    fn mirror(&mut self, wt_id: &str, sha: &str, bundle: Option<&[u8]>) -> Option<usize> {
        if !self.mirror_on {
            return None;
        }
        let (repo_id, branch) = wt_id.split_once(':')?;
        let ri = self.repos.iter().position(|r| r.id == repo_id)?;
        // default: `<repo>.worktrees/` beside the repo; `mirrors = <dir>` puts them in <dir>/<repo>/
        let base = match &self.mirrors {
            Some(dir) => dir.join(safe_dir(&self.repos[ri].name)),
            None => self.repos[ri].root.parent()?.join(format!("{}.worktrees", self.repos[ri].name)),
        };
        if !git::valid_branch(branch) {
            self.log_once(format!("badbranch {branch}"), format!("not mirroring branch {branch:?}: invalid name"));
            return None;
        }
        let root = self.repos[ri].root.clone();
        let path = base.join(safe_dir(branch));
        if path.exists() {
            self.log_once(format!("mirror {}", path.display()), format!("not mirroring {branch}: {} already exists", path.display()));
            return None;
        }
        if !git::has_commit(&root, sha) {
            if let Some(b) = bundle {
                if let Err(e) = git::fetch_bundle(&root, b, &self.tmp) {
                    log!("mirror {branch}: {e}");
                }
            }
        }
        let made = std::fs::create_dir_all(path.parent()?).and_then(|_| git::worktree_add(&root, &path, branch, sha));
        if let Err(e) = made {
            self.log_once(format!("mirror {branch}"), format!("mirror {branch}: {e}"));
            return None;
        }
        log!("mirrored branch {branch} at {}", path.display());
        let canon = std::fs::canonicalize(&path).ok()?;
        self.repos[ri].wt_stamp = None; // re-list worktrees next tick
        let wi = self.add_wt(&canon, repo_id)?;
        self.rebuild_watcher();
        Some(wi)
    }

    // ---------- incoming ----------

    fn bump(&mut self, seq: u64) {
        self.r.last_seq = self.r.last_seq.max(seq);
    }

    fn on_op(&mut self, op: Op) {
        self.dirty = true;
        if op.t_event > 0 && op.t_seq > 0 && op.origin != self.sh.name {
            let now = (now_ms() as i64 + self.offset) as u64;
            self.lat.push_back((op.t_seq.saturating_sub(op.t_event), now.saturating_sub(op.t_seq)));
            if self.lat.len() > 1000 {
                self.lat.pop_front();
            }
        }
        if op.wt.starts_with("repo:") {
            self.on_meta_op(op)
        } else if op.wt.starts_with("vault:") {
            self.on_vault_op(op)
        } else if op.path == REF {
            self.on_ref_op(op)
        } else {
            self.on_file_op(op)
        }
    }

    fn on_file_op(&mut self, mut op: Op) {
        let Some(wi) = self.wt_index(&op.wt) else { return self.bump(op.seq) };
        if !valid_path(&op.path) || excluded(&op.path) {
            log!("refused remote change to {:?}", op.path);
            return self.bump(op.seq);
        }
        let k = (op.wt.clone(), op.path.clone());
        let root = self.wts[wi].root.clone();
        let disk = read_disk(&root, &op.path);
        let dh = hash_of(disk.as_deref());
        if self.r.needs_content(&op, dh) && op.data.content().is_none() {
            let base = self.cache.map.get(&op.base).cloned().or_else(|| (dh == op.base).then(|| disk.clone()).flatten());
            let full = base.and_then(|b| op.data.apply_delta(&b)).filter(|d| sha256(d) == op.new);
            match full {
                Some(d) => op.data = Data::Full(d),
                None => {
                    self.outstanding.insert(k.clone(), op.seq);
                    return self.send(Msg::NeedFull { wt: k.0, path: k.1 });
                }
            }
        }
        if let Some(c) = op.data.content() {
            if hash_of(c) != op.new || c.map_or(0, |d| d.len() as u64) > MAX_FILE {
                log!("refused remote change to {:?}: bad content", op.path);
                return self.bump(op.seq);
            }
            if let Some(d) = c {
                self.cache.put(op.new, d);
            }
        }
        self.outstanding.remove(&k);
        let fx = self.r.applied(&op, dh, Data::from_opt(disk));
        self.apply_file(wi, &op.path, fx, &op.origin, false);
    }

    fn hold_reason(&self, wi: usize, path: &str, origin: &str, new: Option<&[u8]>) -> Option<String> {
        let id = self.wts[wi].id()?;
        if self.held.iter().any(|h| h.wt == id && h.path == path) {
            return Some("a newer version of a change already waiting".into());
        }
        if !origin.is_empty() && self.readonly.iter().any(|r| r == origin) {
            return Some(format!("{origin} is trusted read-only"));
        }
        if !runs_code(path) {
            return None;
        }
        if path.rsplit('/').next() == Some("package.json") {
            let disk = read_disk(&self.wts[wi].root, path);
            if scripts_of(disk.as_deref()) == scripts_of(new) {
                return None;
            }
            return Some("changes the scripts in package.json".into());
        }
        Some("this path runs code".into())
    }

    fn hold(&mut self, wt: String, path: String, data: Data, origin: &str, reason: String) {
        self.held.retain(|h| !(h.wt == wt && h.path == path));
        let msg = format!(
            "hilo: a change to {path} from {} is waiting for approval ({reason}). Ask the human to run `hilo approve`.",
            if origin.is_empty() { "another machine" } else { origin }
        );
        log!("{msg}");
        crate::notify(&format!("{path}: change waiting for `hilo approve`"));
        self.sh.notify_session(None, msg);
        self.held.push(Held { wt, path, data, origin: origin.into(), reason });
        self.dirty = true;
    }

    fn apply_file(&mut self, wi: usize, path: &str, fx: Vec<Effect>, origin: &str, approved: bool) {
        let root = self.wts[wi].root.clone();
        let branch = self.wts[wi].branch.clone().unwrap_or_else(|| "detached".into());
        let id = self.wts[wi].id().unwrap_or_default();
        for e in fx {
            match e {
                Effect::Save(d) => {
                    let Some(c) = d.content() else { continue };
                    if git::in_head(&root, path, c) {
                        continue; // already in a commit: nothing to lose
                    }
                    match git::save_conflict(&root, &branch, path, c, &self.tmp) {
                        Ok(b) => {
                            let who = if origin.is_empty() { "another machine" } else { origin };
                            log!("conflict on {path}: {who} won; this machine's version is on {b}");
                            let author = self.sh.authors.lock().unwrap().get(&(id.clone(), path.into())).cloned();
                            self.sh.notify_session(
                                author,
                                format!("hilo: while you were editing {path}, an agent on {who} saved it first. Your version is on branch {b} (`git show {b}:{path}`); the file now has theirs. Re-read it before editing again."),
                            );
                        }
                        Err(e) => {
                            // never overwrite what we could not save
                            log!("conflict on {path}: could not save this machine's version ({e}); leaving the file alone");
                            return;
                        }
                    }
                }
                Effect::Write(d) => {
                    let Some(c) = d.content() else { continue };
                    if !approved {
                        if let Some(reason) = self.hold_reason(wi, path, origin, c) {
                            self.hold(id.clone(), path.into(), d.clone(), origin, reason);
                            continue;
                        }
                    }
                    match fsops::write(&root, path, c) {
                        Ok(st) => self.wts[wi].scanner.note(path, st, hash_of(c)),
                        Err(e) => log!("could not write {path}: {e}"),
                    }
                }
            }
        }
    }

    fn on_reject(&mut self, rej: Reject) {
        self.dirty = true;
        if rej.wt.starts_with("repo:") {
            let _ = self.r.rejected(&rej, rej.current, Data::Omitted);
            return;
        }
        if let Some(repo) = rej.wt.strip_prefix("vault:").map(str::to_string) {
            let Some((file, key)) = vault::split_path(&rej.path).map(|(a, b)| (a.to_string(), b.to_string())) else { return };
            if !self.repos.iter().any(|r| r.id == repo) {
                return;
            }
            let disk = self.vault(&repo).current(&file, &key).cloned();
            let fx = self.r.rejected(&rej, hash_of(disk.as_deref()), Data::from_opt(disk));
            return self.apply_vault(&repo, &file, &key, fx, &rej.origin, false);
        }
        let Some(wi) = self.wt_index(&rej.wt) else { return };
        if rej.path == REF {
            if let Some(Some((sha, _, _))) = rej.data.content().map(|c| c.and_then(parse_ref)) {
                self.ref_sha.insert(rej.wt.clone(), sha);
            }
            let (dh, disk) = self.ref_disk(wi);
            let fx = self.r.rejected(&rej, dh, disk);
            return self.apply_ref(wi, fx, &rej.origin, false);
        }
        if !valid_path(&rej.path) || excluded(&rej.path) || hash_of(rej.data.content().flatten()) != rej.current {
            return;
        }
        let disk = read_disk(&self.wts[wi].root, &rej.path);
        let fx = self.r.rejected(&rej, hash_of(disk.as_deref()), Data::from_opt(disk));
        self.apply_file(wi, &rej.path, fx, &rej.origin, false);
    }

    // ---------- commits ----------

    fn ref_disk(&self, wi: usize) -> (Hash, Data) {
        let w = &self.wts[wi];
        match w.branch.as_ref().and_then(|b| git::read_ref(&w.common, b)) {
            Some(t) => (ref_hash(&t), Data::Full(ref_payload(&t, false, None))),
            None => (NONE, Data::Absent),
        }
    }

    fn on_ref_op(&mut self, op: Op) {
        let Some(Some((sha, _, bundle))) = op.data.content().map(|c| c.and_then(parse_ref)) else { return self.bump(op.seq) };
        if op.new != ref_hash(&sha) {
            return self.bump(op.seq);
        }
        let wi = match self.wt_index(&op.wt) {
            Some(i) => i,
            None => match self.mirror(&op.wt, &sha, bundle.as_deref()) {
                Some(i) => i,
                None => return self.bump(op.seq),
            },
        };
        self.ref_sha.insert(op.wt.clone(), sha);
        let (dh, disk) = self.ref_disk(wi);
        let fx = self.r.applied(&op, dh, disk);
        self.apply_ref(wi, fx, &op.origin, false);
    }

    fn apply_ref(&mut self, wi: usize, fx: Vec<Effect>, origin: &str, approved: bool) {
        let root = self.wts[wi].root.clone();
        let id = self.wts[wi].id().unwrap_or_default();
        let branch = self.wts[wi].branch.clone().unwrap_or_default();
        for e in fx {
            let Some(Some((sha, _, bundle))) = (match &e {
                Effect::Save(d) | Effect::Write(d) => d.content().map(|c| c.and_then(parse_ref)),
            }) else {
                continue;
            };
            match e {
                Effect::Save(_) => {
                    let winner = self.ref_sha.get(&id).cloned().unwrap_or_default();
                    if git::is_ancestor(&root, &sha, &winner) {
                        continue; // nothing of ours is lost
                    }
                    match git::save_conflict_commit(&root, &branch, &sha) {
                        Ok(b) => log!("{branch}: another machine committed first; this machine's commit {} is on {b}", &sha[..7]),
                        Err(e) => {
                            log!("{branch}: could not keep commit {}: {e}; leaving the branch alone", &sha[..7]);
                            return;
                        }
                    }
                }
                Effect::Write(d) => {
                    if !approved && !origin.is_empty() && self.readonly.iter().any(|r| r == origin) {
                        self.hold(id.clone(), REF.into(), d, origin, format!("{origin} is trusted read-only"));
                        continue;
                    }
                    if !git::has_commit(&root, &sha) {
                        if let Some(b) = &bundle {
                            if let Err(e) = git::fetch_bundle(&root, b, &self.tmp) {
                                log!("{branch}: {e}");
                            }
                        }
                    }
                    if !git::has_commit(&root, &sha) {
                        log!("{branch}: commit {} is missing here (try `git fetch`)", &sha[..7]);
                        return;
                    }
                    if git::read_ref(&self.wts[wi].common, &branch).as_deref() != Some(&sha) {
                        match git::reset_mixed(&root, &sha) {
                            Ok(()) => log!("{branch} -> {}", &sha[..7]),
                            Err(e) => log!("{branch}: {e}"),
                        }
                    }
                }
            }
        }
    }

    // ---------- envs ----------

    fn vault(&mut self, repo: &str) -> &mut Store {
        self.vaults.entry(repo.to_string()).or_insert_with(|| Store::load(repo))
    }

    fn save_vault(&mut self, repo: &str) {
        if let Err(e) = self.vault(repo).save() {
            log!("cannot save vault: {e}");
        }
    }

    fn on_env(&mut self, wi: usize, rel: &str, data: Option<Vec<u8>>, ignored: bool) {
        let repo = self.wts[wi].repo.clone();
        let root_s = self.wts[wi].root.to_string_lossy().to_string();
        let mk = (root_s.clone(), rel.to_string());
        let Some(data) = data else {
            self.vault(&repo).materialized.remove(&mk);
            return;
        };
        if !ignored {
            if self.vault(&repo).materialized.remove(&mk) {
                let _ = fsops::write(&self.wts[wi].root, rel, None);
                log!("removed {rel} in {root_s}: git no longer ignores it, so it could end up in a commit");
                self.save_vault(&repo);
            } else {
                self.log_once(format!("envnotignored {root_s}/{rel}"), format!("not syncing {rel} in {root_s}: git does not ignore it"));
            }
            return;
        }
        let wanted: BTreeMap<String, String> = vault::parse(&String::from_utf8_lossy(&data)).into_iter().collect();
        let keys = self.vkeys.clone();
        let eff = self.vault(&repo).effective(&keys, rel, false);
        let mut changed = false;
        for (k, v) in &wanted {
            if eff.get(k) != Some(v) {
                changed |= self.set_key(&repo, rel, k, v).is_ok();
            }
        }
        for k in eff.keys().filter(|k| !wanted.contains_key(*k)) {
            changed |= self.unset_key(&repo, rel, k).is_ok();
        }
        if changed {
            self.materialize(&repo, rel, Some(wi));
        }
    }

    fn set_key(&mut self, repo: &str, file: &str, k: &str, v: &str) -> Result<(), String> {
        let keys = self.vkeys.clone();
        let st = self.vault(repo);
        if let Some((_, val)) = st.local.get_mut(&(file.to_string(), k.to_string())) {
            *val = v.to_string();
            self.save_vault(repo);
            return Ok(());
        }
        let ct = vault::seal(&keys, repo, file, k, v.as_bytes()).ok_or("no vault key yet (is this machine paired?)")?;
        st.set_shared(file, k, Some(ct.clone()));
        self.save_vault(repo);
        if let Some(op) = self.r.local(&vault::wt_of(repo), &vault::key_path(file, k), sha256(&ct), Data::Full(ct)) {
            self.send_op(op);
        }
        Ok(())
    }

    fn unset_key(&mut self, repo: &str, file: &str, k: &str) -> Result<(), String> {
        let st = self.vault(repo);
        if st.local.remove(&(file.to_string(), k.to_string())).is_none() {
            st.set_shared(file, k, None);
            if let Some(op) = self.r.local(&vault::wt_of(repo), &vault::key_path(file, k), NONE, Data::Absent) {
                self.send_op(op);
            }
        }
        self.save_vault(repo);
        Ok(())
    }

    fn on_vault_op(&mut self, op: Op) {
        let Some(repo) = op.wt.strip_prefix("vault:").map(str::to_string) else { return };
        let Some((file, key)) = vault::split_path(&op.path).map(|(a, b)| (a.to_string(), b.to_string())) else { return self.bump(op.seq) };
        let ok = self.repos.iter().any(|r| r.id == repo)
            && valid_path(&file)
            && is_env_name(file.rsplit('/').next().unwrap_or(""))
            && vault::valid_key(&key)
            && op.data.content().is_some_and(|c| hash_of(c) == op.new);
        if !ok {
            return self.bump(op.seq);
        }
        let disk = self.vault(&repo).current(&file, &key).cloned();
        let fx = self.r.applied(&op, hash_of(disk.as_deref()), Data::from_opt(disk));
        self.apply_vault(&repo, &file, &key, fx, &op.origin, false);
    }

    fn apply_vault(&mut self, repo: &str, file: &str, key: &str, fx: Vec<Effect>, origin: &str, approved: bool) {
        let keys = self.vkeys.clone();
        for e in fx {
            match e {
                Effect::Save(d) => {
                    if let Some(Some(ct)) = d.content() {
                        let ct = ct.to_vec();
                        self.vault(repo).remember(file, key, Some(ct));
                        log!("vault: {origin} changed {key} first; this machine's value is in `hilo env history`");
                    }
                }
                Effect::Write(d) => {
                    if !approved && !origin.is_empty() && self.readonly.iter().any(|r| r == origin) {
                        self.hold(vault::wt_of(repo), vault::key_path(file, key), d, origin, format!("{origin} is trusted read-only"));
                        continue;
                    }
                    let Some(ct) = d.content().map(|c| c.map(<[u8]>::to_vec)) else { continue };
                    let st = self.vault(repo);
                    let prev = st.current(file, key).and_then(|c| vault::open(&keys, repo, file, key, c)).filter(|p| !vault::is_mark(p));
                    let now_local = ct.as_ref().and_then(|c| vault::open(&keys, repo, file, key, c)).is_some_and(|p| p == vault::LOCAL_MARK);
                    let lk = (file.to_string(), key.to_string());
                    if now_local && !st.local.contains_key(&lk) {
                        if let Some(p) = prev {
                            st.local.insert(lk, (false, String::from_utf8_lossy(&p).into_owned()));
                        }
                    }
                    st.set_shared(file, key, ct);
                }
            }
        }
        self.save_vault(repo);
        self.materialize(repo, file, None);
    }

    fn materialize_all(&mut self, wi: usize) {
        let repo = self.wts[wi].repo.clone();
        for f in self.vault(&repo).files() {
            self.materialize_one(&repo, &f, wi);
        }
        self.save_vault(&repo);
    }

    fn materialize(&mut self, repo: &str, file: &str, skip: Option<usize>) {
        for wi in 0..self.wts.len() {
            if self.wts[wi].repo == repo && Some(wi) != skip {
                self.materialize_one(repo, file, wi);
            }
        }
        self.save_vault(repo);
    }

    /// Writes `file` in worktree `wi` so it holds the effective values. Only where git ignores it.
    fn materialize_one(&mut self, repo: &str, file: &str, wi: usize) {
        if self.strict || self.wts[wi].branch.is_none() {
            return;
        }
        let keys = self.vkeys.clone();
        let values = self.vault(repo).effective(&keys, file, false);
        let root = self.wts[wi].root.clone();
        let p = root.join(file);
        if !p.parent().is_some_and(Path::is_dir) || (values.is_empty() && !p.exists()) {
            return;
        }
        match self.wts[wi].scanner.is_ignored(file) {
            Ok(true) => {}
            _ => {
                return self.log_once(format!("envnotignored {}/{file}", root.display()), format!("not writing {file} in {}: git does not ignore it", root.display()));
            }
        }
        let existing = std::fs::read_to_string(&p).unwrap_or_default();
        let text = vault::render(&existing, &values);
        if text == existing {
            return;
        }
        match fsops::write_mode(&root, file, Some(text.as_bytes()), 0o600) {
            Ok(st) => {
                self.wts[wi].scanner.note(file, st, sha256(text.as_bytes()));
                self.vault(repo).materialized.insert((root.to_string_lossy().into(), file.into()));
            }
            Err(e) => log!("could not write {file}: {e}"),
        }
    }

    // ---------- the socket's slow path ----------

    fn wt_for_cwd(&self, cwd: &str) -> Option<usize> {
        let c = canonical_maybe_missing(Path::new(cwd));
        self.wts.iter().enumerate().filter(|(_, w)| c.starts_with(&w.root)).max_by_key(|(_, w)| w.root.as_os_str().len()).map(|(i, _)| i)
    }

    fn ipc(&mut self, a: Vec<String>) -> (u8, String) {
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        match a.as_slice() {
            ["status", rest @ ..] => (0, self.status(rest.contains(&"--latency"))),
            ["add", path] => self.add_path(path),
            ["progress"] => {
                let pending = self.r.caught_up().len();
                (0, format!("pending={pending} caught={} repos={} worktrees={} online={}", self.caught as u8, self.repos.len(), self.wts.len(), self.peers.join(",")))
            }
            ["env", cwd, file, cmd, args @ ..] => match self.env_cmd(cwd, file, cmd, args) {
                Ok(s) => (0, s),
                Err(e) => (1, e),
            },
            ["approve", args @ ..] => self.approve(args),
            ["trust", name, mode] => {
                let mut cfg = Config::load();
                let mut ro: Vec<String> = cfg.all("readonly").into_iter().map(str::to_string).filter(|n| n != name).collect();
                if *mode == "read-only" {
                    ro.push(name.to_string());
                }
                cfg.remove("readonly");
                for r in &ro {
                    cfg.add("readonly", r);
                }
                let _ = cfg.save();
                self.readonly = ro;
                (0, format!("{name}: {mode}"))
            }
            ["revoke", id] => {
                let mut ts = config::cloud_tokens();
                let Some(t) = ts.iter_mut().find(|t| t.id == *id) else { return (1, format!("no cloud token {id}")) };
                t.revoked = true;
                let _ = config::save_cloud_tokens(&ts);
                let (gone, keep): (Vec<config::Peer>, Vec<config::Peer>) = config::peers().into_iter().partition(|p| p.cloud_token() == Some(*id));
                let _ = config::save_peers(&keep);
                if let Some(atx) = &self.atx {
                    let _ = atx.send(anchor::In::Kick(gone.iter().map(|p| p.key).collect()));
                }
                (0, format!("token {id} revoked; {} cloud machine(s) cut off", gone.len()))
            }
            ["unpair", name] => {
                let Some(atx) = &self.atx else { return (1, "run this on the anchor".into()) };
                let (tx, rx) = channel();
                let _ = atx.send(anchor::In::Unpair(name.to_string(), tx));
                match rx.recv_timeout(Duration::from_secs(30)) {
                    Ok(Ok(keys)) if keys.is_empty() => (0, format!("{name} unpaired. Vault key rotated.")),
                    Ok(Ok(keys)) => (0, format!("{name} unpaired. Vault key rotated.\nIt had seen these shared keys; rotate them at their source:\n  {}", keys.join("\n  "))),
                    Ok(Err(e)) => (1, e),
                    Err(_) => (1, "anchor did not answer".into()),
                }
            }
            _ => (1, "unknown request".into()),
        }
    }

    fn status(&self, latency: bool) -> String {
        let mut s = String::new();
        let conn = if self.out.is_some() && self.caught { "in sync" } else if self.out.is_some() { "catching up" } else { "offline" };
        s += &format!("machine   {} ({})\n", self.sh.name, self.role);
        if self.role == "member" {
            s += &format!("anchor    {} [{conn}]\n", self.anchor_addr);
        }
        s += &format!("online    {}\n", if self.peers.is_empty() { "-".into() } else { self.peers.join(", ") });
        for (f, _) in &self.folders {
            let n = self.repos.iter().filter(|r| r.root.parent() == Some(f.as_path())).count();
            s += &format!("folder    {} ({n} repos; new repos here sync on their own)\n", crate::tui::tilde(f));
        }
        s += "worktrees\n";
        for w in &self.wts {
            let pend = w.id().map_or(0, |id| self.r.caught_up().iter().filter(|o| o.wt == id).count());
            s += &format!(
                "  {} [{}]{}\n",
                w.root.display(),
                w.branch.as_deref().unwrap_or("detached, paused"),
                if pend > 0 { format!(" {pend} changes waiting to send") } else { String::new() }
            );
        }
        let leases = self.sh.leases.lock().unwrap().snapshot();
        s += &format!("leases    {}\n", leases.len());
        for l in leases {
            s += &format!("  {} — {} (session {}, {} s left)\n", l.path, l.machine, l.session.chars().take(4).collect::<String>(), l.ttl_ms / 1000);
        }
        let conflicts: usize = self.repos.iter().map(|r| git::conflict_branches(&r.root)).sum();
        s += &format!("conflicts {conflicts} saved branch(es) (git branch --list 'hilo/conflict/*')\n");
        s += &format!("approve   {} change(s) waiting{}\n", self.held.len(), if self.held.is_empty() { "" } else { " — run `hilo approve`" });
        if latency {
            let pct = |mut v: Vec<u64>, p: usize| {
                v.sort();
                v.get((v.len() * p / 100).min(v.len().saturating_sub(1))).copied().unwrap_or(0)
            };
            let a: Vec<u64> = self.lat.iter().map(|x| x.0).collect();
            let b: Vec<u64> = self.lat.iter().map(|x| x.1).collect();
            let t: Vec<u64> = self.lat.iter().map(|x| x.0 + x.1).collect();
            s += &format!("latency   {} samples (changes from other machines)\n", self.lat.len());
            s += &format!("  write → anchor   p50 {} ms  p95 {} ms\n", pct(a.clone(), 50), pct(a, 95));
            s += &format!("  anchor → here    p50 {} ms  p95 {} ms\n", pct(b.clone(), 50), pct(b, 95));
            s += &format!("  total            p50 {} ms  p95 {} ms\n", pct(t.clone(), 50), pct(t, 95));
        }
        s
    }

    fn env_cmd(&mut self, cwd: &str, file: &str, cmd: &str, args: &[&str]) -> Result<String, String> {
        let wi = self.wt_for_cwd(cwd).ok_or("not inside a synced worktree")?;
        let repo = self.wts[wi].repo.clone();
        let reldir = canonical_maybe_missing(Path::new(cwd)).strip_prefix(&self.wts[wi].root).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let file = if file.is_empty() { ".env".to_string() } else { file.to_string() };
        let file = if reldir.is_empty() || file.contains('/') { file } else { format!("{reldir}/{file}") };
        if !valid_path(&file) || !is_env_name(file.rsplit('/').next().unwrap_or("")) {
            return Err(format!("{file} is not an env file"));
        }
        let keys = self.vkeys.clone();
        let res = match (cmd, args) {
            ("set", [k, v]) if vault::valid_key(k) => self.set_key(&repo, &file, k, v).map(|_| format!("{k} set in {file}")),
            ("unset", [k]) => self.unset_key(&repo, &file, k).map(|_| format!("{k} removed from {file}")),
            ("local" | "prod", [k]) => {
                let st = self.vault(&repo);
                let val = st.effective(&keys, &file, true).get(*k).cloned().ok_or(format!("{k} is not set in {file}"))?;
                st.local.insert((file.clone(), k.to_string()), (cmd == "prod", val));
                if st.current(&file, k).is_some() {
                    let m = if cmd == "prod" { vault::PROD_MARK } else { vault::LOCAL_MARK };
                    let mark = vault::seal(&keys, &repo, &file, k, m).ok_or("no vault key")?;
                    st.set_shared(&file, k, Some(mark.clone()));
                    if let Some(op) = self.r.local(&vault::wt_of(&repo), &vault::key_path(&file, k), sha256(&mark), Data::Full(mark)) {
                        self.send_op(op);
                    }
                }
                self.save_vault(&repo);
                Ok(if cmd == "prod" {
                    format!("{k} now lives only on this machine and only reaches processes started with `hilo run`")
                } else {
                    format!("{k} is now local: each machine keeps its own value")
                })
            }
            ("ls", []) => {
                let st = self.vault(&repo);
                let mut out = String::new();
                for f in st.files() {
                    let mut ks: BTreeMap<String, &str> = BTreeMap::new();
                    for ((ff, k), v) in &st.shared {
                        if *ff == f && vault::open(&keys, &repo, ff, k, v).is_some_and(|p| !vault::is_mark(&p)) {
                            ks.insert(k.clone(), "shared");
                        }
                    }
                    for ((ff, k), (prod, _)) in &st.local {
                        if *ff == f {
                            ks.insert(k.clone(), if *prod { "prod (this machine, hilo run only)" } else { "local (this machine)" });
                        }
                    }
                    out += &format!("{f}\n");
                    for (k, layer) in ks {
                        out += &format!("  {k:<32} {layer}\n");
                    }
                }
                Ok(if out.is_empty() { "no variables yet".into() } else { out })
            }
            ("history", rest) => {
                let st = self.vault(&repo);
                let out: Vec<String> = st
                    .history
                    .iter()
                    .rev()
                    .filter(|h| h.1 == file && rest.first().is_none_or(|k| h.2 == *k))
                    .map(|h| format!("{}  {}  previous value {}", h.0, h.2, if h.3.is_some() { "kept" } else { "(unset)" }))
                    .collect();
                Ok(if out.is_empty() { "no history".into() } else { out.join("\n") })
            }
            ("revert", [k, rest @ ..]) => {
                let n: usize = rest.first().and_then(|n| n.parse().ok()).unwrap_or(1).max(1);
                let st = self.vault(&repo);
                let ct = st.history.iter().rev().filter(|h| h.1 == file && h.2 == *k).filter_map(|h| h.3.clone()).nth(n - 1).ok_or("no such version")?;
                let pt = vault::open(&keys, &repo, &file, k, &ct).ok_or("cannot open that version")?;
                let v = String::from_utf8_lossy(&pt).into_owned();
                self.set_key(&repo, &file, k, &v).map(|_| format!("{k} reverted"))
            }
            _ => Err("usage: hilo env set KEY=VALUE | unset KEY | local KEY | prod KEY | ls | history [KEY] | revert KEY [N]".into()),
        };
        self.materialize(&repo, &file, None);
        res
    }

    fn approve(&mut self, args: &[&str]) -> (u8, String) {
        if args.is_empty() {
            if self.held.is_empty() {
                return (0, "nothing waiting".into());
            }
            let mut s = String::new();
            for h in &self.held {
                let root = self.wt_index(&h.wt).map_or(h.wt.clone(), |i| self.wts[i].root.display().to_string());
                s += &format!("● {root}: {} from {} — {}\n", if h.path == REF { "branch move" } else { h.path.as_str() }, h.origin, h.reason);
                if let (Some(wi), Some(c)) = (self.wt_index(&h.wt), h.data.content()) {
                    if h.path != REF {
                        let tmp = self.tmp.join("approve.preview");
                        let _ = std::fs::write(&tmp, c.unwrap_or_default());
                        let cur = self.wts[wi].root.join(&h.path);
                        let cur = if cur.exists() { cur } else { PathBuf::from("/dev/null") };
                        s += &git::diff_files(&cur, &tmp);
                        let _ = std::fs::remove_file(tmp);
                    }
                }
            }
            s += "\nhilo approve --all | hilo approve PATH | hilo approve --deny PATH";
            return (0, s);
        }
        let (deny, targets): (bool, Vec<&str>) = match args {
            ["--deny", rest @ ..] => (true, rest.to_vec()),
            ["--all"] => (false, vec![]),
            rest => (false, rest.to_vec()),
        };
        let pick: Vec<Held> = {
            let (take, keep): (Vec<Held>, Vec<Held>) = std::mem::take(&mut self.held).into_iter().partition(|h| targets.is_empty() || targets.contains(&h.path.as_str()));
            self.held = keep;
            take
        };
        self.dirty = true;
        let n = pick.len();
        for h in pick {
            if deny {
                // keep ours: the next scan sends this machine's version
                if let Some(wi) = self.wt_index(&h.wt) {
                    self.wts[wi].scanner.forget(&h.path);
                }
                continue;
            }
            if let Some(repo) = h.wt.strip_prefix("vault:").map(str::to_string) {
                if let Some((f, k)) = vault::split_path(&h.path).map(|(a, b)| (a.to_string(), b.to_string())) {
                    self.apply_vault(&repo, &f, &k, vec![Effect::Write(h.data)], &h.origin, true);
                }
            } else if let Some(wi) = self.wt_index(&h.wt) {
                if h.path == REF {
                    self.apply_ref(wi, vec![Effect::Write(h.data)], &h.origin, true);
                } else {
                    self.apply_file(wi, &h.path, vec![Effect::Write(h.data)], &h.origin, true);
                }
            }
        }
        (0, format!("{} {n} change(s)", if deny { "rejected" } else { "applied" }))
    }

    fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::Up(o) => {
                let resume = self.outstanding.values().map(|s| s.saturating_sub(1)).min().unwrap_or(u64::MAX).min(self.r.last_seq);
                let _ = o.send(Msg::Hello { last_seq: resume, name: self.sh.name.clone() });
                *self.sh.out.lock().unwrap() = Some(o.clone());
                self.out = Some(o);
                self.caught = false;
                if self.role == "member" {
                    self.send(Msg::Time { t0: now_ms(), t1: 0 });
                }
            }
            Ev::Down => {
                self.out = None;
                *self.sh.out.lock().unwrap() = None;
                self.caught = false;
                self.peers.clear();
            }
            Ev::Net(Msg::CaughtUp) => {
                self.caught = true;
                for op in self.r.caught_up() {
                    self.send_op(op);
                }
                self.resync_pending();
                log!("in sync (seq {})", self.r.last_seq);
            }
            Ev::Net(Msg::Op(op)) => self.on_op(op),
            Ev::Net(Msg::Reject(r)) => self.on_reject(r),
            Ev::Net(Msg::Lease(l)) => self.sh.leases.lock().unwrap().apply(&l),
            Ev::Net(Msg::VaultKey { epoch, key }) => {
                if self.vkeys.get(&epoch) != Some(&key) {
                    if let Err(e) = vault::store_key(epoch, &key) {
                        log!("cannot store vault key: {e}");
                    }
                    self.vkeys = vault::keys();
                }
            }
            Ev::Net(Msg::Time { t0, t1 }) => {
                let t2 = now_ms();
                self.offset = t1 as i64 - ((t0 + t2) / 2) as i64;
            }
            Ev::Net(Msg::Peers(p)) => self.peers = p,
            Ev::Net(_) => {}
            Ev::Fs(paths) => self.on_fs(paths),
            Ev::Touched(p) => {
                let abs = canonical_maybe_missing(&p);
                let best = self.wts.iter().enumerate().filter(|(_, w)| abs.starts_with(&w.root)).max_by_key(|(_, w)| w.root.as_os_str().len()).map(|(i, _)| i);
                if let Some(wi) = best {
                    if let Some(rel) = abs.strip_prefix(&self.wts[wi].root).ok().and_then(|r| r.to_str()).map(str::to_string) {
                        self.scan_wt(wi, Some(&[rel]));
                    }
                }
            }
            Ev::Ipc(a, reply) => {
                let r = self.ipc(a);
                let _ = reply.send(r);
            }
        }
    }
}

/// Member side: keep a connection to the anchor alive.
fn connect_loop(addr: String, key: [u8; 32], anchor_key: [u8; 32], tx: Sender<Ev>) {
    let mut wait = Duration::from_secs(1);
    loop {
        let res = (|| -> io::Result<()> {
            let mut s: TcpStream = net::dial(&addr)?;
            s.set_nodelay(true)?;
            let (ctx, crx, remote) = net::initiate(&mut s, key, &net::ZERO_PSK)?;
            if !ct_eq(&remote, &anchor_key) {
                return Err(io::Error::other("anchor key changed; refusing"));
            }
            log!("connected to anchor {addr}");
            let (otx, orx) = channel();
            net::spawn_writer(s.try_clone()?, ctx, orx);
            let _ = tx.send(Ev::Up(otx));
            let r = net::read_loop(&mut s, crx, |m| tx.send(Ev::Net(m)).is_ok());
            let _ = s.shutdown(std::net::Shutdown::Both);
            r
        })();
        let _ = tx.send(Ev::Down);
        match res {
            Err(e) => log!("anchor {addr}: {e}"),
            Ok(()) => return, // daemon is shutting down
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_secs(10));
    }
}

pub fn run() -> io::Result<()> {
    let cfg = Config::load();
    let key = crate::secret::waiting_note(config::key)?;
    let name = config::name();
    config::ensure_home()?;
    let sh = Arc::new(Shared {
        name: name.clone(),
        leases: Mutex::new(lease::Table::default()),
        roots: RwLock::new(vec![]),
        out: Mutex::new(None),
        notices: Mutex::new(HashMap::new()),
        authors: Mutex::new(HashMap::new()),
        broadcast: Mutex::new((vec![], HashMap::new())),
    });
    let (tx, rx) = channel::<Ev>();
    crate::ipc::serve(sh.clone(), tx.clone())?;

    let role = cfg.get("role").unwrap_or_default().to_string();
    let mut atx_keep = None;
    match role.as_str() {
        "anchor" => {
            let atx = anchor::start(cfg.get("bind").or(cfg.get("listen")).unwrap_or("127.0.0.1:7710"), key)?;
            // The local replica talks to the anchor in-process, through the same messages.
            let (otx, orx) = channel::<Msg>();
            let (ctx, crx) = channel::<Msg>();
            atx.send(anchor::In::Connect { id: 0, tx: ctx, key: crate::noise::public_key(&key), ip: String::new(), stream: None }).ok();
            let a2 = atx.clone();
            std::thread::spawn(move || orx.into_iter().all(|m| a2.send(anchor::In::Msg(0, m)).is_ok()));
            let t = tx.clone();
            std::thread::spawn(move || crx.into_iter().all(|m| t.send(Ev::Net(m)).is_ok()));
            tx.send(Ev::Up(otx)).ok();
            atx_keep = Some(atx);
        }
        "member" => {
            let addr = cfg.get("anchor").unwrap_or_default().to_string();
            let anchor_key: [u8; 32] = cfg
                .get("anchor_key")
                .and_then(unhex)
                .and_then(|v| v.try_into().ok())
                .ok_or_else(|| io::Error::other("no anchor key: run `hilo join` again"))?;
            let t = tx.clone();
            std::thread::spawn(move || connect_loop(addr, key, anchor_key, t));
        }
        _ => return Err(io::Error::other("not set up: run `hilo init` or `hilo join`")),
    }

    let (fs_tx, fs_rx) = channel();
    let t = tx.clone();
    std::thread::spawn(move || fs_rx.into_iter().all(|e| t.send(Ev::Fs(e)).is_ok()));

    let folders: Vec<(PathBuf, String)> = cfg
        .all("folder")
        .into_iter()
        .filter_map(|f| std::fs::canonicalize(f).ok())
        .map(|p| {
            let l = folder_label(&p);
            (p, l)
        })
        .collect();
    let mut repos: Vec<Repo> = vec![];
    for r in cfg.all("repo") {
        let root = std::fs::canonicalize(r).unwrap_or_else(|_| PathBuf::from(r));
        match (git::repo_id(&root), git::common_dir(&root)) {
            (Ok(id), Ok(common)) if !repos.iter().any(|x| x.id == id) => {
                let name = root.file_name().map_or("repo".into(), |n| n.to_string_lossy().into_owned());
                let label = folders.iter().find(|f| root.parent() == Some(f.0.as_path())).map(|f| f.1.clone());
                repos.push(Repo { root, id, common, name, wt_stamp: None, label });
            }
            (Ok(_), Ok(_)) => {}
            (Err(e), _) | (_, Err(e)) => log!("skipping {r}: {e}"),
        }
    }
    let mirror_on = cfg.get("mirrors") != Some("off");
    let mirrors = cfg.get("mirrors").filter(|m| *m != "off").map(PathBuf::from);
    let mut d = D {
        sh,
        r: load_state(),
        repos,
        folders,
        last_folders: Instant::now() - Duration::from_secs(60),
        wts: vec![],
        vaults: HashMap::new(),
        vkeys: crate::secret::waiting_note(vault::keys),
        held: load_held(),
        cache: Cache::default(),
        outstanding: HashMap::new(),
        needs_resync: HashSet::new(),
        ref_sha: HashMap::new(),
        out: None,
        caught: false,
        offset: 0,
        lat: VecDeque::new(),
        peers: vec![],
        readonly: cfg.all("readonly").into_iter().map(str::to_string).collect(),
        strict: cfg.get("env") == Some("strict"),
        mirrors,
        mirror_on,
        watcher: None,
        fs_tx,
        role: role.clone(),
        anchor_addr: cfg.get("anchor").unwrap_or_default().to_string(),
        atx: atx_keep,
        dirty: false,
        warned: HashSet::new(),
        tmp: config::path("tmp"),
    };
    // confirmed branch tips: needed to build small bundles
    for ((wt, path), _) in d.r.confirmed.clone() {
        if path == REF {
            if let Some(i) = wt.split_once(':').and_then(|(repo, b)| d.repos.iter().find(|r| r.id == repo).map(|r| (r, b))) {
                if let Some(sha) = git::read_ref(&i.0.common, i.1) {
                    if d.r.confirmed.get(&(wt.clone(), REF.into())) == Some(&ref_hash(&sha)) {
                        d.ref_sha.insert(wt.clone(), sha);
                    }
                }
            }
        }
    }
    d.scan_folders();
    d.discover();

    let (mut last_tick, mut last_full, mut last_save, mut last_time) = (Instant::now(), Instant::now(), Instant::now(), Instant::now());
    loop {
        match rx.recv_timeout(TICK.saturating_sub(last_tick.elapsed())) {
            Ok(ev) => {
                d.on_event(ev);
                // take whatever else is already queued before housekeeping
                while let Ok(ev) = rx.try_recv() {
                    d.on_event(ev);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();
            d.tick();
        }
        if last_full.elapsed() >= if d.watcher.is_some() { VERIFY } else { POLL } {
            last_full = Instant::now();
            d.full_scan();
        }
        if d.role == "member" && d.out.is_some() && last_time.elapsed() >= TIME_SYNC {
            last_time = Instant::now();
            d.send(Msg::Time { t0: now_ms(), t1: 0 });
        }
        if last_save.elapsed() >= SAVE {
            last_save = Instant::now();
            d.save();
        }
    }
}
