//! `hilo mcp`: leases (claim / release / who) and file tools (read / write / list / search) over MCP
//! (JSON-RPC on stdio), for agents without hooks and sandboxes that only see synced files.
//! Leases are cooperative: an agent that ignores them is still protected by the anchor's ordering rule.
#![forbid(unsafe_code)]
use crate::config::Config;
use crate::fsops::{self, is_dotgit, is_secret, TMP_PREFIX};
use crate::grep::is_binary;
use crate::ipc;
use crate::json::Json;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const NOT_RUNNING: &str = "hilo daemon is not running";
const MAX_READ: usize = 256 << 10;
const MAX_ENTRIES: usize = 1000;
const INSTRUCTIONS: &str = "Files here are synced live between machines by hilo. Before editing a file, call claim with its path; if another agent holds it, work on something else, and call release when done. read, write, list and search work on synced files (paths absolute or relative to the working directory); write claims the file for you and syncs it at once.";

type Call<'a> = &'a dyn Fn(&[&str]) -> io::Result<(u8, String)>;

/// A tool with its arguments as (name, JSON type, description); names ending in `?` are optional.
fn tool(name: &str, desc: &str, args: &[(&str, &str, &str)]) -> Json {
    let props = args.iter().map(|(n, t, d)| (n.trim_end_matches('?'), Json::obj(vec![("type", Json::s(t)), ("description", Json::s(d))]))).collect();
    let required = args.iter().filter(|a| !a.0.ends_with('?')).map(|a| Json::s(a.0)).collect();
    Json::obj(vec![
        ("name", Json::s(name)),
        ("description", Json::s(desc)),
        ("inputSchema", Json::obj(vec![("type", Json::s("object")), ("properties", Json::obj(props)), ("required", Json::Arr(required))])),
    ])
}

fn tools() -> Json {
    const PATH: (&str, &str, &str) = ("path", "string", "File path, absolute or relative to the working directory");
    const DIR: (&str, &str, &str) = ("path?", "string", "Folder, absolute or relative to the working directory (default: the working directory)");
    Json::Arr(vec![
        tool("claim", "Take a file before editing it, so agents on other machines know. Says who holds it if it is taken.", &[PATH]),
        tool("release", "Let go of a file you claimed.", &[PATH]),
        tool("who", "Who is editing this file right now, if anyone.", &[PATH]),
        tool(
            "read",
            "Read a synced text file. Binary files answer with their size only. Long files are cut at 256 KB: use offset and limit.",
            &[PATH, ("offset?", "integer", "First line to read, from 1"), ("limit?", "integer", "How many lines to read")],
        ),
        tool(
            "write",
            "Write a whole file (created if missing). Claims it first, writes it atomically and syncs it to the other machines at once. Refused if another agent holds it.",
            &[PATH, ("content", "string", "The new content of the whole file")],
        ),
        tool("list", "Files and folders in a synced folder, with sizes.", &[DIR, ("depth?", "integer", "How many levels down (default 1, at most 4)")]),
        tool(
            "search",
            "Literal text search over the synced files in a folder (ignored files are never searched). Answers path:line: text, paths relative to the folder, at most 200 results.",
            &[("pattern", "string", "Text to find (not a regex)"), DIR, ("ignore_case?", "boolean", "Match regardless of ASCII case")],
        ),
    ])
}

fn abs(p: &str) -> String {
    let path = Path::new(p);
    if path.is_absolute() {
        p.to_string()
    } else {
        std::env::current_dir().map(|d| d.join(path).to_string_lossy().into_owned()).unwrap_or_else(|_| p.into())
    }
}

/// Only a failed connection means the daemon is down; a slow answer is a timeout.
fn failed(e: io::Error, what: &str) -> String {
    match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => NOT_RUNNING.into(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => format!("{what} timed out"),
        _ => format!("{what}: {e}"),
    }
}

