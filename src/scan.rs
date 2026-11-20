//! Finds what changed in a worktree: either a full walk (startup, every 30 s, or the plan-B poll)
//! or just the paths the watcher / a hook reported. Compares (mtime, size, inode) before reading.
//! A drive (a plain folder) scans the same way, minus envs and any repo nested in it.
#![forbid(unsafe_code)]
use crate::crypto::{hex, sha256};
use crate::fsops::{self, has_secret, is_dotgit, is_env_name, is_secret, Stamp, MAX_FILE, TMP_PREFIX};
use crate::git::Ignore;
use crate::log;
use crate::sync::Hash;
use crate::wire::{Rd, W};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Default, Debug, PartialEq)]
pub struct Changes {
    /// synced files whose content changed (None = deleted)
    pub files: Vec<(String, Option<Vec<u8>>)>,
    /// `.env*` files whose content changed: (path, content, ignored by git)
    pub env: Vec<(String, Option<Vec<u8>>, bool)>,
    /// directories seen (the Linux watcher adds a watch for each)
    pub dirs: Vec<String>,
}

pub struct Scanner {
    pub root: PathBuf,
    ignore: Option<Ignore>,
    ignored: HashMap<String, bool>,
    pub files: HashMap<String, (Stamp, Hash)>,
    env: HashMap<String, Stamp>,
    gitignores: HashMap<String, Stamp>,
    warned: HashSet<String>,
    dirty_index: bool,
    /// drives: the empty bare repo `git check-ignore` runs against
    drive: Option<PathBuf>,
    /// drives: (device, inode) of the root, kept in the index, so a folder swapped in at the same
    /// path (unmounted, moved and recreated) is never read as "every file was deleted"
    root_id: Option<(u64, u64)>,
    /// hilo's own folder (keys, history): never scanned, whatever holds it
    home: PathBuf,
}

fn index_path(root: &Path) -> String {
    format!("index/{}", &hex(&sha256(root.to_string_lossy().as_bytes()))[..16])
}

impl Scanner {
    pub fn new(root: &Path) -> Self {
        let mut s = Scanner {
            root: root.into(),
            ignore: None,
            ignored: HashMap::new(),
            files: HashMap::new(),
            env: HashMap::new(),
            gitignores: HashMap::new(),
            warned: HashSet::new(),
            dirty_index: false,
            drive: None,
            root_id: None,
            home: crate::daemon::canonical_maybe_missing(&crate::config::home()),
        };
        // Persistent index: a restart doesn't re-hash the whole repo. Path "" holds the root's identity.
        let raw = std::fs::read(crate::config::path(&index_path(root))).unwrap_or_default();
        let mut r = Rd(&raw);
        while let (Some(p), Some(a), Some(b), Some(c), Some(d), Some(h)) = (r.str(), r.u64(), r.u64(), r.u64(), r.u64(), r.hash()) {
            if p.is_empty() {
                s.root_id = Some((a, b));
            } else {
                s.files.insert(p, ((a as i64, b as i64, c, d), h));
            }
        }
        s
    }

    /// Drives: the root is a folder, and the same folder the index was built from.
    pub fn root_ok(&self) -> io::Result<(u64, u64)> {
        let gone = || io::Error::other(format!("drive folder {} is gone", self.root.display()));
        std::fs::metadata(&self.root).ok().filter(|m| m.is_dir()).ok_or_else(gone)?;
        match fsops::dir_id(&self.root).map_err(|_| gone())? {
            id if self.root_id.is_some_and(|r| r != id) => {
                Err(io::Error::other(format!("{} is another folder now (unmounted, or moved and recreated); `hilo add --drive` it to sync this one", self.root.display())))
            }
            id => Ok(id),
        }
    }

    /// The user says the folder there now is the drive: forget the old one's index, so files it
    /// doesn't have are fetched again instead of deleted everywhere.
    pub fn reset_root(&mut self) {
        self.root_id = None;
        self.files.clear();
        self.dirty_index = true;
    }

    pub fn drive(root: &Path, label: &str) -> Self {
        let mut s = Scanner::new(root);
        s.drive = Some(crate::config::path(&format!("tmp/drive-ignore/{label}.git")));
        s
    }

