//! ~/.hilo: `config` (key = value), `key`, `peers`, `tokens`. HILO_HOME overrides the location.
#![forbid(unsafe_code)]
use crate::crypto::{hex, hkdf, random, unhex};
use std::io::{self, Write};
#[cfg(unix)]
pub use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;

/// Windows has no mode bits, so `.mode()` is a no-op there: the profile folder the hilo home lives
/// in is already readable only by its user (and SYSTEM and admins, who could read a 0600 file too).
#[cfg(windows)]
pub trait DirBuilderExt {
    fn mode(&mut self, _: u32) -> &mut Self {
        self
    }
}
#[cfg(windows)]
impl DirBuilderExt for std::fs::DirBuilder {}
#[cfg(windows)]
pub trait OpenOptionsExt {
    fn mode(&mut self, _: u32) -> &mut Self {
        self
    }
}
#[cfg(windows)]
impl OpenOptionsExt for std::fs::OpenOptions {}

/// The user's home: HOME, or USERPROFILE on Windows (where HOME is usually unset).
pub fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| if cfg!(windows) { std::env::var_os("USERPROFILE") } else { None }).map(PathBuf::from)
}

pub fn home() -> PathBuf {
    std::env::var_os("HILO_HOME").map(PathBuf::from).unwrap_or_else(|| user_home().expect("HOME").join(".hilo"))
}

pub fn path(name: &str) -> PathBuf {
    home().join(name)
}

/// Tests that point HILO_HOME elsewhere, or need it to stay put, take this.
#[cfg(test)]
pub static TEST_HOME: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn ensure_home() -> io::Result<()> {
    for d in ["", "tmp", "anchor/blobs", "secrets", "vault", "index", "store/objects"] {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(home().join(d))?;
    }
    Ok(())
}

/// Atomic, 0600.
pub fn write_private(name: &str, data: &[u8]) -> io::Result<()> {
    let tmp = path(&format!("{name}.tmp"));
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(data)?;
    std::fs::rename(tmp, path(name))
}

fn read(name: &str) -> String {
    std::fs::read_to_string(path(name)).unwrap_or_default()
}

#[derive(Default)]
pub struct Config(Vec<(String, String)>);

impl Config {
    pub fn load() -> Config {
        Config::parse(&read("config"))
    }
    fn parse(s: &str) -> Config {
        Config(
            s.lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .collect(),
        )
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().find(|e| e.0 == k).map(|e| e.1.as_str())
    }
    pub fn all(&self, k: &str) -> Vec<&str> {
        self.0.iter().filter(|e| e.0 == k).map(|e| e.1.as_str()).collect()
    }
    pub fn set(&mut self, k: &str, v: &str) {
        self.0.retain(|e| e.0 != k);
        self.0.push((k.into(), v.into()));
    }
    pub fn remove(&mut self, k: &str) {
        self.0.retain(|e| e.0 != k);
    }
    pub fn add(&mut self, k: &str, v: &str) {
        if !self.all(k).contains(&v) {
            self.0.push((k.into(), v.into()));
        }
    }
    /// `drive = <label> <absolute path>` lines; the path is the rest of the line (spaces allowed).
    pub fn drives(&self) -> Vec<(String, PathBuf)> {
        self.all("drive").into_iter().filter_map(|v| v.split_once(' ')).map(|(l, p)| (l.to_string(), PathBuf::from(p.trim_start()))).filter(|(_, p)| p.is_absolute()).collect()
    }
    pub fn save(&self) -> io::Result<()> {
        let s: String = self.0.iter().map(|(k, v)| format!("{k} = {v}\n")).collect();
        write_private("config", s.as_bytes())
    }
}

/// This machine's X25519 secret (Keychain on macOS).
pub fn key() -> io::Result<[u8; 32]> {
    crate::secret::get_or_create("machine")
}

/// This machine's name, shown to the others.
pub fn name() -> String {
    if let Some(n) = Config::load().get("name") {
        return n.to_string();
    }
    #[cfg(windows)] // hostname.exe has no -s
    if let Some(n) = std::env::var("COMPUTERNAME").ok().filter(|n| !n.is_empty()) {
        return n.to_lowercase();
    }
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "machine".into())
}

