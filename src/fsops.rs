//! Validating and writing paths that arrive from other machines.
//! The receiver trusts nothing the sender validated.
use crate::crypto::{hex, random};
use std::ffi::{c_char, c_int, CString};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const MAX_FILE: u64 = 50 * 1024 * 1024;
pub const TMP_PREFIX: &str = ".hilo-tmp-";

/// Characters HFS+/APFS ignore when comparing names (same set git checks).
fn ignorable(c: char) -> bool {
    matches!(c as u32, 0x200c..=0x200f | 0x202a..=0x202e | 0x206a..=0x206f | 0xfeff)
}

/// `.git` under any case, with ignorable code points stripped.
// ponytail: no full Unicode normalization; NFD look-alikes of ".git" need decomposed letters, which ASCII ".git" has none of.
pub fn is_dotgit(comp: &str) -> bool {
    let s: String = comp.chars().filter(|c| !ignorable(*c)).collect();
    s.eq_ignore_ascii_case(".git")
}

pub fn valid_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 4096
        && !p.starts_with('/')
        && !p.contains('\0')
        && p.split('/').all(|c| !c.is_empty() && c != "." && c != ".." && c.len() <= 255 && !is_dotgit(c))
}

/// `.env`, `.env.local`, ... (not `.env.example`, which belongs in git). They travel through the vault.
pub fn is_env_name(name: &str) -> bool {
    (name == ".env" || name.starts_with(".env.")) && name != ".env.example"
}

/// Secret files that never travel as files.
pub fn is_secret(p: &str) -> bool {
    let name = p.rsplit('/').next().unwrap_or(p);
    is_env_name(name)
        || [".pem", ".key", ".p12"].iter().any(|s| name.ends_with(s))
        || ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"].iter().any(|s| name.starts_with(s))
        || (name.starts_with("serviceAccount") && name.ends_with(".json"))
        || ["credentials.json", ".npmrc", ".dev.vars", "local.settings.json"].contains(&name)
}

/// Paths a file sync must never carry: hilo temp files and secrets.
pub fn excluded(p: &str) -> bool {
    p.rsplit('/').next().unwrap_or(p).starts_with(TMP_PREFIX) || is_secret(p)
}

/// Paths that run code: a remote change to them waits for `hilo approve`.
/// `package.json` only counts when its "scripts" change (see `scripts_of`).
pub fn runs_code(p: &str) -> bool {
    let name = p.rsplit('/').next().unwrap_or(p);
    let first = p.split('/').next().unwrap_or(p);
    [".claude", ".vscode", ".husky"].contains(&first)
        || p.starts_with(".github/workflows/")
        || [".mcp.json", ".envrc", "Makefile", "package.json", "CLAUDE.md", "AGENTS.md"].contains(&name)
}

/// The "scripts" object of a package.json, as JSON text (None if absent or unparsable).
pub fn scripts_of(pkg: Option<&[u8]>) -> Option<String> {
    let j = crate::json::Json::parse(std::str::from_utf8(pkg?).ok()?)?;
    j.get("scripts").map(|s| s.to_string())
}

/// Known key formats, as whole tokens: AWS key ids are `AKIA` + 16 of [A-Z0-9], GitHub tokens
/// `ghp_` + 36 alphanumerics, Stripe live keys `sk_live_` + 16 or more. A match glued to other token
/// characters (base64 blobs, minified code) is not a key.
pub fn has_secret(data: &[u8]) -> bool {
    let tok = |c: u8| c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'_' || c == b'-';
    let rule = |pre: &[u8], n: usize, ok: fn(u8) -> bool, exact: bool| {
        data.windows(pre.len()).enumerate().any(|(i, w)| {
            if w != pre || (i > 0 && tok(data[i - 1])) {
                return false;
            }
            let rest = &data[i + pre.len()..];
            let len = rest.iter().take_while(|c| ok(**c)).count();
            len >= n && (!exact || len == n) && rest.get(len).is_none_or(|c| !tok(*c))
        })
    };
    rule(b"AKIA", 16, |c| c.is_ascii_uppercase() || c.is_ascii_digit(), true)
        || rule(b"ghp_", 36, |c| c.is_ascii_alphanumeric(), true)
        || rule(b"sk_live_", 16, |c| c.is_ascii_alphanumeric(), false)
        || data.windows(16).any(|w| w == b"PRIVATE KEY-----")
}

// ---- openat-based writes: a parent turned into a symlink can't redirect us ----