    pub fn save_index(&mut self) -> io::Result<()> {
        if !self.dirty_index {
            return Ok(());
        }
        let mut w = W::default();
        for (p, ((a, b, c, d), h)) in &self.files {
            w.str(p).u64(*a as u64).u64(*b as u64).u64(*c).u64(*d).hash(h);
        }
        if let Some((dev, ino)) = self.root_id {
            w.str("").u64(dev).u64(ino).u64(0).u64(0).hash(&[0; 32]);
        }
        self.dirty_index = false;
        crate::config::write_private(&index_path(&self.root), &w.0)
    }

    /// Forget a file so the next look reads it again.
    pub fn forget(&mut self, rel: &str) {
        self.files.remove(rel);
        self.env.remove(rel);
    }

    /// hilo itself just wrote this; don't report it back.
    pub fn note(&mut self, rel: &str, st: Option<Stamp>, h: Hash) {
        match st {
            Some(st) if is_env_name(rel.rsplit('/').next().unwrap_or(rel)) => {
                self.env.insert(rel.into(), st);
            }
            Some(st) => {
                self.files.insert(rel.into(), (st, h));
            }
            None => {
                self.files.remove(rel);
                self.env.remove(rel);
            }
        }
        self.dirty_index = true;
    }

    pub fn is_ignored(&mut self, rel: &str) -> io::Result<bool> {
        if let Some(v) = self.ignored.get(rel) {
            return Ok(*v);
        }
        if self.ignore.is_none() {
            self.ignore = Some(match &self.drive {
                Some(g) => Ignore::drive(&self.root, g)?,
                None => Ignore::new(&self.root)?,
            });
        }
        let v = self.ignore.as_mut().unwrap().ignored(rel);
        if v.is_err() {
            self.ignore = None; // restart next time
        }
        let v = v?;
        self.ignored.insert(rel.into(), v);
        Ok(v)
    }

    fn warn(&mut self, rel: &str, why: &str) {
        if self.warned.insert(rel.into()) {
            log!("skip {rel}: {why}");
        }
    }

    /// Drives: a directory holding a repo is skipped (that repo syncs on its own, if added).
    fn nested(&mut self, rel: &str) -> bool {
        let yes = self.drive.is_some() && std::fs::symlink_metadata(self.root.join(rel).join(".git")).is_ok();
        if yes {
            self.warn(&format!("{rel}/"), "a git repo inside a drive; add it with `hilo add`");
        }
        yes
    }

    pub fn scan(&mut self) -> io::Result<Changes> {
        self.walk("")
    }

    /// Look only at these paths (relative). Directories are walked.
    pub fn check(&mut self, rels: &[String]) -> io::Result<Changes> {
        let mut out = Changes::default();
        let mut done: HashSet<&str> = HashSet::new();
        for rel in rels {
            if rel.is_empty() {
                return self.walk("");
            }
            if !done.insert(rel) || rel.split('/').any(is_dotgit) {
                continue;
            }
            // already covered by a parent in this batch
            if rels.iter().any(|o| o != rel && rel.starts_with(&format!("{o}/"))) {
                continue;
            }
            if rel.split('/').any(|c| c.starts_with(TMP_PREFIX)) {
                continue;
            }
            let c = self.walk(rel)?;
            out.files.extend(c.files);
            out.env.extend(c.env);
            out.dirs.extend(c.dirs);
        }
        Ok(out)
    }

