//! The env vault. Each repo has a shared layer (encrypted, synced key by key through the anchor
//! like files) and a local layer (never leaves the machine; `prod` keys only reach `hilo run`).
use crate::config;
use crate::crypto::random;
use crate::secret;
use crate::wire::{Rd, W};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::ChaCha20Poly1305;
use std::collections::{BTreeMap, BTreeSet};
use std::io;

/// Shared-layer value meaning "this key is machine-local everywhere": each machine keeps its value.
pub const LOCAL_MARK: &[u8] = b"\0hilo:local";
/// "This key is a production key on one machine": every other machine drops its copy.
pub const PROD_MARK: &[u8] = b"\0hilo:prod";

pub fn is_mark(p: &[u8]) -> bool {
    p == LOCAL_MARK || p == PROD_MARK
}
const HISTORY: usize = 50;

pub fn wt_of(repo: &str) -> String {
    format!("vault:{repo}")
}
pub fn key_path(file: &str, key: &str) -> String {
    format!("{file}\n{key}")
}
pub fn split_path(p: &str) -> Option<(&str, &str)> {
    p.split_once('\n')
}

pub fn valid_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 256
        && k.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

// ---- keys: one per epoch; `hilo unpair` starts a new epoch ----

pub fn epoch() -> u32 {
    config::Config::load().get("vault_epoch").and_then(|v| v.parse().ok()).unwrap_or(0)
}

pub fn keys() -> BTreeMap<u32, [u8; 32]> {
    (1..=epoch()).filter_map(|e| Some((e, secret::get(&format!("vault-{e}")).ok()??))).collect()
}

pub fn store_key(e: u32, k: &[u8; 32]) -> io::Result<()> {
    secret::set(&format!("vault-{e}"), k)?;
    let mut c = config::Config::load();
    if epoch() < e {
        c.set("vault_epoch", &e.to_string());
        c.save()?;
    }
    Ok(())
}

pub fn new_epoch() -> io::Result<(u32, [u8; 32])> {
    let e = epoch() + 1;
    let k = random();
    store_key(e, &k)?;
    Ok((e, k))
}

fn ad(repo: &str, file: &str, key: &str) -> Vec<u8> {
    format!("{repo}\0{file}\0{key}").into_bytes()
}

/// epoch(4) | nonce(12) | ciphertext
pub fn seal(keys: &BTreeMap<u32, [u8; 32]>, repo: &str, file: &str, key: &str, value: &[u8]) -> Option<Vec<u8>> {
    let (e, k) = keys.iter().next_back()?;
    let nonce: [u8; 12] = random();
    let ct = ChaCha20Poly1305::new(k.into()).encrypt(&nonce.into(), Payload { msg: value, aad: &ad(repo, file, key) }).ok()?;
    Some([&e.to_be_bytes()[..], &nonce, &ct].concat())
}

pub fn open(keys: &BTreeMap<u32, [u8; 32]>, repo: &str, file: &str, key: &str, sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < 16 {
        return None;
    }
    let e = u32::from_be_bytes(sealed[..4].try_into().ok()?);
    let nonce: [u8; 12] = sealed[4..16].try_into().ok()?;
    ChaCha20Poly1305::new(keys.get(&e)?.into())
        .decrypt(&nonce.into(), Payload { msg: &sealed[16..], aad: &ad(repo, file, key) })
        .ok()
}

// ---- per-repo store on this machine ----

#[derive(Default)]
pub struct Store {
    pub repo: String,
    /// (file, key) -> sealed value as agreed through the anchor
    pub shared: BTreeMap<(String, String), Vec<u8>>,
    /// (file, key) -> (prod, plaintext). Kept encrypted at rest with this machine's `local` key.
    pub local: BTreeMap<(String, String), (bool, String)>,
    /// (unix secs, file, key, previous sealed value or None)
    pub history: Vec<(u64, String, String, Option<Vec<u8>>)>,
    /// worktree roots + files hilo wrote, to remove them if they stop being ignored
    pub materialized: BTreeSet<(String, String)>,
}

fn shared_path(repo: &str) -> String {
    format!("vault/{repo}")
}
fn local_path(repo: &str) -> String {
    format!("vault/{repo}.local")
}

impl Store {
    pub fn load(repo: &str) -> Store {
        let mut s = Store { repo: repo.into(), ..Default::default() };
        let raw = std::fs::read(config::path(&shared_path(repo))).unwrap_or_default();
        let mut r = Rd(&raw);
        let mut ok = || -> Option<()> {
            for _ in 0..r.u32()? {
                s.shared.insert((r.str()?, r.str()?), r.bytes()?.to_vec());
            }
            for _ in 0..r.u32()? {
                s.history.push((r.u64()?, r.str()?, r.str()?, r.opt()?));
            }
            for _ in 0..r.u32()? {
                s.materialized.insert((r.str()?, r.str()?));
            }
            Some(())
        };
        ok();
        if let (Ok(raw), Ok(Some(k))) = (std::fs::read(config::path(&local_path(repo))), secret::get("local")) {
            let keys = BTreeMap::from([(0u32, k)]);
            if let Some(pt) = open(&keys, repo, "", "", &raw) {
                let mut r = Rd(&pt);
                let mut ok = || -> Option<()> {
                    for _ in 0..r.u32()? {
                        s.local.insert((r.str()?, r.str()?), (r.u8()? == 1, r.str()?));
                    }
                    Some(())
                };
                ok();
            }
        }
        s
    }