/// Paired machines, as the anchor knows them: key, name, last address.
#[derive(Clone, Debug)]
pub struct Peer {
    pub key: [u8; 32],
    pub name: String,
    pub ip: String,
    /// "" for machines you paired; `cloud:<token id>:<env 0|1>:<last seen>` for cloud sandboxes
    pub tag: String,
}

impl Peer {
    pub fn cloud_token(&self) -> Option<&str> {
        self.tag.strip_prefix("cloud:").and_then(|t| t.split(':').next())
    }
    /// May this machine receive the vault keys?
    pub fn gets_env(&self) -> bool {
        match self.tag.strip_prefix("cloud:") {
            None => true,
            Some(t) => t.split(':').nth(1) == Some("1"),
        }
    }
}

pub fn peers() -> Vec<Peer> {
    read("peers")
        .lines()
        .filter_map(|l| {
            let mut p = l.split(' ');
            let key = unhex(p.next()?)?.try_into().ok()?;
            let name = p.next().unwrap_or("?").into();
            let ip = p.next().unwrap_or("").into();
            Some(Peer { key, name, ip, tag: p.next().unwrap_or("").into() })
        })
        .collect()
}

pub fn save_peers(ps: &[Peer]) -> io::Result<()> {
    let s: String = ps.iter().map(|p| format!("{} {} {} {}\n", hex(&p.key), p.name.replace(' ', "_"), if p.ip.is_empty() { "-" } else { &p.ip }, p.tag)).collect();
    write_private("peers", s.as_bytes())
}

// ---- pairing tokens: 60 bits, 12 base32 chars, single use, 10 minutes, persisted ----

const B32: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

pub fn new_token() -> io::Result<String> {
    let mut bits = u64::from_be_bytes(random()) >> 4;
    let t: String = (0..12).map(|_| {
        let c = B32[(bits & 31) as usize] as char;
        bits >>= 5;
        c
    }).collect();
    let mut s = read("tokens");
    s.push_str(&format!("{t} {} 0\n", now() + 600));
    write_private("tokens", s.as_bytes())?;
    Ok(t)
}

/// Unused, unexpired tokens.
pub fn active_tokens() -> Vec<String> {
    read("tokens")
        .lines()
        .filter_map(|l| {
            let mut p = l.split(' ');
            let (t, exp, used) = (p.next()?, p.next()?.parse::<u64>().ok()?, p.next()?);
            (used == "0" && exp > now()).then(|| t.to_string())
        })
        .collect()
}

pub fn use_token(t: &str) -> io::Result<()> {
    let s: String = read("tokens")
        .lines()
        .map(|l| if l.starts_with(&format!("{t} ")) { format!("{} 1\n", &l[..l.len() - 2]) } else { format!("{l}\n") })
        .collect();
    write_private("tokens", s.as_bytes())
}

pub fn token_psk(t: &str) -> [u8; 32] {
    hkdf(b"hilo pairing v1", t.as_bytes(), b"", 32).try_into().unwrap()
}

// ---- cloud tokens: reusable, revocable, 160 bits. Each sandbox that uses one joins as a cloud machine ----

/// What a scoped token sees: one folder of a drive (or all of it), or one repo.
#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    pub unit: String,   // `drive:<label>` or `repo:<repo id>`
    pub prefix: String, // inside a drive, "" = all of it; always "" for a repo
}

/// `drive:notes/clientes`, `repo:<id>`
impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self.prefix.trim_end_matches('/') {
            "" => write!(f, "{}", self.unit),
            p => write!(f, "{}/{p}", self.unit),
        }
    }
}

/// Is key (`wt`, `path`) inside the scope? Anything unknown is outside.
pub fn in_scope(s: &Scope, wt: &str, path: &str) -> bool {
    match s.unit.split_once(':') {
        // its worktrees `R:<branch>`, its vault and its announcement
        Some(("repo", r)) if !r.is_empty() => {
            wt.strip_prefix(r).is_some_and(|b| b.starts_with(':')) || wt.strip_prefix("vault:") == Some(r) || wt.strip_prefix("repo:") == Some(r)
        }
        Some(("drive", l)) if !l.is_empty() && wt == s.unit => {
            let p = s.prefix.trim_end_matches('/');
            p.is_empty() || path == p || path.strip_prefix(p).is_some_and(|r| r.starts_with('/'))
        }
        _ => false,
    }
}

