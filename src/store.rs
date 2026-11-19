//! Version store: every version of a file that passes through hilo on this machine, so any file
//! can be recovered after hilo or anyone else overwrites or deletes it. `~/.hilo/store/objects/<hex>`
//! holds contents (written once), `journal` the append-only records. The daemon is the only writer.
#![forbid(unsafe_code)]
use crate::anchor::now_ms;
use crate::config;
use crate::crypto::hex;
use crate::fsops::{self, Stamp};
use crate::sync::{hash_of, Hash, Key, NONE};
use crate::wire::{Rd, W};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Local,
    Remote,
    Conflict,
    Replaced,
    Restored,
}

impl Kind {
    const ALL: [Kind; 5] = [Kind::Local, Kind::Remote, Kind::Conflict, Kind::Replaced, Kind::Restored];
    pub fn name(self) -> &'static str {
        ["local", "remote", "conflict", "replaced", "restored"][self as usize]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub t_ms: u64,
    pub unit: String,
    pub path: String,
    pub hash: Hash, // NONE = deleted
    pub kind: Kind,
    pub origin: String,
}

pub struct Store {
    root: PathBuf,
    entries: Vec<Entry>, // journal order
    by_key: HashMap<Key, Vec<usize>>, // indices into entries, oldest first
    sizes: HashMap<Hash, u64>, // objects on disk
    journal: File,
    len: u64, // journal bytes known good
}

fn encode(e: &Entry) -> Vec<u8> {
    let mut w = W::default();
    w.u64(e.t_ms).str(&e.unit).str(&e.path).hash(&e.hash).u8(e.kind as u8).str(&e.origin);
    w.0
}

fn open_journal(p: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).mode(0o600).open(p)
}

impl Store {
    pub fn open() -> io::Result<Store> {
        Store::open_at(config::path("store"))
    }

    /// Replays the journal; a torn tail from a crash is cut off.
    pub fn open_at(root: PathBuf) -> io::Result<Store> {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(root.join("objects"))?;
        let p = root.join("journal");
        let bytes = std::fs::read(&p).unwrap_or_default();
        let mut entries = vec![];
        let mut good = 0;
        let mut r = Rd(&bytes);
        while let Some(e) = (|| {
            Some(Entry { t_ms: r.u64()?, unit: r.str()?, path: r.str()?, hash: r.hash()?, kind: *Kind::ALL.get(r.u8()? as usize)?, origin: r.str()? })
        })() {
            entries.push(e);
            good = bytes.len() - r.0.len();
        }
        let journal = open_journal(&p)?;
        journal.set_len(good as u64)?;
        let mut sizes = HashMap::new();
        for e in std::fs::read_dir(root.join("objects"))?.flatten() {
            let h = e.file_name().to_str().and_then(crate::crypto::unhex).and_then(|v| Hash::try_from(v).ok());
            if let (Some(h), Ok(m)) = (h, e.metadata()) {
                sizes.insert(h, m.len());
            }
        }
        let mut s = Store { root, entries, by_key: HashMap::new(), sizes, journal, len: good as u64 };
        s.reindex();
        Ok(s)
    }

    fn reindex(&mut self) {
        self.by_key.clear();
        for (i, e) in self.entries.iter().enumerate() {
            self.by_key.entry((e.unit.clone(), e.path.clone())).or_default().push(i);
        }
    }

    fn object(&self, h: &Hash) -> PathBuf {
        self.root.join("objects").join(hex(h))
    }

    pub fn put(&mut self, unit: &str, path: &str, data: Option<&[u8]>, kind: Kind, origin: &str) -> io::Result<Hash> {
        let hash = hash_of(data);
        if let Some(d) = data.filter(|_| !self.sizes.contains_key(&hash)) {
            let p = self.object(&hash);
            let tmp = p.with_extension("tmp");
            let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(d)?;
            std::fs::rename(tmp, p)?;
            self.sizes.insert(hash, d.len() as u64);
        }
        let e = Entry { t_ms: now_ms(), unit: unit.into(), path: path.into(), hash, kind, origin: origin.into() };
        let b = encode(&e);
        if let Err(err) = self.journal.write_all(&b) {
            let _ = self.journal.set_len(self.len); // don't leave a torn record for the next append to follow
            return Err(err);
        }
        self.len += b.len() as u64;
        self.by_key.entry((e.unit.clone(), e.path.clone())).or_default().push(self.entries.len());
        self.entries.push(e);
        Ok(hash)
    }

    pub fn get(&self, h: &Hash) -> io::Result<Option<Vec<u8>>> {
        if *h == NONE { Ok(None) } else { std::fs::read(self.object(h)).map(Some) }
    }

    pub fn size(&self, h: &Hash) -> Option<u64> {
        self.sizes.get(h).copied()
    }

    /// (versions, bytes of content kept)
    pub fn totals(&self) -> (usize, u64) {
        (self.entries.len(), self.sizes.values().sum())
    }

    /// Newest first.
    pub fn history(&self, unit: &str, path: &str) -> Vec<&Entry> {
        self.by_key.get(&(unit.to_string(), path.to_string())).map_or(vec![], |v| v.iter().rev().map(|&i| &self.entries[i]).collect())
    }