    pub fn save(&self) -> io::Result<()> {
        let mut w = W::default();
        w.u32(self.shared.len() as u32);
        for ((f, k), v) in &self.shared {
            w.str(f).str(k).bytes(v);
        }
        w.u32(self.history.len() as u32);
        for (t, f, k, v) in &self.history {
            w.u64(*t).str(f).str(k).opt(v.as_deref());
        }
        w.u32(self.materialized.len() as u32);
        for (a, b) in &self.materialized {
            w.str(a).str(b);
        }
        std::fs::DirBuilder::new().recursive(true).create(config::path("vault"))?;
        config::write_private(&shared_path(&self.repo), &w.0)?;
        let mut l = W::default();
        l.u32(self.local.len() as u32);
        for ((f, k), (prod, v)) in &self.local {
            l.str(f).str(k).u8(*prod as u8).str(v);
        }
        let keys = BTreeMap::from([(0u32, secret::get_or_create("local")?)]);
        let sealed = seal(&keys, &self.repo, "", "", &l.0).ok_or_else(|| io::Error::other("seal"))?;
        config::write_private(&local_path(&self.repo), &sealed)
    }

    pub fn current(&self, file: &str, key: &str) -> Option<&Vec<u8>> {
        self.shared.get(&(file.to_string(), key.to_string()))
    }

    /// Record the previous value before replacing it, keeping 50 versions per file.
    pub fn set_shared(&mut self, file: &str, key: &str, sealed: Option<Vec<u8>>) {
        let k = (file.to_string(), key.to_string());
        let prev = match sealed {
            Some(v) => self.shared.insert(k, v),
            None => self.shared.remove(&k),
        };
        self.remember(file, key, prev);
    }

    pub fn remember(&mut self, file: &str, key: &str, prev: Option<Vec<u8>>) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.history.push((now, file.into(), key.into(), prev));
        let n = self.history.iter().filter(|h| h.1 == file).count();
        if n > HISTORY {
            let i = self.history.iter().position(|h| h.1 == file).unwrap();
            self.history.remove(i);
        }
    }

    pub fn files(&self) -> BTreeSet<String> {
        self.shared.keys().chain(self.local.keys()).map(|k| k.0.clone()).collect()
    }

    /// What a worktree's `file` should contain: shared values with local ones on top.
    /// `with_prod` only for `hilo run`.
    pub fn effective(&self, keys: &BTreeMap<u32, [u8; 32]>, file: &str, with_prod: bool) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for ((f, k), v) in &self.shared {
            if f == file {
                if let Some(pt) = open(keys, &self.repo, f, k, v).filter(|p| !is_mark(p)) {
                    out.insert(k.clone(), String::from_utf8_lossy(&pt).into_owned());
                }
            }
        }
        for ((f, k), (prod, v)) in &self.local {
            if f == file && (with_prod || !prod) {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }
}

// ---- .env text ----

/// `KEY=value` lines (optionally `export KEY=value`); value kept raw, as written after `=`.
pub fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim_start();
            if l.starts_with('#') {
                return None;
            }
            let l = l.strip_prefix("export ").unwrap_or(l);
            let (k, v) = l.split_once('=')?;
            let k = k.trim();
            valid_key(k).then(|| (k.to_string(), v.trim_end_matches('\r').to_string()))
        })
        .collect()
}

/// Rewrites `existing` to hold exactly `values`: changed lines in place, comments kept, new keys appended.
pub fn render(existing: &str, values: &BTreeMap<String, String>) -> String {
    let mut seen = BTreeSet::new();
    let mut out = String::new();
    for line in existing.lines() {
        let parsed = parse(line);
        match parsed.first() {
            Some((k, _)) if seen.contains(k) || !values.contains_key(k) => {} // dropped
            Some((k, _)) => {
                let export = if line.trim_start().starts_with("export ") { "export " } else { "" };
                out.push_str(&format!("{export}{k}={}\n", values[k]));
                seen.insert(k.clone());
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    for (k, v) in values {
        if !seen.contains(k) {
            out.push_str(&format!("{k}={v}\n"));
        }
    }
    out
}

/// For `hilo run`: strip one level of matching quotes.
pub fn unquote(v: &str) -> String {
    let v = v.trim();
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.split(" #").next().unwrap_or(v).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn env_text() {
        let src = "# db\nexport A=1\nB=\"two words\"\nC=3\n";
        assert_eq!(parse(src), [("A".into(), "1".into()), ("B".into(), "\"two words\"".into()), ("C".into(), "3".into())]);
        let vals = BTreeMap::from([("A".to_string(), "9".to_string()), ("B".into(), "\"two words\"".into()), ("D".into(), "4".into())]);
        assert_eq!(render(src, &vals), "# db\nexport A=9\nB=\"two words\"\nD=4\n");
        assert_eq!(unquote("\"two words\""), "two words");
        assert_eq!(unquote("x # comment"), "x");
    }

    #[test]
    fn seal_open() {
        let keys = BTreeMap::from([(1, [3u8; 32]), (2, [4u8; 32])]);
        let s = seal(&keys, "r", ".env", "K", b"v").unwrap();
        assert_eq!(&s[..4], &2u32.to_be_bytes());
        assert_eq!(open(&keys, "r", ".env", "K", &s).unwrap(), b"v");
        assert!(open(&keys, "r", ".env", "OTHER", &s).is_none()); // bound to its key
        let old = BTreeMap::from([(1, [3u8; 32])]);
        let s1 = seal(&old, "r", ".env", "K", b"v").unwrap();
        assert_eq!(open(&keys, "r", ".env", "K", &s1).unwrap(), b"v"); // old epochs still open
    }
}