    /// Look at `prefix` ("" = everything): a file, a directory, or something now gone.
    fn walk(&mut self, prefix: &str) -> io::Result<Changes> {
        if self.root.starts_with(&self.home) {
            return Err(io::Error::other(format!("{} is inside hilo's own folder; not syncing it", self.root.display())));
        }
        if !prefix.is_empty() && self.root.join(prefix).starts_with(&self.home) {
            return Ok(Changes::default());
        }
        if self.drive.is_some() {
            let id = self.root_ok()?;
            if self.root_id.is_none() {
                self.root_id = Some(id);
                self.dirty_index = true;
            }
        }
        if self.drive.is_none() && !self.root.join(".git").exists() {
            return Err(io::Error::other(format!("{} is not a git worktree anymore", self.root.display())));
        }
        let mut out = Changes::default();
        let mut seen = HashSet::new();
        let mut complete = true;
        let mut rules_changed = false;
        let mut stack = vec![];
        // ancestors of a targeted path must not be ignored either
        let ancestors_ok = (|| -> io::Result<bool> {
            let parts: Vec<&str> = prefix.split('/').collect();
            for i in 1..parts.len() {
                if self.nested(&parts[..i].join("/")) || self.is_ignored(&format!("{}/", parts[..i].join("/")))? {
                    return Ok(false);
                }
            }
            Ok(true)
        })()?;
        if prefix.is_empty() {
            stack.push(String::new());
        } else if ancestors_ok {
            match std::fs::symlink_metadata(self.root.join(prefix)) {
                Ok(m) if m.is_dir() => {
                    if !self.nested(prefix) && !self.is_ignored(&format!("{prefix}/"))? {
                        out.dirs.push(prefix.to_string());
                        stack.push(prefix.to_string());
                    }
                }
                Ok(m) => self.look(prefix, &m, &mut out, &mut seen, &mut rules_changed)?,
                Err(_) => {} // gone: handled below
            }
        }
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(self.root.join(&dir)) else {
                complete = false;
                continue;
            };
            for e in entries {
                let Ok(e) = e else {
                    complete = false;
                    continue;
                };
                let name = e.file_name();
                let Some(name) = name.to_str() else { continue }; // non-UTF-8 names don't travel
                if is_dotgit(name) || name.starts_with(TMP_PREFIX) {
                    continue;
                }
                let rel = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
                let Ok(m) = e.metadata() else {
                    complete = false;
                    continue;
                };
                if m.is_dir() {
                    if !m.file_type().is_symlink() && self.root.join(&rel) != self.home && !self.nested(&rel) && !self.is_ignored(&format!("{rel}/"))? {
                        out.dirs.push(rel.clone());
                        stack.push(rel);
                    }
                    continue;
                }
                self.look(&rel, &m, &mut out, &mut seen, &mut rules_changed)?;
            }
        }
        if complete && prefix.is_empty() && self.drive.is_some() && seen.is_empty() && !self.files.is_empty() {
            // an unmounted or emptied-at-once drive must not turn into deletions on every machine
            return Err(io::Error::other(format!(
                "drive folder {} is empty but had {} files; not deleting them everywhere (files deleted one at a time still sync)",
                self.root.display(),
                self.files.len()
            )));
        }
        if complete {
            let under = |k: &str| prefix.is_empty() || k == prefix || k.starts_with(&format!("{prefix}/"));
            let gone: Vec<String> = self.files.keys().filter(|k| under(k) && !seen.contains(*k)).cloned().collect();
            for rel in gone {
                self.files.remove(&rel);
                self.dirty_index = true;
                // still on disk = became ignored, not deleted
                if std::fs::symlink_metadata(self.root.join(&rel)).is_err() {
                    out.files.push((rel, None));
                }
            }
            let gone: Vec<String> = self.env.keys().filter(|k| under(k) && !seen.contains(*k)).cloned().collect();
            for rel in gone {
                self.env.remove(&rel);
                if std::fs::symlink_metadata(self.root.join(&rel)).is_err() {
                    out.env.push((rel, None, true));
                }
            }
        }
        if rules_changed {
            // new ignore rules: forget every answer and look at everything again right away
            self.ignore = None;
            self.ignored.clear();
            self.env.clear();
            let more = self.walk("")?;
            out.files.extend(more.files);
            out.env.extend(more.env);
            out.dirs.extend(more.dirs);
        }
        Ok(out)
    }

    fn look(&mut self, rel: &str, m: &std::fs::Metadata, out: &mut Changes, seen: &mut HashSet<String>, rules_changed: &mut bool) -> io::Result<()> {
        if !m.is_file() || m.file_type().is_symlink() {
            return Ok(()); // ponytail: symlinks don't sync in v1
        }
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let st = fsops::stamp(m);
        if is_env_name(name) && self.drive.is_some() {
            self.warn(rel, "env files never sync from a drive");
            return Ok(());
        }
        if is_env_name(name) {
            seen.insert(rel.to_string());
            if self.env.get(rel) != Some(&st) {
                let ignored = self.is_ignored(rel)?;
                if let Ok(d) = std::fs::read(self.root.join(rel)) {
                    self.env.insert(rel.into(), st);
                    out.env.push((rel.into(), Some(d), ignored));
                }
            }
            return Ok(());
        }
        if is_secret(rel) {
            self.warn(rel, "secret file: never synced (put values in the vault)");
            return Ok(());
        }
        if self.is_ignored(rel)? {
            return Ok(());
        }
        seen.insert(rel.to_string());
        if name == ".gitignore" && self.gitignores.insert(rel.into(), st).is_some_and(|old| old != st) {
            *rules_changed = true;
        }
        if self.files.get(rel).is_some_and(|f| f.0 == st) {
            return Ok(());
        }
        if m.len() > MAX_FILE {
            self.warn(rel, "larger than 50 MB");
            return Ok(());
        }
        let p = self.root.join(rel);
        let Ok(data) = std::fs::read(&p) else { return Ok(()) };
        if std::fs::symlink_metadata(&p).map(|m| fsops::stamp(&m)).ok() != Some(st) {
            seen.remove(rel); // written while we read: next round
            return Ok(());
        }
        if has_secret(&data) {
            self.warn(rel, "looks like it contains a key");
            return Ok(());
        }
        let h = sha256(&data);
        let changed = self.files.get(rel).is_none_or(|f| f.1 != h);
        self.files.insert(rel.into(), (st, h));
        self.dirty_index = true;
        if changed {
            out.files.push((rel.into(), Some(data)));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_sees_changes_and_respects_ignores() {
        let d = crate::git::tests::temp_repo();
        std::fs::write(d.join(".gitignore"), "node_modules/\n*.log\n.env*\n").unwrap();
        let mut s = Scanner { root: d.clone(), ignore: None, ignored: HashMap::new(), files: HashMap::new(), env: HashMap::new(), gitignores: HashMap::new(), warned: HashSet::new(), dirty_index: false, drive: None, root_id: None, home: PathBuf::from("/nonexistent") };
        let mut first: Vec<String> = s.scan().unwrap().files.into_iter().map(|c| c.0).collect();
        first.sort();
        assert_eq!(first, [".gitignore", "a.txt"]);
        std::fs::create_dir_all(d.join("node_modules/x")).unwrap();
        std::fs::write(d.join("node_modules/x/i.js"), "x").unwrap();
        std::fs::write(d.join("debug.log"), "x").unwrap();
        std::fs::write(d.join(".env"), "SECRET=1").unwrap();
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/b.ts"), "b").unwrap();
        let c = s.scan().unwrap();
        assert_eq!(c.files, vec![("src/b.ts".to_string(), Some(b"b".to_vec()))]);
        assert_eq!(c.env, vec![(".env".to_string(), Some(b"SECRET=1".to_vec()), true)]);
        assert!(s.scan().unwrap().files.is_empty());
        // targeted checks
        std::fs::write(d.join("src/b.ts"), "b2").unwrap();
        std::fs::write(d.join("node_modules/x/i.js"), "y").unwrap();
        let c = s.check(&["src/b.ts".into(), "node_modules/x/i.js".into()]).unwrap();
        assert_eq!(c.files, vec![("src/b.ts".to_string(), Some(b"b2".to_vec()))]);
        std::fs::remove_dir_all(d.join("src")).unwrap();
        assert_eq!(s.check(&["src".into()]).unwrap().files, vec![("src/b.ts".to_string(), None)]);
        // hilo's own writes are not reported back
        let st = fsops::write(&d, "w.txt", Some(b"mine")).unwrap();
        s.note("w.txt", st, sha256(b"mine"));
        assert!(s.check(&["w.txt".into()]).unwrap().files.is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn drive_scan() {
        let _home = crate::config::TEST_HOME.lock().unwrap_or_else(|e| e.into_inner()); // its ignore repo lives there
        let d = std::env::temp_dir().join(format!("hilo-drive-{}", hex(&crate::crypto::random::<4>())));
        let w = |p: &str, c: &str| {
            std::fs::create_dir_all(d.join(p).parent().unwrap()).unwrap();
            std::fs::write(d.join(p), c).unwrap();
        };
        w(".gitignore", "*.log\n");
        w("a.txt", "a");
        w("sub/.gitignore", "secret.txt\n!keep.log\n"); // nested .gitignore files count, negations too
        w("sub/b.txt", "b");
        w("sub/x.log", "x");
        w("sub/keep.log", "k");
        w("sub/secret.txt", "s");
        w("sub/deep/c.txt", "c");
        w("node_modules/m.js", "m"); // built-in excludes
        w(".DS_Store", "x");
        w("sub/.hilo-thing", "x");
        w(".env", "K=1"); // never from a drive, no vault here
        w("sub/.env.local", "K=1");
        w("id.pem", "x");
        w("nested/.git/HEAD", "ref: refs/heads/main\n"); // a repo inside: skipped whole
        w("nested/n.txt", "n");
        let d = fsops::canonical(&d).unwrap();
        let mut s = Scanner::drive(&d, &format!("t{}", hex(&crate::crypto::random::<4>())));
        let names = |c: &Changes| {
            let mut v: Vec<String> = c.files.iter().map(|f| f.0.clone()).collect();
            v.sort();
            v
        };
        let c = s.scan().unwrap();
        assert_eq!(names(&c), [".gitignore", "a.txt", "sub/.gitignore", "sub/b.txt", "sub/deep/c.txt", "sub/keep.log"]);
        assert!(c.env.is_empty());
        assert!(s.check(&["nested/n.txt".into(), "nested".into(), ".env".into()]).unwrap().files.is_empty());
        // a .gitignore change re-reads everything: x.log now syncs, deep/ stops (still on disk: not a deletion)
        std::fs::write(d.join(".gitignore"), "deep/\n").unwrap();
        assert_eq!(names(&s.scan().unwrap()), [".gitignore", "sub/x.log"]);
        assert!(!s.files.contains_key("sub/deep/c.txt"));
        std::fs::remove_file(d.join("a.txt")).unwrap();
        assert_eq!(s.check(&["a.txt".into()]).unwrap().files, vec![("a.txt".to_string(), None)]);
        std::fs::remove_dir_all(&d).unwrap();
        assert!(s.scan().is_err()); // the folder is gone: an error, not every file deleted
    }

    #[test]
    fn drive_root_swapped_or_emptied() {
        let _home = crate::config::TEST_HOME.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::ensure_home().unwrap(); // the index lives there
        let d = std::env::temp_dir().join(format!("hilo-swap-{}", hex(&crate::crypto::random::<4>())));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a.txt"), "a").unwrap();
        std::fs::write(d.join("sub/b.txt"), "b").unwrap();
        let d = fsops::canonical(&d).unwrap();
        let label = format!("t{}", hex(&crate::crypto::random::<4>()));
        let mut s = Scanner::drive(&d, &label);
        assert_eq!(s.scan().unwrap().files.len(), 2);
        s.save_index().unwrap();
        // everything deleted at once (or the volume's contents vanished): not a mass delete
        std::fs::remove_file(d.join("a.txt")).unwrap();
        std::fs::remove_file(d.join("sub/b.txt")).unwrap();
        assert!(s.scan().is_err());
        // deletions the watcher reports one by one still sync
        assert_eq!(s.check(&["a.txt".into()]).unwrap().files, vec![("a.txt".to_string(), None)]);
        std::fs::write(d.join("a.txt"), "a").unwrap();
        std::fs::write(d.join("sub/b.txt"), "b").unwrap();
        s.scan().unwrap();
        s.save_index().unwrap();
        // the folder moved away and an empty one took its place (or an unmounted mount point)
        let away = d.with_extension("away");
        std::fs::rename(&d, &away).unwrap();
        std::fs::create_dir(&d).unwrap();
        assert!(s.scan().is_err() && s.root_ok().is_err());
        let mut again = Scanner::drive(&d, &label); // a restart remembers which folder it was
        assert!(again.root_ok().is_err() && again.scan().is_err());
        // the same folder back: fine
        std::fs::remove_dir(&d).unwrap();
        std::fs::rename(&away, &d).unwrap();
        assert!(Scanner::drive(&d, &label).root_ok().is_ok());
        // re-added on purpose: the new folder is the drive, and what it lacks is not "deleted"
        std::fs::rename(&d, &away).unwrap();
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("c.txt"), "c").unwrap();
        again.reset_root();
        assert_eq!(again.scan().unwrap().files, vec![("c.txt".to_string(), Some(b"c".to_vec()))]);
        std::fs::remove_dir_all(&d).unwrap();
        std::fs::remove_dir_all(&away).unwrap();
    }
}