    /// Paths under `prefix` ("" = the whole unit) whose newest entry is a deletion.
    pub fn deleted(&self, unit: &str, prefix: &str) -> Vec<&Entry> {
        let mut v: Vec<&Entry> = self
            .by_key
            .iter()
            .filter(|((u, p), _)| u == unit && (prefix.is_empty() || p == prefix || p.strip_prefix(prefix).is_some_and(|r| r.starts_with('/'))))
            .filter_map(|(_, ix)| ix.last().map(|&i| &self.entries[i]))
            .filter(|e| e.hash == NONE)
            .collect();
        v.sort_by(|a, b| b.t_ms.cmp(&a.t_ms));
        v
    }

    /// Drops the oldest entries until none is older than `max_age_days` and the objects fit in
    /// `max_bytes`, never the newest entry of a path; then deletes objects nothing references.
    pub fn prune(&mut self, max_age_days: u64, max_bytes: u64) -> io::Result<(usize, u64)> {
        let cutoff = now_ms().saturating_sub(max_age_days * 86_400_000);
        let newest: std::collections::HashSet<usize> = self.by_key.values().filter_map(|v| v.last().copied()).collect();
        let mut refs: HashMap<Hash, usize> = HashMap::new();
        for e in &self.entries {
            *refs.entry(e.hash).or_default() += 1;
        }
        let mut total: u64 = refs.keys().filter_map(|h| self.sizes.get(h)).sum();
        let mut keep = vec![true; self.entries.len()];
        for (i, e) in self.entries.iter().enumerate() {
            if newest.contains(&i) || (e.t_ms >= cutoff && total <= max_bytes) {
                continue;
            }
            keep[i] = false;
            let n = refs.get_mut(&e.hash).unwrap();
            *n -= 1;
            if *n == 0 {
                refs.remove(&e.hash);
                total -= self.sizes.get(&e.hash).copied().unwrap_or(0);
            }
        }
        let dropped = keep.iter().filter(|k| !**k).count();
        if dropped > 0 {
            let mut it = keep.iter();
            let kept: Vec<Entry> = std::mem::take(&mut self.entries).into_iter().filter(|_| *it.next().unwrap()).collect();
            let bytes: Vec<u8> = kept.iter().flat_map(encode).collect();
            let p = self.root.join("journal");
            let tmp = p.with_extension("tmp");
            let res = (|| {
                let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
                f.write_all(&bytes)?;
                std::fs::rename(&tmp, &p)?;
                open_journal(&p)
            })();
            match res {
                Ok(f) => {
                    self.journal = f;
                    self.len = bytes.len() as u64;
                    self.entries = kept;
                }
                Err(e) => {
                    // the old journal is still there and still matches what we had: reload it
                    *self = Store::open_at(self.root.clone())?;
                    return Err(e);
                }
            }
            self.reindex();
        }
        let mut freed = 0;
        for e in std::fs::read_dir(self.root.join("objects"))?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let h = crate::crypto::unhex(&name).and_then(|v| Hash::try_from(v).ok());
            if h.is_some_and(|h| refs.contains_key(&h)) {
                continue;
            }
            let n = e.metadata().map_or(0, |m| m.len());
            if std::fs::remove_file(e.path()).is_ok() {
                freed += n;
                if let Some(h) = h {
                    self.sizes.remove(&h);
                }
            }
        }
        Ok((dropped, freed))
    }

    /// Writes `data` at `root/path` only after keeping what is on disk now (if this path has no
    /// record of it yet, as `Replaced`) and the new version (as `kind`). Nothing is written unless
    /// both are kept.
    pub fn keep_and_write(&mut self, root: &Path, unit: &str, path: &str, data: Option<&[u8]>, kind: Kind, origin: &str) -> io::Result<Option<Stamp>> {
        let p = root.join(path);
        let disk = std::fs::symlink_metadata(&p).ok().filter(|m| m.is_file()).and_then(|_| std::fs::read(&p).ok());
        if let Some(d) = &disk {
            let dh = hash_of(Some(d));
            if !self.history(unit, path).iter().any(|e| e.hash == dh) {
                self.put(unit, path, Some(d), Kind::Replaced, "")?;
            }
        }
        self.put(unit, path, data, kind, origin)?;
        fsops::write(root, path, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::random;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("hilo-store-{}", hex(&random::<6>())));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn roundtrip_and_torn_tail() {
        let d = tmp();
        let mut s = Store::open_at(d.clone()).unwrap();
        let h1 = s.put("r:main", "a.txt", Some(b"one"), Kind::Local, "mac").unwrap();
        let h2 = s.put("r:main", "a.txt", Some(b"two"), Kind::Remote, "acer").unwrap();
        s.put("r:main", "b.txt", Some(b"one"), Kind::Local, "mac").unwrap();
        assert_eq!(s.get(&h1).unwrap().as_deref(), Some(&b"one"[..]));
        assert_eq!(s.get(&NONE).unwrap(), None);
        let hist: Vec<(Hash, Kind)> = s.history("r:main", "a.txt").iter().map(|e| (e.hash, e.kind)).collect();
        assert_eq!(hist, [(h2, Kind::Remote), (h1, Kind::Local)]);
        assert_eq!(s.totals(), (3, 6)); // "one" is stored once
        drop(s);
        // a crash in the middle of a record
        let j = d.join("journal");
        let mut f = OpenOptions::new().append(true).open(&j).unwrap();
        f.write_all(&encode(&Entry { t_ms: 1, unit: "r:main".into(), path: "a.txt".into(), hash: NONE, kind: Kind::Local, origin: "x".into() })[..20]).unwrap();
        drop(f);
        let mut s = Store::open_at(d.clone()).unwrap();
        assert_eq!(s.history("r:main", "a.txt").len(), 2);
        s.put("r:main", "a.txt", None, Kind::Local, "mac").unwrap();
        let s = Store::open_at(d.clone()).unwrap();
        assert_eq!(s.history("r:main", "a.txt").len(), 3);
        assert_eq!(s.history("r:main", "a.txt")[0].hash, NONE);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn deleted_under_a_prefix() {
        let d = tmp();
        let mut s = Store::open_at(d.clone()).unwrap();
        for p in ["src/a.rs", "src/b.rs", "srcx/c.rs", "d.rs"] {
            s.put("u", p, Some(p.as_bytes()), Kind::Local, "mac").unwrap();
            s.put("u", p, None, Kind::Remote, "acer").unwrap();
        }
        s.put("u", "src/b.rs", Some(b"back"), Kind::Restored, "mac").unwrap();
        s.put("other", "src/z.rs", None, Kind::Local, "mac").unwrap();
        let names = |v: Vec<&Entry>| -> Vec<String> {
            let mut n: Vec<String> = v.iter().map(|e| e.path.clone()).collect();
            n.sort();
            n
        };
        assert_eq!(names(s.deleted("u", "src")), ["src/a.rs"]);
        assert_eq!(names(s.deleted("u", "")), ["d.rs", "src/a.rs", "srcx/c.rs"]);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn prune_keeps_the_newest_of_each_path() {
        let d = tmp();
        let mut s = Store::open_at(d.clone()).unwrap();
        let old = s.put("u", "a", Some(b"old a"), Kind::Local, "").unwrap();
        s.put("u", "a", Some(b"new a"), Kind::Local, "").unwrap();
        s.put("u", "b", Some(b"only b"), Kind::Local, "").unwrap();
        std::fs::write(d.join("objects/stray.tmp"), b"junk").unwrap();
        assert_eq!(s.prune(30, 1 << 30).unwrap(), (0, 4)); // nothing old, nothing too big: only the stray file goes
        for e in &mut s.entries {
            e.t_ms = 1; // everything is ancient
        }
        assert_eq!(s.prune(30, 1 << 30).unwrap(), (1, 5));
        assert!(s.get(&old).is_err());
        let s = Store::open_at(d.clone()).unwrap();
        assert_eq!(s.history("u", "a").len(), 1);
        assert_eq!(s.history("u", "b").len(), 1);
        assert_eq!(s.totals(), (2, 11));
        // over the size cap: drops old versions even if they are recent, still never the newest
        let mut s = s;
        s.put("u", "a", Some(b"newer a"), Kind::Remote, "acer").unwrap();
        for e in &mut s.entries {
            e.t_ms = now_ms();
        }
        assert_eq!(s.prune(30, 0).unwrap(), (1, 5));
        assert_eq!(s.history("u", "a")[0].kind, Kind::Remote);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn no_overwrite_without_a_copy() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp();
        let root = d.join("wt");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("f"), b"mine, never synced").unwrap();
        let mut s = Store::open_at(d.join("store")).unwrap();
        std::fs::set_permissions(d.join("store/objects"), std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::write(d.join("store/objects/probe"), b"").is_ok() {
            return std::fs::remove_dir_all(d).unwrap(); // root ignores permissions: nothing to test
        }
        assert!(s.keep_and_write(&root, "u", "f", Some(b"theirs"), Kind::Remote, "acer").is_err());
        assert_eq!(std::fs::read(root.join("f")).unwrap(), b"mine, never synced");
        std::fs::set_permissions(d.join("store/objects"), std::fs::Permissions::from_mode(0o700)).unwrap();
        s.keep_and_write(&root, "u", "f", Some(b"theirs"), Kind::Remote, "acer").unwrap();
        assert_eq!(std::fs::read(root.join("f")).unwrap(), b"theirs");
        let h: Vec<Kind> = s.history("u", "f").iter().map(|e| e.kind).collect();
        assert_eq!(h, [Kind::Remote, Kind::Replaced]);
        assert_eq!(s.get(&s.history("u", "f")[1].hash).unwrap().as_deref(), Some(&b"mine, never synced"[..]));
        // already on record: not kept twice
        s.keep_and_write(&root, "u", "f", None, Kind::Remote, "acer").unwrap();
        assert_eq!(s.history("u", "f").len(), 3);
        assert!(!root.join("f").exists());
        std::fs::remove_dir_all(d).unwrap();
    }
}
