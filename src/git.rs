//! Everything git, through the system `git` binary. Paths always go after `--` or via stdin.
#![forbid(unsafe_code)]
use crate::crypto::{hex, random, sha256};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

fn cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).env("GIT_TERMINAL_PROMPT", "0");
    c
}

fn run(mut c: Command, input: Option<&[u8]>) -> io::Result<Vec<u8>> {
    c.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn()?;
    if let Some(i) = input {
        child.stdin.take().unwrap().write_all(i)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(io::Error::other(format!("git: {}", err.trim())));
    }
    Ok(out.stdout)
}

fn git(dir: &Path, args: &[&str]) -> io::Result<String> {
    let mut c = cmd(dir);
    c.args(args);
    Ok(String::from_utf8_lossy(&run(c, None)?).trim().to_string())
}

pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"]).ok().map(PathBuf::from)
}

pub fn gitdir(root: &Path) -> io::Result<PathBuf> {
    git(root, &["rev-parse", "--absolute-git-dir"]).map(PathBuf::from)
}

/// Current branch, read straight from HEAD (no process per scan). None = detached.
pub fn branch(gitdir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    head.trim().strip_prefix("ref: refs/heads/").map(str::to_string)
}

/// `git@github.com:a/b.git`, `https://user@github.com/a/b` -> `github.com/a/b`
/// `owner/repo` from any remote URL. Cloud agents often clone through a local git proxy
/// (`http://proxy@127.0.0.1:1234/git/owner/repo`), so the host can't be part of a repo's identity.
fn normalize_url(u: &str) -> String {
    let u = u.trim().trim_end_matches('/').trim_end_matches(".git");
    let u = u.split_once("://").map_or(u, |(_, rest)| rest);
    let u = u.rsplit_once('@').map_or(u, |(_, rest)| rest);
    let u = if u.contains('/') && u.find(':') < u.find('/') { u.replacen(':', "/", 1) } else { u.to_string() };
    let parts: Vec<&str> = u.split('/').filter(|p| !p.is_empty()).skip(1).collect(); // drop the host
    parts[parts.len().saturating_sub(2)..].join("/").to_lowercase()
}

/// Root commit (smallest, if several) + `owner/repo` of origin.
pub fn repo_id(root: &Path) -> io::Result<String> {
    let roots = git(root, &["rev-list", "--max-parents=0", "HEAD"])?;
    let first = roots.lines().min().unwrap_or_default().to_string();
    let origin = git(root, &["config", "--get", "remote.origin.url"]).unwrap_or_default();
    Ok(hex(&sha256(format!("{first}|{}", normalize_url(&origin)).as_bytes()))[..16].to_string())
}

/// Long-lived `git check-ignore` so the scan path never spawns per file.
pub struct Ignore {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
}

