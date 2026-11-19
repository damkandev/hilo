//! Version store: every version of a file that passes through hilo on this machine, so any file
//! can be recovered after hilo or anyone else overwrites or deletes it. `~/.hilo/store/objects/<hex>`
//! holds contents (written once), `journal` the append-only records. The daemon is the only writer
//! (an exclusive lock on `lock` keeps a second daemon out).
#![forbid(unsafe_code)]
use crate::anchor::now_ms;
use crate::config;
use crate::crypto::{hex, sha256};
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
    journal: Option<File>, // None: the journal is damaged, so nothing is recorded (and nothing overwritten)
    len: u64, // journal bytes known good
    _lock: File,
}

fn sum4(b: &[u8]) -> [u8; 4] {
    let h = sha256(b);
    [h[0], h[1], h[2], h[3]]
}

/// A record: payload length (u32) and its checksum, then the payload and its checksum. Checksums
/// are the first 4 bytes of the SHA-256, so a flipped bit anywhere is damage, never a short read.
fn encode(e: &Entry) -> Vec<u8> {
    let mut w = W::default();
    w.u64(e.t_ms).str(&e.unit).str(&e.path).hash(&e.hash).u8(e.kind as u8).str(&e.origin);
    let len = (w.0.len() as u32).to_be_bytes();
    [&len[..], &sum4(&len), &w.0, &sum4(&w.0)].concat()
}

/// One record. `Err(true)`: the bytes end mid-record (a torn write); `Err(false)`: damage.
fn decode(r: &mut Rd) -> Result<Entry, bool> {
    let b = r.0;
    if b.len() < 8 {
        return Err(true);
    }
    if b[4..8] != sum4(&b[..4]) {
        return Err(false);
    }
    let n = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize;
    let Some(rest) = b[8..].get(..n + 4) else { return Err(true) };
    let (p, sum) = rest.split_at(n);
    if sum != sum4(p) {
        return Err(false);
    }
    r.0 = &b[8 + n + 4..];
    let mut q = Rd(p);
    let e = (|| Some(Entry { t_ms: q.u64()?, unit: q.str()?, path: q.str()?, hash: q.hash()?, kind: *Kind::ALL.get(q.u8()? as usize)?, origin: q.str()? }))();
    // a valid checksum over a payload that doesn't parse: a newer format, not a torn write
    e.filter(|_| q.0.is_empty()).ok_or(false)
}

fn open_journal(p: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).mode(0o600).open(p)
}

impl Store {
    pub fn open() -> io::Result<Store> {
        Store::open_at(config::path("store"))
    }