/// May a sandbox with this scope change (`wt`, `path`)? In scope, and never a repo announcement
/// (it says where the other machines clone the repo from). The anchor and the sandbox share this rule.
pub fn may_write(s: &Scope, wt: &str, path: &str) -> bool {
    in_scope(s, wt, path) && !wt.starts_with("repo:")
}

#[derive(Clone, Debug, PartialEq)]
pub struct CloudToken {
    pub id: String,
    pub secret: String,
    pub name: String,
    pub env: bool,
    pub created: u64,
    pub revoked: bool,
    pub scope: Option<Scope>,
    pub expires: u64, // unix seconds, 0 = never
    pub ro: bool,
}

impl CloudToken {
    pub fn live(&self, now: u64) -> bool {
        !self.revoked && (self.expires == 0 || self.expires > now)
    }
}

/// `id secret name env created revoked scope expires ro`. `scope` is `-` or the hex of
/// `unit NUL prefix` (labels and folders may have spaces). Lines from before scopes end at `revoked`.
/// A field that doesn't parse drops the whole line: a token that can't be read can't be used.
fn parse_cloud_line(l: &str) -> Option<CloudToken> {
    let p: Vec<&str> = l.split(' ').collect();
    let scope = match p.get(6) {
        None | Some(&"-") => None,
        Some(h) => {
            let s = String::from_utf8(unhex(h)?).ok()?;
            let (unit, prefix) = s.split_once('\0')?;
            Some(Scope { unit: unit.into(), prefix: prefix.into() })
        }
    };
    Some(CloudToken {
        id: p.first()?.to_string(),
        secret: p.get(1)?.to_string(),
        name: p.get(2)?.to_string(),
        env: *p.get(3)? == "1",
        created: p.get(4)?.parse().ok()?,
        revoked: *p.get(5)? == "1",
        scope,
        expires: p.get(7).map_or(Some(0), |e| e.parse().ok())?,
        ro: p.get(8) == Some(&"1"),
    })
}

fn cloud_line(t: &CloudToken) -> String {
    let scope = t.scope.as_ref().map_or("-".into(), |s| hex(format!("{}\0{}", s.unit, s.prefix).as_bytes()));
    format!("{} {} {} {} {} {} {scope} {} {}\n", t.id, t.secret, t.name.replace(' ', "_"), t.env as u8, t.created, t.revoked as u8, t.expires, t.ro as u8)
}

pub fn cloud_tokens() -> Vec<CloudToken> {
    read("cloud_tokens").lines().filter_map(parse_cloud_line).collect()
}

pub fn save_cloud_tokens(ts: &[CloudToken]) -> io::Result<()> {
    write_private("cloud_tokens", ts.iter().map(cloud_line).collect::<String>().as_bytes())
}

fn b32(n: usize) -> String {
    let r: [u8; 32] = random();
    r.iter().take(n).map(|b| B32[(b & 31) as usize] as char).collect()
}

pub fn new_cloud_token(name: &str, env: bool, scope: Option<Scope>, expires: u64, ro: bool) -> io::Result<CloudToken> {
    let t = CloudToken { id: b32(6), secret: b32(32), name: name.into(), env, created: now(), revoked: false, scope, expires, ro };
    let mut all = cloud_tokens();
    all.push(t.clone());
    save_cloud_tokens(&all)?;
    Ok(t)
}

pub fn cloud_psk(secret: &str) -> [u8; 32] {
    hkdf(b"hilo cloud v1", secret.as_bytes(), b"", 32).try_into().unwrap()
}

/// `HILO-CLOUD-<32 chars>@host:port` -> (secret, address)
pub fn parse_cloud(code: &str) -> Option<(String, String)> {
    let (t, addr) = code.trim().split_once('@')?;
    let s = t.to_uppercase().strip_prefix("HILO-CLOUD-")?.to_string();
    (s.len() == 32 && s.bytes().all(|b| B32.contains(&b)) && !addr.is_empty()).then(|| (s, addr.to_string()))
}

pub fn format_cloud(secret: &str, addr: &str) -> String {
    format!("HILO-CLOUD-{secret}@{addr}")
}