impl Ignore {
    pub fn new(root: &Path) -> io::Result<Self> {
        let mut c = cmd(root);
        c.args(["check-ignore", "--stdin", "-z", "--non-matching", "-v"])
            .env("GIT_FLUSH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = c.spawn()?;
        let stdin = child.stdin.take().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Ok(Ignore { child, stdin, out })
    }

    /// Directories must end in `/`.
    pub fn ignored(&mut self, rel: &str) -> io::Result<bool> {
        self.stdin.write_all(rel.as_bytes())?;
        self.stdin.write_all(b"\0")?;
        self.stdin.flush()?;
        let mut field = || -> io::Result<Vec<u8>> {
            let mut f = vec![];
            if self.out.read_until(0, &mut f)? == 0 {
                return Err(io::Error::other("check-ignore exited"));
            }
            f.pop();
            Ok(f)
        };
        let (source, _line, pattern, _path) = (field()?, field()?, field()?, field()?);
        Ok(!source.is_empty() && !pattern.starts_with(b"!"))
    }
}

impl Drop for Ignore {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Past this many conflict branches hilo stops making more (and so leaves the file alone).
pub const MAX_CONFLICTS: usize = 500;

fn conflict_room(root: &Path) -> io::Result<()> {
    if conflict_branches(root) >= MAX_CONFLICTS {
        return Err(io::Error::other(format!("{MAX_CONFLICTS} hilo/conflict/* branches already; delete old ones")));
    }
    Ok(())
}

/// Puts `data` (None = deleted) for `path` on `hilo/conflict/<branch>/<date>`, without touching the worktree.
pub fn save_conflict(root: &Path, branch: &str, path: &str, data: Option<&[u8]>, tmp_dir: &Path) -> io::Result<String> {
    conflict_room(root)?;
    let index = tmp_dir.join(format!("index-{}", hex(&random::<8>())));
    let res = (|| {
        let g = |args: &[&str], input: Option<&[u8]>| {
            let mut c = cmd(root);
            c.args(args)
                .env("GIT_INDEX_FILE", &index)
                .env("GIT_AUTHOR_NAME", "hilo")
                .env("GIT_AUTHOR_EMAIL", "hilo@localhost")
                .env("GIT_COMMITTER_NAME", "hilo")
                .env("GIT_COMMITTER_EMAIL", "hilo@localhost");
            run(c, input).map(|o| String::from_utf8_lossy(&o).trim().to_string())
        };
        g(&["read-tree", "HEAD"], None)?;
        let entry = match data {
            Some(d) => format!("100644 {}\t{path}\0", g(&["hash-object", "-w", "--stdin"], Some(d))?),
            None => format!("0 {}\t{path}\0", "0".repeat(40)),
        };
        g(&["update-index", "-z", "--index-info"], Some(entry.as_bytes()))?;
        let tree = g(&["write-tree"], None)?;
        let msg = format!("hilo: your version of {path}, kept after a conflict");
        let commit = g(&["commit-tree", &tree, "-p", "HEAD", "-m", &msg], None)?;
        let name = format!("hilo/conflict/{branch}/{}", crate::stamp(true));
        g(&["update-ref", &format!("refs/heads/{name}"), &commit, ""], None)?; // "" = must not exist yet
        Ok(name)
    })();
    let _ = std::fs::remove_file(&index);
    res
}

pub fn common_dir(root: &Path) -> io::Result<PathBuf> {
    git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).map(PathBuf::from)
}

pub fn valid_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn valid_branch(b: &str) -> bool {
    !b.starts_with('-') && git(Path::new("."), &["check-ref-format", "--branch", b]).is_ok_and(|o| o == b)
}

/// Every worktree of the repo at `root`: (path, branch). Detached ones have no branch.
pub fn worktrees(root: &Path) -> io::Result<Vec<(PathBuf, Option<String>)>> {
    let out = git(root, &["worktree", "list", "--porcelain"])?;
    let mut v = vec![];
    for block in out.split("\n\n") {
        let mut path = None;
        let mut branch = None;
        let mut skip = false;
        for l in block.lines() {
            if let Some(p) = l.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = l.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            } else if l == "bare" || l.starts_with("prunable") {
                skip = true;
            }
        }
        if let (Some(p), false) = (path, skip) {
            v.push((p, branch));
        }
    }
    Ok(v)
}

/// Tip of `branch`, read from the ref files (no process on the hot path).
pub fn read_ref(common: &Path, branch: &str) -> Option<String> {
    let name = format!("refs/heads/{branch}");
    if let Ok(s) = std::fs::read_to_string(common.join(&name)) {
        let s = s.trim().to_string();
        return valid_sha(&s).then_some(s);
    }
    let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
    packed.lines().find_map(|l| {
        let (sha, r) = l.split_once(' ')?;
        (r == name && valid_sha(sha)).then(|| sha.to_string())
    })
}