/// (unit root, path inside it) through the daemon; anything outside a synced unit is refused.
fn locate(call: Call, p: &str) -> Result<(PathBuf, String), String> {
    match call(&["locate", p]) {
        Err(e) => Err(failed(e, "hilo daemon")),
        Ok((0, t)) => match t.split('\0').collect::<Vec<_>>()[..] {
            [_, root, rel] => Ok((PathBuf::from(root), rel.to_string())),
            _ => Err("bad answer from the daemon".into()),
        },
        Ok((_, t)) => Err(t),
    }
}

fn num(args: &Json, k: &str) -> Option<usize> {
    match args.get(k)? {
        Json::Num(n) | Json::Str(n) => n.parse().ok(),
        _ => None,
    }
}

/// Text of a file, lines `offset..offset+limit` (from 1), cut at MAX_READ bytes.
fn render(data: &[u8], offset: usize, limit: Option<usize>) -> String {
    if is_binary(data) {
        return format!("binary file, {} bytes", data.len());
    }
    let text = String::from_utf8_lossy(data);
    let total = text.lines().count();
    let first = offset.max(1);
    let mut out = String::new();
    let mut last = first - 1;
    for line in text.split_inclusive('\n').skip(first - 1).take(limit.unwrap_or(usize::MAX)) {
        if out.is_empty() && line.len() > MAX_READ {
            let mut n = MAX_READ;
            while !line.is_char_boundary(n) {
                n -= 1;
            }
            return format!("{}\n[line {first} cut at 256 KB of its {} bytes]", &line[..n], line.len());
        }
        if out.len() + line.len() > MAX_READ {
            out += &format!("\n[cut at 256 KB after line {last} of {total}; read on with offset {}]", last + 1);
            return out;
        }
        out += line;
        last += 1;
    }
    if first > 1 || last < total {
        out += &format!("{}[lines {first}-{last} of {total}]", if out.ends_with('\n') || out.is_empty() { "" } else { "\n" });
    }
    out
}

