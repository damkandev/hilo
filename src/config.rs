//! ~/.hilo: `config` (key = value), `key`, `peers`, `tokens`. HILO_HOME overrides the location.
#![forbid(unsafe_code)]
use crate::crypto::{hex, hkdf, random, unhex};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HILO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".hilo"))
}

pub fn path(name: &str) -> PathBuf {
    home().join(name)
}

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
        Config(
            read("config")
                .lines()
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

fn now() -> u64 {
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

#[derive(Clone, Debug)]
pub struct CloudToken {
    pub id: String,
    pub secret: String,
    pub name: String,
    pub env: bool,
    pub created: u64,
    pub revoked: bool,
}

pub fn cloud_tokens() -> Vec<CloudToken> {
    read("cloud_tokens")
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split(' ').collect();
            Some(CloudToken {
                id: p.first()?.to_string(),
                secret: p.get(1)?.to_string(),
                name: p.get(2)?.to_string(),
                env: *p.get(3)? == "1",
                created: p.get(4)?.parse().ok()?,
                revoked: *p.get(5)? == "1",
            })
        })
        .collect()
}

pub fn save_cloud_tokens(ts: &[CloudToken]) -> io::Result<()> {
    let s: String = ts
        .iter()
        .map(|t| format!("{} {} {} {} {} {}\n", t.id, t.secret, t.name.replace(' ', "_"), t.env as u8, t.created, t.revoked as u8))
        .collect();
    write_private("cloud_tokens", s.as_bytes())
}

fn b32(n: usize) -> String {
    let r: [u8; 32] = random();
    r.iter().take(n).map(|b| B32[(b & 31) as usize] as char).collect()
}

pub fn new_cloud_token(name: &str, env: bool) -> io::Result<CloudToken> {
    let t = CloudToken { id: b32(6), secret: b32(32), name: name.into(), env, created: now(), revoked: false };
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