    /// Replays the journal. A torn tail from a crash is cut off; anything else that doesn't parse
    /// is kept as it is (with a copy in `journal.bad.<ms>`) and the store records nothing.
    pub fn open_at(root: PathBuf) -> io::Result<Store> {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(root.join("objects"))?;
        let lock = OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(root.join("lock"))?;
        lock.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => io::Error::new(io::ErrorKind::WouldBlock, "another hilo daemon is running (the version store is locked)"),
            std::fs::TryLockError::Error(e) => e,
        })?;
        let p = root.join("journal");
        let bytes = match std::fs::read(&p) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => vec![],
            r => r?,
        };
        let (mut entries, mut good, mut damaged) = (vec![], 0, false);
        let mut r = Rd(&bytes);
        while !r.0.is_empty() {
            match decode(&mut r) {
                Ok(e) => {
                    entries.push(e);
                    good = bytes.len() - r.0.len();
                }
                Err(torn) => {
                    damaged = !torn;
                    break;
                }
            }
        }
        let journal = if damaged {
            let bad = root.join(format!("journal.bad.{}", now_ms()));
            std::fs::copy(&p, &bad)?;
            crate::log!(
                "history: {} is damaged after byte {good} (a copy is in {}); until it is fixed, nothing is recorded and changes from other machines are not written here",
                p.display(),
                bad.display()
            );
            None
        } else {
            let j = open_journal(&p)?;
            j.set_len(good as u64)?;
            Some(j)
        };
        let mut sizes = HashMap::new();
        for e in std::fs::read_dir(root.join("objects"))?.flatten() {
            let h = e.file_name().to_str().and_then(crate::crypto::unhex).and_then(|v| Hash::try_from(v).ok());
            if let (Some(h), Ok(m)) = (h, e.metadata()) {
                sizes.insert(h, m.len());
            }
        }
        let mut s = Store { root, entries, by_key: HashMap::new(), sizes, journal, len: good as u64, _lock: lock };
        s.reindex();
        Ok(s)
    }

    /// The journal is damaged: nothing can be recorded.
    pub fn broken(&self) -> bool {
        self.journal.is_none()
    }

    fn journal(&mut self) -> io::Result<&mut File> {
        self.journal.as_mut().ok_or_else(|| io::Error::other("the version store's journal is damaged"))
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
        self.journal()?;
        let hash = hash_of(data);
        // ponytail: an object already there is re-read to check it; costly only for big files stored twice
        if let Some(d) = data.filter(|_| !self.sizes.contains_key(&hash) || self.get(&hash).is_err()) {
            let p = self.object(&hash);
            let tmp = p.with_extension("tmp");
            let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(d)?;
            f.sync_all()?;
            std::fs::rename(tmp, p)?;
            self.sizes.insert(hash, d.len() as u64);
        }
        let e = Entry { t_ms: now_ms(), unit: unit.into(), path: path.into(), hash, kind, origin: origin.into() };
        let b = encode(&e);
        let len = self.len;
        let j = self.journal()?;
        if let Err(err) = j.write_all(&b) {
            let _ = j.set_len(len); // don't leave a torn record for the next append to follow
            return Err(err);
        }
        self.len += b.len() as u64;
        self.by_key.entry((e.unit.clone(), e.path.clone())).or_default().push(self.entries.len());
        self.entries.push(e);
        Ok(hash)
    }

    /// The content of a version. A damaged object is an error.
    pub fn get(&self, h: &Hash) -> io::Result<Option<Vec<u8>>> {
        if *h == NONE {
            return Ok(None);
        }
        let d = std::fs::read(self.object(h))?;
        if sha256(&d) != *h {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "damaged object in the version store"));
        }
        Ok(Some(d))
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

    /// Drops the oldest entries until no version stopped being current more than `max_age_days`
    /// ago and the objects fit in `max_bytes`, never the newest entry of a path; then deletes
    /// objects nothing references. On error nothing changes.
    pub fn prune(&mut self, max_age_days: u64, max_bytes: u64) -> io::Result<(usize, u64)> {
        if self.broken() {
            return Ok((0, 0)); // the damaged part may reference any object
        }
        let cutoff = now_ms().saturating_sub(max_age_days * 86_400_000);
        // when each version was replaced by the next one of its path; the newest never was
        let mut until = vec![u64::MAX; self.entries.len()];
        for v in self.by_key.values() {
            for w in v.windows(2) {
                until[w[0]] = self.entries[w[1]].t_ms;
            }
        }
        let mut refs: HashMap<Hash, usize> = HashMap::new();
        for e in &self.entries {
            *refs.entry(e.hash).or_default() += 1;
        }
        let mut total: u64 = refs.keys().filter_map(|h| self.sizes.get(h)).sum();
        let mut keep = vec![true; self.entries.len()];
        for (i, e) in self.entries.iter().enumerate() {
            if until[i] == u64::MAX || (until[i] >= cutoff && total <= max_bytes) {
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
            let kept: Vec<Entry> = self.entries.iter().zip(&keep).filter(|(_, k)| **k).map(|(e, _)| e.clone()).collect();
            let bytes: Vec<u8> = kept.iter().flat_map(encode).collect();
            let p = self.root.join("journal");
            let tmp = p.with_extension("tmp");
            let res = (|| -> io::Result<File> {
                let _ = std::fs::remove_file(&tmp);
                let mut f = OpenOptions::new().create_new(true).append(true).mode(0o600).open(&tmp)?;
                f.write_all(&bytes)?;
                f.sync_all()?;
                std::fs::rename(&tmp, &p)?;
                Ok(f) // after the rename this handle is the journal
            })();
            let f = res.inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })?;
            self.journal = Some(f);
            self.len = bytes.len() as u64;
            self.entries = kept;
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

    /// Writes `data` at `root/path` only after keeping what is on disk now (as `Replaced`, unless
    /// it is this path's newest version) and the new version (as `kind`), both synced to disk.
    /// Nothing is written unless both are kept.
    pub fn keep_and_write(&mut self, root: &Path, unit: &str, path: &str, data: Option<&[u8]>, kind: Kind, origin: &str) -> io::Result<Option<Stamp>> {
        let p = root.join(path);
        let disk = std::fs::symlink_metadata(&p).ok().filter(|m| m.is_file()).and_then(|_| std::fs::read(&p).ok());
        if let Some(d) = &disk {
            let dh = hash_of(Some(d));
            if self.history(unit, path).first().map(|e| e.hash) != Some(dh) {
                self.put(unit, path, Some(d), Kind::Replaced, "")?;
            }
        }
        self.put(unit, path, data, kind, origin)?;
        self.journal()?.sync_data()?;
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
        drop(s);
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
        drop(s);
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

    fn root_ignores_perms(dir: &Path) -> bool {
        std::fs::write(dir.join("probe"), b"").is_ok_and(|_| std::fs::remove_file(dir.join("probe")).is_ok())
    }

    #[test]
    fn age_counts_from_when_a_version_stopped_being_current() {
        let d = tmp();
        let mut s = Store::open_at(d.clone()).unwrap();
        let day = 86_400_000;
        let kept = s.put("u", "old", Some(b"written 60 days ago, deleted today"), Kind::Local, "").unwrap();
        s.put("u", "old", None, Kind::Remote, "acer").unwrap();
        let a = s.put("u", "f", Some(b"a"), Kind::Local, "").unwrap();
        let b = s.put("u", "f", Some(b"b"), Kind::Local, "").unwrap();
        s.put("u", "f", Some(b"c"), Kind::Local, "").unwrap();
        for (e, age) in s.entries.iter_mut().zip([60, 0, 60, 40, 0]) {
            e.t_ms = now_ms() - age * day;
        }
        // a stopped being current 40 days ago (when b came); b stopped today
        assert_eq!(s.prune(30, 1 << 30).unwrap(), (1, 1));
        assert!(s.get(&a).is_err());
        assert_eq!(s.get(&b).unwrap().as_deref(), Some(&b"b"[..]));
        assert_eq!(s.get(&kept).unwrap().as_deref(), Some(&b"written 60 days ago, deleted today"[..]));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn replaced_unless_the_newest_version_is_on_disk() {
        let d = tmp();
        let root = d.join("wt");
        std::fs::create_dir_all(&root).unwrap();
        let mut s = Store::open_at(d.join("store")).unwrap();
        s.put("u", "f", Some(b"A"), Kind::Local, "").unwrap();
        s.put("u", "f", Some(b"B"), Kind::Remote, "acer").unwrap();
        std::fs::write(root.join("f"), b"A").unwrap(); // went back to A here, not scanned yet
        s.keep_and_write(&root, "u", "f", Some(b"C"), Kind::Remote, "acer").unwrap();
        let h: Vec<Kind> = s.history("u", "f").iter().map(|e| e.kind).collect();
        assert_eq!(h, [Kind::Remote, Kind::Replaced, Kind::Remote, Kind::Local]);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_damaged_record_is_kept_and_disables_the_store() {
        // payload: t_ms 8 · unit len 4 + "u" · path len 4 + "f" · hash 32 · kind 1 · origin len 4; header 8
        let rec = 8 + 8 + 5 + 5 + 32 + 1 + 4 + 4;
        // (what, byte, xor mask)
        let cases = [
            ("first record's unit length", 8 + 8 + 3, 2u8), // 1 -> 3: plausible, eats into the path
            ("first record's path", 8 + 8 + 5 + 4, 1),     // "f" -> "g"
            ("first record's kind", 8 + 8 + 5 + 5 + 32, 9), // re-signed below, like a newer binary's kind
            ("last record's unit length", rec + 8 + 8 + 3, 0xc8), // 1 -> 201: would read past the end, not a torn tail
            ("last record's header", rec + 3, 0xff),
        ];
        for (what, at, mask) in cases {
            let d = tmp();
            let mut s = Store::open_at(d.clone()).unwrap();
            let h = s.put("u", "f", Some(b"one"), Kind::Local, "").unwrap();
            s.put("u", "f", Some(b"two"), Kind::Local, "").unwrap();
            drop(s);
            let j = d.join("journal");
            let mut bytes = std::fs::read(&j).unwrap();
            assert_eq!(bytes.len(), 2 * rec);
            bytes[at] ^= mask;
            if what.contains("kind") {
                let p = bytes[8..rec - 4].to_vec();
                bytes[rec - 4..rec].copy_from_slice(&sum4(&p));
            }
            std::fs::write(&j, &bytes).unwrap();
            let mut s = Store::open_at(d.clone()).unwrap();
            assert!(s.broken(), "{what}");
            assert_eq!(std::fs::metadata(&j).unwrap().len(), bytes.len() as u64, "{what}");
            assert!(std::fs::read_dir(&d).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("journal.bad.")), "{what}");
            assert!(s.put("u", "f", Some(b"three"), Kind::Remote, "acer").is_err(), "{what}");
            let root = d.join("wt");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("f"), b"mine").unwrap();
            assert!(s.keep_and_write(&root, "u", "f", Some(b"theirs"), Kind::Remote, "acer").is_err(), "{what}");
            assert_eq!(std::fs::read(root.join("f")).unwrap(), b"mine", "{what}");
            assert_eq!(s.prune(0, 0).unwrap(), (0, 0), "{what}");
            assert!(d.join("objects").join(hex(&h)).exists(), "{what}");
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn a_torn_tail_is_cut_wherever_it_stops() {
        let e = Entry { t_ms: 1, unit: "u".into(), path: "f".into(), hash: NONE, kind: Kind::Local, origin: "x".into() };
        let rec = encode(&e);
        for cut in [1, 5, 8, 9, 30, rec.len() - 1] {
            let d = tmp();
            let mut s = Store::open_at(d.clone()).unwrap();
            s.put("u", "f", Some(b"one"), Kind::Local, "").unwrap();
            drop(s);
            let mut f = OpenOptions::new().append(true).open(d.join("journal")).unwrap();
            f.write_all(&rec[..cut]).unwrap();
            drop(f);
            let mut s = Store::open_at(d.clone()).unwrap();
            assert!(!s.broken(), "cut at {cut}");
            s.put("u", "f", None, Kind::Local, "").unwrap();
            drop(s);
            assert_eq!(Store::open_at(d.clone()).unwrap().history("u", "f").len(), 2, "cut at {cut}");
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn an_unreadable_journal_is_an_error_not_an_empty_store() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp();
        let mut s = Store::open_at(d.clone()).unwrap();
        s.put("u", "f", Some(b"one"), Kind::Local, "").unwrap();
        drop(s);
        let j = d.join("journal");
        let len = std::fs::metadata(&j).unwrap().len();
        std::fs::set_permissions(&j, std::fs::Permissions::from_mode(0o200)).unwrap(); // writable, not readable
        if std::fs::read(&j).is_ok() {
            return std::fs::remove_dir_all(d).unwrap(); // root
        }
        assert!(Store::open_at(d.clone()).is_err());
        std::fs::set_permissions(&j, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::metadata(&j).unwrap().len(), len);
        assert_eq!(Store::open_at(d.clone()).unwrap().history("u", "f").len(), 1);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_failed_prune_leaves_the_store_as_it_was() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp();
        let mut s = Store::open_at(d.clone()).unwrap();
        s.put("u", "f", Some(b"one"), Kind::Local, "").unwrap();
        s.put("u", "f", Some(b"two"), Kind::Local, "").unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o500)).unwrap(); // no journal.tmp
        if root_ignores_perms(&d) {
            return std::fs::remove_dir_all(d).unwrap();
        }
        assert!(s.prune(0, 0).is_err());
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(s.history("u", "f").len(), 2);
        s.put("u", "f", Some(b"three"), Kind::Local, "").unwrap();
        assert_eq!(s.prune(0, 0).unwrap().0, 2);
        s.put("u", "f", Some(b"four"), Kind::Local, "").unwrap(); // goes to the new journal
        drop(s);
        let s = Store::open_at(d.clone()).unwrap();
        let h: Vec<Hash> = s.history("u", "f").iter().map(|e| e.hash).collect();
        assert_eq!(h, [hash_of(Some(b"four")), hash_of(Some(b"three"))]);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn one_daemon_per_store() {
        let d = tmp();
        let s = Store::open_at(d.clone()).unwrap();
        assert_eq!(Store::open_at(d.clone()).err().map(|e| e.kind()), Some(io::ErrorKind::WouldBlock));
        drop(s);
        Store::open_at(d.clone()).unwrap();
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_damaged_object_is_rewritten() {
        let d = tmp();
        let mut s = Store::open_at(d.clone()).unwrap();
        let h = s.put("u", "a", Some(b"abc"), Kind::Local, "").unwrap();
        let obj = d.join("objects").join(hex(&h));
        std::fs::write(&obj, b"").unwrap(); // crash before the data reached the disk
        drop(s);
        let mut s = Store::open_at(d.clone()).unwrap();
        assert!(s.get(&h).is_err());
        s.put("u", "b", Some(b"abc"), Kind::Local, "").unwrap();
        assert_eq!(s.get(&h).unwrap().as_deref(), Some(&b"abc"[..]));
        std::fs::write(&obj, b"abd").unwrap(); // bit rot
        assert!(s.get(&h).is_err());
        s.put("u", "c", Some(b"abc"), Kind::Local, "").unwrap();
        assert_eq!(s.get(&h).unwrap().as_deref(), Some(&b"abc"[..]));
        std::fs::remove_dir_all(d).unwrap();
    }
}