#[cfg(target_os = "macos")]
mod c {
    pub const O_WRONLY: i32 = 1;
    pub const O_NOFOLLOW: i32 = 0x100;
    pub const O_CREAT: i32 = 0x200;
    pub const O_EXCL: i32 = 0x800;
    pub const O_DIRECTORY: i32 = 0x100000;
    pub const O_CLOEXEC: i32 = 0x1000000;
    pub type Mode = u16;
}
#[cfg(target_os = "linux")]
mod c {
    pub const O_WRONLY: i32 = 1;
    pub const O_CREAT: i32 = 0x40;
    pub const O_EXCL: i32 = 0x80;
    pub const O_CLOEXEC: i32 = 0x80000;
    #[cfg(target_arch = "aarch64")]
    pub const O_DIRECTORY: i32 = 0x4000;
    #[cfg(target_arch = "aarch64")]
    pub const O_NOFOLLOW: i32 = 0x8000;
    #[cfg(not(target_arch = "aarch64"))]
    pub const O_DIRECTORY: i32 = 0x10000;
    #[cfg(not(target_arch = "aarch64"))]
    pub const O_NOFOLLOW: i32 = 0x20000;
    pub type Mode = u32;
}

extern "C" {
    fn openat(fd: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
    fn mkdirat(fd: c_int, path: *const c_char, mode: c::Mode) -> c_int;
    fn renameat(from_fd: c_int, from: *const c_char, to_fd: c_int, to: *const c_char) -> c_int;
    fn unlinkat(fd: c_int, path: *const c_char, flags: c_int) -> c_int;
}

fn cstr(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in path"))
}

fn check(r: c_int) -> io::Result<c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

fn open_at(dir: &OwnedFd, name: &str, flags: c_int, mode: u32) -> io::Result<OwnedFd> {
    let n = cstr(name)?;
    let fd = check(unsafe { openat(dir.as_raw_fd(), n.as_ptr(), flags | c::O_NOFOLLOW | c::O_CLOEXEC, mode) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Refuse `name` when only a case-insensitive twin exists: on APFS both are the same file.
// ponytail: one readdir per component per write; cache listings if deep trees with huge dirs get slow.
fn case_twin(dir: &Path, name: &str) -> io::Result<()> {
    let mut twin = None;
    for e in std::fs::read_dir(dir)? {
        let other = e?.file_name();
        let other = other.to_string_lossy();
        if other == name {
            return Ok(());
        }
        if other.eq_ignore_ascii_case(name) {
            twin = Some(other.into_owned());
        }
    }
    match twin {
        Some(o) => Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("case collision with {o}"))),
        None => Ok(()),
    }
}

/// Opens every parent of `rel` with O_NOFOLLOW, creating missing ones if asked.
fn open_parent(root: &Path, rel: &str, create: bool) -> io::Result<(OwnedFd, PathBuf, String)> {
    if !valid_path(rel) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid path"));
    }
    let mut dir: OwnedFd = File::open(root)?.into();
    let mut at = root.to_path_buf();
    let mut comps: Vec<&str> = rel.split('/').collect();
    let name = comps.pop().unwrap().to_string();
    for comp in comps {
        let flags = c::O_DIRECTORY;
        case_twin(&at, comp)?;
        dir = match open_at(&dir, comp, flags, 0) {
            Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                let n = cstr(comp)?;
                check(unsafe { mkdirat(dir.as_raw_fd(), n.as_ptr(), 0o755) })?;
                open_at(&dir, comp, flags, 0)?
            }
            r => r?,
        };
        at.push(comp);
    }
    Ok((dir, at, name))
}

pub type Stamp = (i64, i64, u64, u64); // mtime s, mtime ns, size, inode

pub fn stamp(m: &std::fs::Metadata) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    (m.mtime(), m.mtime_nsec(), m.size(), m.ino())
}

pub fn write(root: &Path, rel: &str, data: Option<&[u8]>) -> io::Result<Option<Stamp>> {
    write_mode(root, rel, data, 0o644)
}

