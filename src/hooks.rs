//! `hilo hook pre|post|stop`, run by Claude Code with the event as JSON on stdin.
//! Exit 2 + stderr = Claude Code blocks the tool (pre) or shows the message to the agent.
//! If the daemon is down, hooks fail open: the anchor's ordering rule still protects every write.
#![forbid(unsafe_code)]
use crate::fsops::is_env_name;
use crate::ipc;
use crate::json::Json;
use std::io::Read;
use std::time::Duration;

const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
const ENV_MSG: &str = "hilo: .env files are managed by hilo and hidden from agents. The project already sees the variables when it runs. To change one, use `hilo env set KEY=value` (values are never printed).";
const HILO_MSG: &str = "hilo: ~/.hilo holds this machine's keys and the env vault; agents can't read it.";

/// `.env` named in a shell command (not `.envrc`, `.env.example`, `process.env`, `dotenv`).
pub fn mentions_env(cmd: &str) -> bool {
    let b = cmd.as_bytes();
    let mut i = 0;
    while let Some(off) = cmd[i..].find(".env") {
        let at = i + off;
        let before_ok = at == 0 || b" \t/'\"=<>(:`$;|&".contains(&b[at - 1]);
        let rest = &cmd[at + 4..];
        let after_ok = rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace() || "'\";|&)*>".contains(c)) || (rest.starts_with('.') && !rest.starts_with(".example"));
        if before_ok && after_ok {
            return true;
        }
        i = at + 4;
    }
    false
}

/// The hilo home as a whole path component (so a `-backup` sibling or `x.hilo.json` don't count).
fn touches_hilo_home(s: &str) -> bool {
    s.match_indices(".hilo").any(|(i, _)| {
        let before = s[..i].chars().next_back();
        let after = s[i + 5..].chars().next();
        before == Some('/') && after.is_none_or(|c| c == '/' || c.is_whitespace() || "'\";|&)".contains(c))
    })
}

/// Reads of secrets, decided in-process (no socket, no network). Only the paths a tool opens and
/// shell commands count: the text an agent writes into a file is not an access.
pub fn sensitive(tool: &str, input: &Json) -> Option<&'static str> {
    let field = |k: &str| input.get(k).and_then(Json::str);
    let mut looked: Vec<&str> = ["file_path", "notebook_path", "path", "glob"].iter().filter_map(|k| field(k)).collect();
    if tool == "Bash" {
        looked.extend(field("command"));
    }
    if looked.iter().any(|s| touches_hilo_home(s)) {
        return Some(HILO_MSG);
    }
    let name_of = |k: &str| field(k).map(|p| p.rsplit('/').next().unwrap_or(p).to_string());
    let env = match tool {
        "Read" => name_of("file_path").is_some_and(|n| is_env_name(&n)),
        "Grep" => name_of("path").is_some_and(|n| is_env_name(&n)) || field("glob").is_some_and(|g| g.contains(".env") && !g.contains(".env.example")),
        "Bash" => field("command").is_some_and(mentions_env),
        _ => false,
    };
    env.then_some(ENV_MSG)
}

pub fn run(kind: &str) -> i32 {
    let mut raw = String::new();
    let _ = std::io::stdin().take(16 << 20).read_to_string(&mut raw);
    let Some(ev) = Json::parse(&raw) else { return 0 };
    let tool = ev.get("tool_name").and_then(Json::str).unwrap_or("");
    let input = ev.get("tool_input").cloned().unwrap_or(Json::Null);
    let session = ev.get("session_id").and_then(Json::str).unwrap_or("unknown").to_string();
    let path = input.get("file_path").or_else(|| input.get("notebook_path")).and_then(Json::str).map(str::to_string);
    let call = |a: &[&str]| ipc::call(&ipc::args(a), Duration::from_secs(2)).ok();
    let (code, msg) = match kind {
        "pre" => {
            if let Some(m) = sensitive(tool, &input) {
                (2, m.to_string())
            } else if let (true, Some(p)) = (EDIT_TOOLS.contains(&tool), &path) {
                call(&["claim", &session, p]).unwrap_or((0, String::new()))
            } else {
                (0, String::new())
            }
        }
        "post" => match (EDIT_TOOLS.contains(&tool), &path) {
            (true, Some(p)) => call(&["touch", &session, p]).unwrap_or((0, String::new())),
            _ => (0, String::new()),
        },
        "stop" => {
            let r = call(&["release", &session]).unwrap_or((0, String::new()));
            // don't keep an agent alive in a loop: only interrupt a first stop
            if ev.get("stop_hook_active") == Some(&Json::Bool(true)) { (0, String::new()) } else { r }
        }
        _ => (0, String::new()),
    };
    if code == 2 {
        eprintln!("{msg}");
        return 2;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn env_detection() {
        for yes in ["cat .env", "source ./.env.local", "grep KEY apps/web/.env", "cp .env /tmp", "cat '.env'", "less ~/.hilo/key", "ls ~/.hilo", "cat $HOME/.hilo/config"] {
            assert!(mentions_env(yes) || touches_hilo_home(yes), "{yes}");
        }
        for no in ["cat .envrc", "cat .env.example", "node -e 'process.env.X'", "npm i dotenv", "ls", "hilo env set A=1", "mkdir ~/.hilo-incoming", "cat a/x.hilo.json"] {
            assert!(!mentions_env(no) && !touches_hilo_home(no), "{no}");
        }
        let read = Json::parse(r#"{"file_path":"/x/apps/.env.production"}"#).unwrap();
        assert!(sensitive("Read", &read).is_some());
        let ok = Json::parse(r#"{"file_path":"/x/.env.example"}"#).unwrap();
        assert!(sensitive("Read", &ok).is_none());
        let home = Json::parse(r#"{"pattern":"x","path":"/Users/a/.hilo"}"#).unwrap();
        assert!(sensitive("Grep", &home).is_some());
        let write = Json::parse(r#"{"file_path":"/x/notes.md","content":"see ~/.hilo/config"}"#).unwrap();
        assert!(sensitive("Write", &write).is_none()); // writing about it is not reading it
    }
}