pub fn has_commit(root: &Path, sha: &str) -> bool {
    valid_sha(sha) && git(root, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_ok()
}

pub fn is_ancestor(root: &Path, a: &str, b: &str) -> bool {
    a == b || (valid_sha(a) && valid_sha(b) && git(root, &["merge-base", "--is-ancestor", a, b]).is_ok())
}

/// Commits needed to go from what everyone has (`exclude`) to `tip`. None if nothing is missing.
pub fn bundle(root: &Path, tip: &str, exclude: &[String], tmp_dir: &Path) -> io::Result<Option<Vec<u8>>> {
    let file = tmp_dir.join(format!("out-{}.bundle", hex(&random::<8>())));
    let refname = format!("refs/hilo/out/{}", hex(&random::<4>()));
    git(root, &["update-ref", &refname, tip])?;
    let mut args: Vec<String> = vec!["bundle".into(), "create".into(), "-q".into(), file.to_string_lossy().into(), refname.clone()];
    args.extend(exclude.iter().filter(|e| valid_sha(e) && has_commit(root, e)).map(|e| format!("^{e}")));
    if exclude.is_empty() {
        args.extend(["--not".into(), "--remotes".into()]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = git(root, &args);
    let _ = git(root, &["update-ref", "-d", &refname]);
    let out = match res {
        Ok(_) => std::fs::read(&file).map(Some),
        Err(e) if e.to_string().contains("empty bundle") => Ok(None),
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_file(&file);
    out
}

/// Brings the commits of a bundle into the repo. Missing prerequisites: try `origin` once.
pub fn fetch_bundle(root: &Path, bundle: &[u8], tmp_dir: &Path) -> io::Result<()> {
    let file = tmp_dir.join(format!("in-{}.bundle", hex(&random::<8>())));
    std::fs::write(&file, bundle)?;
    let f = file.to_string_lossy().to_string();
    let res = (|| {
        if git(root, &["bundle", "verify", "-q", &f]).is_err() {
            let _ = git(root, &["fetch", "-q", "origin"]);
            git(root, &["bundle", "verify", "-q", &f])?;
        }
        let heads = git(root, &["bundle", "list-heads", &f])?;
        let head = heads.lines().next().and_then(|l| l.split_once(' ')).map(|(_, r)| r.to_string());
        let head = head.ok_or_else(|| io::Error::other("empty bundle"))?;
        git(root, &["fetch", "-q", &f, &format!("+{head}:refs/hilo/in")])
    })();
    let _ = std::fs::remove_file(&file);
    res.map(|_| ())
}

/// Every branch and tag, for a machine that doesn't have the repo yet. None if it's too big to send.
pub fn bundle_all(root: &Path, tmp_dir: &Path, max: usize) -> Option<Vec<u8>> {
    let file = tmp_dir.join(format!("all-{}.bundle", hex(&random::<8>())));
    let f = file.to_string_lossy().to_string();
    let ok = git(root, &["bundle", "create", "-q", &f, "--branches", "--tags"]).is_ok();
    let data = ok.then(|| std::fs::read(&file).ok()).flatten();
    let _ = std::fs::remove_file(&file);
    data.filter(|d| d.len() <= max)
}

/// Clones from a bundle (or, without one, from `origin`) into `target`, then points `origin` where
/// the source machine has it, so both copies are recognized as the same repo.
pub fn clone_into(target: &Path, branch: &str, origin: &str, bundle: Option<&[u8]>, tmp_dir: &Path) -> io::Result<()> {
    let parent = target.parent().ok_or_else(|| io::Error::other("no parent"))?;
    let t = target.to_string_lossy().to_string();
    match bundle {
        Some(b) => {
            let file = tmp_dir.join(format!("clone-{}.bundle", hex(&random::<8>())));
            std::fs::write(&file, b)?;
            let r = git(parent, &["clone", "-q", "-b", branch, "--", &file.to_string_lossy(), &t]);
            let _ = std::fs::remove_file(&file);
            r?;
        }
        None if !origin.is_empty() => {
            git(parent, &["clone", "-q", "-b", branch, "--", origin, &t])?;
        }
        None => return Err(io::Error::other("too big to send and no origin to clone from")),
    }
    if origin.is_empty() {
        git(target, &["remote", "remove", "origin"])?;
    } else {
        git(target, &["remote", "set-url", "origin", origin])?;
    }
    Ok(())
}

pub fn origin_url(root: &Path) -> String {
    git(root, &["config", "--get", "remote.origin.url"]).unwrap_or_default()
}

/// Moves the checked-out branch of the worktree at `root` to `sha`, leaving files alone.
pub fn reset_mixed(root: &Path, sha: &str) -> io::Result<()> {
    git(root, &["reset", "-q", "--mixed", sha]).map(|_| ())
}

pub fn save_conflict_commit(root: &Path, branch: &str, sha: &str) -> io::Result<String> {
    conflict_room(root)?;
    let name = format!("hilo/conflict/{branch}/{}", crate::stamp(true));
    git(root, &["update-ref", &format!("refs/heads/{name}"), sha, ""])?;
    Ok(name)
}

/// Creates a worktree for `branch` at `path` (creating the branch at `sha` if it doesn't exist).
pub fn worktree_add(root: &Path, path: &Path, branch: &str, sha: &str) -> io::Result<()> {
    let p = path.to_string_lossy();
    if git(root, &["show-ref", "--verify", "-q", &format!("refs/heads/{branch}")]).is_ok() {
        git(root, &["worktree", "add", "-q", &p, branch]).map(|_| ())
    } else {
        git(root, &["worktree", "add", "-q", "-b", branch, &p, sha]).map(|_| ())
    }
}

/// Is this exact content already committed at HEAD:path? Then overwriting it loses nothing.
pub fn in_head(root: &Path, path: &str, data: Option<&[u8]>) -> bool {
    let head = git(root, &["rev-parse", "-q", "--verify", &format!("HEAD:{path}")]).ok();
    match data {
        None => head.is_none(),
        Some(d) => {
            let mut c = cmd(root);
            c.args(["hash-object", "--stdin"]);
            let blob = run(c, Some(d)).map(|o| String::from_utf8_lossy(&o).trim().to_string()).ok();
            head.is_some() && blob == head
        }
    }
}

/// Unified diff between two files, for `hilo approve`.
pub fn diff_files(a: &Path, b: &Path) -> String {
    let out = Command::new("git").args(["diff", "--no-index", "--no-color", "--"]).arg(a).arg(b).output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

pub fn conflict_branches(root: &Path) -> usize {
    git(root, &["for-each-ref", "--format=%(refname)", "refs/heads/hilo/conflict/"])
        .map_or(0, |s| s.lines().filter(|l| !l.is_empty()).count())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn temp_repo() -> PathBuf {
        let d = std::env::temp_dir().join(format!("hilo-git-{}", hex(&random::<4>())));
        std::fs::create_dir_all(&d).unwrap();
        for a in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@t"], &["config", "user.name", "t"]] {
            git(&d, a).unwrap();
        }
        std::fs::write(d.join(".gitignore"), "node_modules/\n*.log\n!keep.log\n").unwrap();
        std::fs::write(d.join("a.txt"), "one").unwrap();
        git(&d, &["add", "."]).unwrap();
        git(&d, &["commit", "-qm", "init"]).unwrap();
        d
    }

    #[test]
    fn commits_travel_by_bundle() {
        let a = temp_repo();
        let b = std::env::temp_dir().join(format!("hilo-git-{}", hex(&random::<4>())));
        git(&std::env::temp_dir(), &["clone", "-q", &a.to_string_lossy(), &b.to_string_lossy()]).unwrap();
        let tmp = std::env::temp_dir();
        let base = read_ref(&common_dir(&a).unwrap(), "main").unwrap();
        std::fs::write(a.join("a.txt"), "two").unwrap();
        git(&a, &["commit", "-qam", "two"]).unwrap();
        let tip = read_ref(&common_dir(&a).unwrap(), "main").unwrap();
        assert!(is_ancestor(&a, &base, &tip) && !is_ancestor(&a, &tip, &base));
        let bun = bundle(&a, &tip, &[base.clone()], &tmp).unwrap().unwrap();
        assert!(bundle(&a, &base, &[tip.clone()], &tmp).unwrap().is_none()); // nothing missing
        std::fs::write(b.join("a.txt"), "two").unwrap(); // files arrive before the commit
        fetch_bundle(&b, &bun, &tmp).unwrap();
        reset_mixed(&b, &tip).unwrap();
        assert_eq!(git(&b, &["status", "--porcelain"]).unwrap(), "");
        assert_eq!(read_ref(&common_dir(&b).unwrap(), "main").unwrap(), tip);
        let wt = b.with_extension("wt");
        worktree_add(&b, &wt, "feature", &tip).unwrap();
        let wts = worktrees(&b).unwrap();
        assert!(wts.iter().any(|(_, br)| br.as_deref() == Some("feature")));
        assert!(in_head(&b, "a.txt", Some(b"two")) && !in_head(&b, "a.txt", Some(b"three")));
        assert!(valid_branch("feat/x") && !valid_branch("a..b") && !valid_branch("-x"));
        for d in [a, b, wt] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn urls() {
        for u in [
            "git@GitHub.com:a/b.git",
            "https://u@github.com/A/b/",
            "ssh://git@github.com/a/b.git",
            "http://local_proxy@127.0.0.1:41637/git/a/b",
            "https://x-access-token:abc@github.com/a/b.git",
        ] {
            assert_eq!(normalize_url(u), "a/b", "{u}");
        }
        assert_eq!(normalize_url(""), "");
    }

    #[test]
    fn ignore_conflict_branch() {
        let d = temp_repo();
        let mut ig = Ignore::new(&d).unwrap();
        assert!(ig.ignored("node_modules/").unwrap());
        assert!(ig.ignored("x.log").unwrap());
        assert!(!ig.ignored("keep.log").unwrap());
        assert!(!ig.ignored("src/").unwrap());
        assert!(!ig.ignored("a.txt").unwrap());
        assert_eq!(branch(&gitdir(&d).unwrap()).as_deref(), Some("main"));
        let tmp = std::env::temp_dir();
        let r = save_conflict(&d, "main", "dir/new file.txt", Some(b"mine"), &tmp).unwrap();
        let shown = git(&d, &["show", &format!("{r}:dir/new file.txt")]).unwrap();
        assert_eq!(shown, "mine");
        save_conflict(&d, "main", "a.txt", None, &tmp).unwrap();
        assert_eq!(conflict_branches(&d), 2);
        assert_eq!(git(&d, &["status", "--porcelain"]).unwrap(), ""); // worktree untouched
        std::fs::remove_dir_all(&d).unwrap();
    }
}