/// `name/` for folders, `name  N bytes` for files, `name@` for links (not followed).
fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<String>) {
    let mut es: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().collect();
    es.sort_by_key(|e| e.file_name());
    for e in es {
        if out.len() >= MAX_ENTRIES {
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if is_dotgit(&name) || name.starts_with(TMP_PREFIX) {
            continue;
        }
        let rel = format!("{prefix}{name}");
        match std::fs::symlink_metadata(e.path()) {
            Ok(m) if m.is_dir() => {
                out.push(format!("{rel}/"));
                if depth > 1 {
                    walk(&e.path(), &format!("{rel}/"), depth - 1, out);
                }
            }
            Ok(m) if m.is_file() => out.push(format!("{rel}  {} bytes", m.len())),
            Ok(_) => out.push(format!("{rel}@")),
            Err(_) => {}
        }
    }
}

/// One `tools/call`: (is error, text). `call` talks to the daemon (a fake in tests).
fn call_tool(name: &str, args: &Json, session: &str, call: Call) -> (bool, String) {
    let arg = |k: &str| args.get(k).and_then(Json::str);
    let path = abs(arg("path").unwrap_or("."));
    let r: Result<String, String> = match name {
        "claim" | "release" | "who" => {
            let r = match name {
                "claim" => call(&["claim", session, &path]).map(|(c, t)| if c == 0 { format!("claimed {path}") } else { t }),
                "release" => call(&["release", session, &path]).map(|_| format!("released {path}")),
                _ => call(&["who", &path]).map(|(_, t)| t),
            };
            return (false, r.unwrap_or_else(|_| format!("{NOT_RUNNING}; go ahead")));
        }
        "read" => locate(call, &path).and_then(|(root, rel)| {
            if rel.is_empty() || root.join(&rel).is_dir() {
                return Err(format!("{path} is a folder: use list"));
            }
            if is_secret(&rel) {
                return Err(format!("{rel}: secret files are not readable here (hilo keeps .env values in its vault)"));
            }
            let p = root.join(&rel);
            match std::fs::metadata(&p) {
                Ok(m) if !m.is_file() => Err(format!("{rel}: not a regular file")), // a FIFO would block forever
                Ok(m) if m.len() > fsops::MAX_FILE => Err(format!("{rel}: too large ({} bytes)", m.len())),
                _ => std::fs::read(&p).map_err(|e| format!("{rel}: {e}")).map(|d| render(&d, num(args, "offset").unwrap_or(1), num(args, "limit"))),
            }
        }),
        "write" => (|| {
            let content = arg("content").ok_or("write needs content")?;
            if Config::load().get("scope_ro") == Some("1") {
                return Err("this machine joined with a read-only token: write is refused".into());
            }
            let (root, rel) = locate(call, &path)?;
            if rel.is_empty() || is_secret(&rel) {
                return Err(format!("{path}: not writable here (folders and secret files are refused; use `hilo env set` for .env)"));
            }
            let mut notes = vec![];
            match call(&["claim", session, &path]) {
                Err(e) => return Err(failed(e, "claim")),
                // the busy answer is exactly lease::busy_message; anything else with code 2 is notices
                Ok((2, t)) if t.starts_with(&format!("hilo: {rel} is being edited by another agent")) => return Err(t),
                Ok((_, t)) => notes.push(t),
            }
            fsops::write(&root, &rel, Some(content.as_bytes())).map_err(|e| format!("{rel}: {e}"))?;
            notes.push(call(&["touch", session, &path]).map(|r| r.1).unwrap_or_default());
            notes.retain(|n| !n.is_empty());
            notes.insert(0, format!("wrote {rel} ({} bytes)", content.len()));
            Ok(notes.join("\n\n"))
        })(),
        "list" => locate(call, &path).map(|(root, rel)| {
            let mut out = vec![];
            walk(&root.join(&rel), "", num(args, "depth").unwrap_or(1).clamp(1, 4), &mut out);
            if out.len() >= MAX_ENTRIES {
                out.push(format!("[stopped at {MAX_ENTRIES} entries; list a subfolder]"));
            }
            if out.is_empty() { "(empty)".into() } else { out.join("\n") }
        }),
        "search" => match arg("pattern").filter(|p| !p.is_empty()) {
            None => Err("search needs a pattern".into()),
            Some(pat) => locate(call, &path).and_then(|_| {
                let flags = if args.get("ignore_case") == Some(&Json::Bool(true)) { "i" } else { "" };
                match call(&["grep", pat, &path, flags]) {
                    Ok((0, t)) => Ok(t),
                    Ok((_, t)) => Err(t),
                    Err(e) => Err(failed(e, "search")),
                }
            }),
        },
        _ => Err(format!("unknown tool {name}")),
    };
    match r {
        Ok(t) => (false, t),
        Err(t) => (true, t),
    }
}

/// The answer to one JSON-RPC line, or None for notifications and junk.
fn handle(line: &str, session: &str, call: Call) -> Option<Json> {
    let req = Json::parse(line)?;
    let id = req.get("id").cloned()?; // notifications need no answer
    let result = match req.get("method").and_then(Json::str).unwrap_or("") {
        "initialize" => Ok(Json::obj(vec![
            ("protocolVersion", req.at(&["params", "protocolVersion"]).cloned().unwrap_or(Json::s("2024-11-05"))),
            ("capabilities", Json::obj(vec![("tools", Json::obj(vec![]))])),
            ("serverInfo", Json::obj(vec![("name", Json::s("hilo")), ("version", Json::s(env!("CARGO_PKG_VERSION")))])),
            ("instructions", Json::s(INSTRUCTIONS)),
        ])),
        "ping" => Ok(Json::obj(vec![])),
        "tools/list" => Ok(Json::obj(vec![("tools", tools())])),
        "tools/call" => {
            let name = req.at(&["params", "name"]).and_then(Json::str).unwrap_or("");
            let args = req.at(&["params", "arguments"]).cloned().unwrap_or(Json::obj(vec![]));
            let (is_err, text) = call_tool(name, &args, session, call);
            Ok(Json::obj(vec![
                ("content", Json::Arr(vec![Json::obj(vec![("type", Json::s("text")), ("text", Json::s(&text))])])),
                ("isError", Json::Bool(is_err)),
            ]))
        }
        _ => Err(Json::obj(vec![("code", Json::n(-32601)), ("message", Json::s("method not found"))])),
    };
    let mut resp = vec![("jsonrpc", Json::s("2.0")), ("id", id)];
    match result {
        Ok(r) => resp.push(("result", r)),
        Err(e) => resp.push(("error", e)),
    }
    Some(Json::obj(resp))
}

/// Random hex first: lease messages show a session's first 4 characters, as with Claude Code's UUIDs.
fn session_id() -> String {
    format!("{}-mcp-{}", crate::crypto::hex(&crate::crypto::random::<2>()), std::process::id())
}

pub fn run() -> i32 {
    let session = session_id();
    // a search reads files and can take a while on a big folder; the rest is answered at once
    let call = |a: &[&str]| ipc::call(&ipc::args(a), Duration::from_secs(if a[0] == "grep" { 60 } else { 5 }));
    let mut out = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Some(resp) = handle(&line, &session, &call) else { continue };
        if writeln!(out, "{}", resp.to_string()).and_then(|_| out.flush()).is_err() {
            break;
        }
    }
    let _ = call(&["release", &session]);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text(r: &Json) -> (bool, String) {
        let t = r.at(&["result", "content"]).unwrap().arr()[0].get("text").and_then(Json::str).unwrap().to_string();
        (r.at(&["result", "isError"]) == Some(&Json::Bool(true)), t)
    }
    fn req(tool: &str, args: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{tool}","arguments":{args}}}}}"#)
    }

    #[test]
    fn tools() {
        let down = |_: &[&str]| -> io::Result<(u8, String)> { Err(io::Error::from(io::ErrorKind::NotFound)) }; // no socket file
        let list = handle(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#, "s", &down).unwrap();
        let names: Vec<&str> = list.at(&["result", "tools"]).unwrap().arr().iter().filter_map(|t| t.get("name").and_then(Json::str)).collect();
        assert_eq!(names, ["claim", "release", "who", "read", "write", "list", "search"]);
        assert!(handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, "s", &down).is_none());
        assert_eq!(text(&handle(&req("read", r#"{"path":"/x"}"#), "s", &down).unwrap()), (true, NOT_RUNNING.into()));
        assert!(!text(&handle(&req("claim", r#"{"path":"/x"}"#), "s", &down).unwrap()).0); // leases fail open

        // a fake daemon whose only unit is a temp dir
        let root = std::env::temp_dir().join(format!("hilo-mcp-{}", crate::crypto::hex(&crate::crypto::random::<4>())));
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        std::fs::write(root.join("src/a.txt"), "1\n2\n3\n").unwrap();
        std::fs::write(root.join("img.bin"), b"\x89PNG\0\0").unwrap();
        std::fs::write(root.join(".env"), "K=secret").unwrap();
        std::fs::write(root.join(format!("{TMP_PREFIX}x")), "").unwrap();
        let r2 = root.clone();
        let fake = move |a: &[&str]| -> io::Result<(u8, String)> {
            Ok(match a {
                ["locate", p] => match Path::new(p).strip_prefix(&r2) {
                    Ok(rel) => (0, format!("drive:t\0{}\0{}", r2.display(), rel.display())),
                    Err(_) => (1, format!("{p} is not in a synced folder")),
                },
                ["claim", _, p] if p.ends_with("busy.txt") => (2, "hilo: busy.txt is being edited by another agent (m, session abcd, 3 s ago).".into()),
                ["touch", ..] => (2, "note: someone else saved x".into()),
                ["grep", pat, ..] => (0, format!("grep {pat}")),
                _ => (0, String::new()),
            })
        };
        let r = |tool: &str, args: String| text(&handle(&req(tool, &args), "s", &fake).unwrap());
        let at = |rel: &str| root.join(rel).display().to_string();
        assert_eq!(r("read", format!(r#"{{"path":"{}"}}"#, at("src/a.txt"))), (false, "1\n2\n3\n".into()));
        assert_eq!(r("read", format!(r#"{{"path":"{}","offset":2,"limit":1}}"#, at("src/a.txt"))), (false, "2\n[lines 2-2 of 3]".into()));
        assert_eq!(r("read", format!(r#"{{"path":"{}"}}"#, at("img.bin"))), (false, "binary file, 6 bytes".into()));
        assert!(r("read", format!(r#"{{"path":"{}"}}"#, at(".env"))).0);
        assert!(r("read", r#"{"path":"/etc/hosts"}"#.into()).0);
        assert!(r("write", r#"{"path":"/etc/x","content":"x"}"#.into()).0);
        assert!(r("write", format!(r#"{{"path":"{}","content":"x"}}"#, at("busy.txt"))).1.contains("another agent"));
        let w = r("write", format!(r#"{{"path":"{}","content":"hi"}}"#, at("new/dir/b.txt")));
        assert_eq!(w, (false, "wrote new/dir/b.txt (2 bytes)\n\nnote: someone else saved x".into()));
        assert_eq!(std::fs::read(root.join("new/dir/b.txt")).unwrap(), b"hi");
        let l = r("list", format!(r#"{{"path":"{}","depth":2}}"#, root.display())).1;
        assert_eq!(l, ".env  8 bytes\nimg.bin  6 bytes\nnew/\nnew/dir/\nsrc/\nsrc/a.txt  6 bytes\nsrc/deep/");
        assert_eq!(r("search", format!(r#"{{"pattern":"x","path":"{}"}}"#, root.display())), (false, "grep x".into()));
        assert!(r("search", r#"{"pattern":"x","path":"/etc"}"#.into()).0);
        // a FIFO has no end: refused, not read (this used to hang the server)
        assert!(std::process::Command::new("mkfifo").arg(root.join("pipe")).status().unwrap().success());
        assert_eq!(r("read", format!(r#"{{"path":"{}"}}"#, at("pipe"))), (true, "pipe: not a regular file".into()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn slow_daemon_is_not_a_dead_daemon() {
        let slow = |a: &[&str]| -> io::Result<(u8, String)> {
            match a[0] {
                "locate" => Ok((0, "u\0/r\0f.txt".into())),
                "grep" => Err(io::Error::from(io::ErrorKind::WouldBlock)), // what a read timeout gives
                _ => Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
            }
        };
        assert_eq!(call_tool("search", &Json::parse(r#"{"pattern":"x"}"#).unwrap(), "s", &slow), (true, "search timed out".into()));
        // a refused connection is the one case that means the daemon is down (and nothing is written)
        assert_eq!(call_tool("write", &Json::parse(r#"{"path":"/r/f.txt","content":"x"}"#).unwrap(), "s", &slow), (true, NOT_RUNNING.into()));
    }

    #[test]
    fn sessions_differ_in_what_leases_show() {
        let s = session_id();
        assert!(s[..4].chars().all(|c| c.is_ascii_hexdigit()) && s.ends_with(&std::process::id().to_string()), "{s}");
    }

    #[test]
    fn one_huge_line_still_reads() {
        let out = render("é".repeat(MAX_READ).as_bytes(), 1, None); // 2 bytes per char: one line over the cap
        assert!(out.starts_with("éé") && out.len() <= MAX_READ + 100 && out.ends_with(&format!("[line 1 cut at 256 KB of its {} bytes]", 2 * MAX_READ)), "{}", &out[out.len().saturating_sub(80)..]);
    }

    #[test]
    fn long_files_are_cut() {
        let big = "x".repeat(1000) + "\n";
        let out = render(big.repeat(300).as_bytes(), 1, None);
        assert!(out.len() < MAX_READ + 200 && out.contains("[cut at 256 KB after line 261 of 300; read on with offset 262]"), "{}", &out[out.len() - 100..]);
    }
}