/// Atomic write (O_EXCL temp + renameat) or delete. Keeps the existing mode bits (`new_mode` for new files),
/// never setuid/setgid. Returns the written file's stamp, so the scanner doesn't re-read it.
pub fn write_mode(root: &Path, rel: &str, data: Option<&[u8]>, new_mode: u32) -> io::Result<Option<Stamp>> {
    let Some(data) = data else {
        let (dir, _, name) = match open_parent(root, rel, false) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            r => r?,
        };
        let n = cstr(&name)?;
        return match check(unsafe { unlinkat(dir.as_raw_fd(), n.as_ptr(), 0) }) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            r => r.map(|_| None),
        };
    };
    if data.len() as u64 > MAX_FILE {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "file too large"));
    }
    let (dir, at, name) = open_parent(root, rel, true)?;
    case_twin(&at, &name)?;
    let mode = match open_at(&dir, &name, 0, 0) {
        Ok(fd) => {
            let m = File::from(fd).metadata()?;
            if !m.is_file() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "target is not a regular file"));
            }
            m.permissions().mode() & 0o777
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => new_mode & 0o777,
        Err(e) => return Err(e), // ELOOP: target is a symlink
    };
    let tmp = format!("{TMP_PREFIX}{}", hex(&random::<8>()));
    let mut f = File::from(open_at(&dir, &tmp, c::O_WRONLY | c::O_CREAT | c::O_EXCL, 0o600)?);
    let res = (|| {
        f.write_all(data)?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        let st = stamp(&f.metadata()?); // rename keeps inode and mtime
        // ponytail: no fsync per file (PRD perf); a crash may leave the old version, which the anchor still has.
        let (t, n) = (cstr(&tmp)?, cstr(&name)?);
        check(unsafe { renameat(dir.as_raw_fd(), t.as_ptr(), dir.as_raw_fd(), n.as_ptr()) }).map(|_| Some(st))
    })();
    if res.is_err() {
        let t = cstr(&tmp)?;
        unsafe { unlinkat(dir.as_raw_fd(), t.as_ptr(), 0) };
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        for bad in ["", "/etc/passwd", "a/../b", "./a", "a//b", ".git/config", "x/.GIT/hooks/pre-commit", "x/.g\u{200c}it/y", "a\0b"] {
            assert!(!valid_path(bad), "{bad:?}");
        }
        for ok in ["a", "src/a.ts", ".gitignore", ".github/x.yml", "a/.gitkeep"] {
            assert!(valid_path(ok), "{ok:?}");
        }
        assert!(excluded(".env") && excluded("apps/web/.env.local") && excluded("a/.hilo-tmp-1"));
        assert!(!excluded(".env.example") && !excluded("src/main.rs") && !excluded(".envrc"));
        assert!(runs_code(".claude/settings.json") && runs_code(".github/workflows/ci.yml") && !runs_code("src/a.ts"));
        assert_eq!(scripts_of(Some(br#"{"name":"x","scripts":{"dev":"vite"}}"#)).as_deref(), Some(r#"{"dev":"vite"}"#));
        assert!(has_secret(b"key = AKIAABCDEFGHIJKLMNOP") && !has_secret(b"AKIA is a prefix"));
        assert!(has_secret(b"AWS_ID=\"AKIAIOSFODNN7EXAMPLE\"\n"));
        assert!(!has_secret(b"data:font/woff2;base64,xxAKIAAgAAAJwApgACAAAAnACqyy")); // base64 blob
        assert!(!has_secret(b" AKIAowAAgAAAogCkluQPWACi ")); // lowercase: not an AWS id
        assert!(has_secret(b"-----BEGIN OPENSSH PRIVATE KEY-----"));
    }

    #[test]
    fn writes_refuse_symlinked_parents() {
        let base = std::env::temp_dir().join(format!("hilo-fs-{}", hex(&random::<4>())));
        let root = base.join("repo");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        write(&root, "a/b/c.txt", Some(b"hi")).unwrap();
        assert_eq!(std::fs::read(root.join("a/b/c.txt")).unwrap(), b"hi");
        std::os::unix::fs::symlink(&outside, root.join("evil")).unwrap();
        assert!(write(&root, "evil/x.txt", Some(b"pwn")).is_err());
        assert!(!outside.join("x.txt").exists());
        std::os::unix::fs::symlink(outside.join("t"), root.join("link")).unwrap();
        assert!(write(&root, "link", Some(b"pwn")).is_err());
        write(&root, "A/b/c.txt", Some(b"x")).unwrap_err(); // case twin of "a"
        std::fs::set_permissions(root.join("a/b/c.txt"), std::fs::Permissions::from_mode(0o755)).unwrap();
        write(&root, "a/b/c.txt", Some(b"v2")).unwrap();
        assert_eq!(std::fs::metadata(root.join("a/b/c.txt")).unwrap().permissions().mode() & 0o777, 0o755);
        write(&root, "a/b/c.txt", None).unwrap();
        assert!(!root.join("a/b/c.txt").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