/// `HILO-7KQ2-M9PA-XR4T@203.0.113.10:7710` -> (token, address)
pub fn parse_code(code: &str) -> Option<(String, String)> {
    let (t, addr) = code.trim().split_once('@')?;
    let t: String = t.to_uppercase().strip_prefix("HILO-")?.chars().filter(|c| *c != '-').collect();
    (t.len() == 12 && t.bytes().all(|b| B32.contains(&b))).then(|| (t, addr.to_string()))
}

pub fn format_code(t: &str, addr: &str) -> String {
    format!("HILO-{}-{}-{}@{addr}", &t[..4], &t[4..8], &t[8..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_lines() {
        let c = Config::parse("folder = /a/code\ndrive = notes /Users/x/My Notes \ndrive=docs   /srv/d\ndrive = bad\ndrive = rel some/where\n");
        assert_eq!(c.drives(), [("notes".into(), PathBuf::from("/Users/x/My Notes")), ("docs".into(), PathBuf::from("/srv/d"))]);
        let mut c = Config::default();
        c.add("drive", "my-notes /tmp/with space/x");
        assert_eq!(Config::parse(&c.0.iter().map(|(k, v)| format!("{k} = {v}\n")).collect::<String>()).drives(), [("my-notes".into(), PathBuf::from("/tmp/with space/x"))]);
    }

    #[test]
    fn scopes() {
        let d = |prefix: &str| Scope { unit: "drive:notes".into(), prefix: prefix.into() };
        assert!(in_scope(&d(""), "drive:notes", "any/thing"));
        assert!(in_scope(&d("a"), "drive:notes", "a"));
        assert!(in_scope(&d("a"), "drive:notes", "a/x"));
        assert!(in_scope(&d("a/"), "drive:notes", "a/x"));
        assert!(in_scope(&d("a b/c"), "drive:notes", "a b/c/d.md"));
        assert!(!in_scope(&d("a"), "drive:notes", "ab/x"));
        assert!(!in_scope(&d("a"), "drive:notes", "ab"));
        assert!(!in_scope(&d("a/b"), "drive:notes", "a"));
        assert!(!in_scope(&d(""), "drive:notes2", "x"));
        assert!(!in_scope(&d(""), "drive:note", "x"));
        assert!(!in_scope(&d(""), "vault:notes", "x"));
        let r = Scope { unit: "repo:R".into(), prefix: String::new() };
        for wt in ["R:main", "R:feat/x", "vault:R", "repo:R"] {
            assert!(in_scope(&r, wt, "src/a.rs"), "{wt}");
        }
        for wt in ["R2:main", "R", "vault:R2", "repo:R2", "drive:R", "X:R:main", ":main"] {
            assert!(!in_scope(&r, wt, "src/a.rs"), "{wt}");
        }
        // send or keep here: a sandbox never sends a repo announcement, even of its own repo
        assert!(may_write(&r, "R:main", "src/a.rs") && may_write(&r, "vault:R", "x") && !may_write(&r, "repo:R", "\0meta"));
        assert!(may_write(&d("a"), "drive:notes", "a/x") && !may_write(&d("a"), "drive:notes", "fuera.md"));
        assert_eq!((d("clientes/").to_string(), r.to_string()), ("drive:notes/clientes".into(), "repo:R".into()));
        for bad in ["repo:", "drive:", "", "other:R", "R"] {
            assert!(!in_scope(&Scope { unit: bad.into(), prefix: String::new() }, "R:main", "a"), "{bad}");
        }
    }

    #[test]
    fn token_lines() {
        let old = parse_cloud_line("ABC123 SECRET name 1 1700000000 0").unwrap();
        assert_eq!((old.scope.clone(), old.expires, old.ro, old.env, old.revoked), (None, 0, false, true, false));
        assert_eq!(parse_cloud_line(cloud_line(&old).trim_end()), Some(old));
        let t = CloudToken {
            id: "X".into(),
            secret: "S".into(),
            name: "n".into(),
            env: false,
            created: 1,
            revoked: true,
            scope: Some(Scope { unit: "drive:my notes".into(), prefix: "a b/c".into() }),
            expires: 99,
            ro: true,
        };
        assert_eq!(parse_cloud_line(cloud_line(&t).trim_end()), Some(t));
        assert!(parse_cloud_line("X S n 0 1 0 zz 0 0").is_none()); // a broken scope is not "no scope"
        assert!(parse_cloud_line("X S n 0 1 0 - soon 0").is_none());
    }
}
