//! What `hilo init` / `hilo join` touch outside ~/.hilo, and how `hilo uninstall` puts it back.
//! Every edit is marked so it can be found and removed without touching anything else.
use crate::json::Json;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
const LABEL: &str = "dev.hilo.daemon";

fn home() -> PathBuf {
    crate::config::user_home().unwrap_or_default()
}

fn sh_quote(p: &Path) -> String {
    if cfg!(windows) {
        // hooks may run under cmd.exe or Git Bash: double quotes mean the same in both, and Windows
        // paths hold no `"`
        return format!("\"{}\"", p.display());
    }
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

fn is_ours(cmd: &str) -> bool {
    cmd.contains("hilo") && (cmd.ends_with(" hook pre") || cmd.ends_with(" hook post") || cmd.ends_with(" hook stop"))
}

const DENY: [&str; 2] = ["Read(~/.hilo/**)", "Edit(~/.hilo/**)"];

fn strip_claude(j: &mut Json) {
    if let Some(Json::Obj(events)) = match j {
        Json::Obj(_) => Some(j.entry("hooks", Json::Obj(vec![]))),
        _ => None,
    } {
        for (_, arr) in events.iter_mut() {
            if let Json::Arr(groups) = arr {
                groups.retain(|g| !g.get("hooks").is_some_and(|h| h.arr().iter().any(|x| x.get("command").and_then(Json::str).is_some_and(is_ours))));
            }
        }
        events.retain(|(_, a)| !matches!(a, Json::Arr(v) if v.is_empty()));
    }
    if let Some(Json::Obj(_)) = j.get("permissions") {
        let p = j.entry("permissions", Json::Obj(vec![]));
        if let Some(Json::Arr(d)) = match p {
            Json::Obj(_) => Some(p.entry("deny", Json::Arr(vec![]))),
            _ => None,
        } {
            d.retain(|x| !x.str().is_some_and(|s| DENY.contains(&s)));
        }
    }
    if matches!(j.get("hooks"), Some(Json::Obj(v)) if v.is_empty()) {
        j.remove("hooks");
    }
}

/// Hooks in ~/.claude/settings.json (T3 Code loads the same file).
fn claude(bin: &Path, add: bool) -> Option<String> {
    let dir = home().join(".claude");
    if !dir.is_dir() {
        return None;
    }
    let p = dir.join("settings.json");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|_| "{}".into());
    let Some(mut j @ Json::Obj(_)) = Json::parse(&text) else {
        return Some(format!("skipped {}: could not parse it", p.display()));
    };
    let _ = std::fs::write(dir.join("settings.json.hilo-bak"), &text);
    strip_claude(&mut j);
    if add {
        let b = sh_quote(bin);
        let hooks = j.entry("hooks", Json::Obj(vec![]));
        for (event, matcher, cmd) in [
            ("PreToolUse", Some("Edit|Write|MultiEdit|NotebookEdit|Read|Grep|Bash"), "pre"),
            ("PostToolUse", Some("Edit|Write|MultiEdit|NotebookEdit"), "post"),
            ("Stop", None, "stop"),
        ] {
            let mut g = vec![];
            if let Some(m) = matcher {
                g.push(("matcher", Json::s(m)));
            }
            g.push(("hooks", Json::Arr(vec![Json::obj(vec![("type", Json::s("command")), ("command", Json::s(&format!("{b} hook {cmd}"))), ("timeout", Json::n(5))])])));
            if let Json::Arr(a) = hooks.entry(event, Json::Arr(vec![])) {
                a.push(Json::obj(g));
            }
        }
        let perms = j.entry("permissions", Json::Obj(vec![]));
        if let Json::Arr(d) = perms.entry("deny", Json::Arr(vec![])) {
            d.extend(DENY.iter().map(|s| Json::s(s)));
        }
    }
    std::fs::write(&p, j.pretty()).ok()?;
    Some(format!("{} hooks in {}", if add { "added" } else { "removed" }, p.display()))
}

/// Replaces (or removes) the text between our markers.
fn marked_block(p: &Path, begin: &str, end: &str, body: Option<&str>) -> bool {
    let text = std::fs::read_to_string(p).unwrap_or_default();
    let mut out = String::new();
    let mut skip = false;
    for l in text.lines() {
        if l.trim() == begin {
            skip = true;
        } else if l.trim() == end && skip {
            skip = false;
        } else if !skip {
            out.push_str(l);
            out.push('\n');
        }
    }
    if let Some(b) = body {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&format!("{begin}\n{b}\n{end}\n"));
    }
    out != text && std::fs::write(p, out).is_ok()
}

fn codex(bin: &Path, add: bool) -> Vec<String> {
    let dir = home().join(".codex");
    if !dir.is_dir() {
        return vec![];
    }
    let toml_bin = bin.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let cfg = add.then(|| format!("[mcp_servers.hilo]\ncommand = \"{toml_bin}\"\nargs = [\"mcp\"]"));
    let agents = add.then_some("Before editing a file, call the hilo `claim` tool with its path. If another agent holds it, work on something else or wait. Call `release` when you are done with it.");
    let mut v = vec![];
    if marked_block(&dir.join("config.toml"), "# >>> hilo", "# <<< hilo", cfg.as_deref()) {
        v.push(format!("{} MCP server in {}", if add { "added" } else { "removed" }, dir.join("config.toml").display()));
    }
    if marked_block(&dir.join("AGENTS.md"), "<!-- hilo -->", "<!-- /hilo -->", agents) {
        v.push(format!("{} claim instructions in {}", if add { "added" } else { "removed" }, dir.join("AGENTS.md").display()));
    }
    v
}

/// JSON MCP configs: Cursor (`mcpServers`) and OpenCode (`mcp`).
fn json_mcp(path: PathBuf, key: &str, entry: Json, add: bool) -> Option<String> {
    if !path.parent()?.is_dir() {
        return None;
    }
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
    let mut j @ Json::Obj(_) = Json::parse(&text)? else { return None };
    let servers = j.entry(key, Json::Obj(vec![]));
    if !matches!(servers, Json::Obj(_)) {
        return None;
    }
    servers.remove("hilo");
    if add {
        servers.entry("hilo", entry);
    }
    std::fs::write(&path, j.pretty()).ok()?;
    Some(format!("{} MCP server in {}", if add { "added" } else { "removed" }, path.display()))
}

fn mcp_clients(bin: &Path, add: bool) -> Vec<String> {
    let b = bin.to_string_lossy().to_string();
    [
        json_mcp(home().join(".cursor/mcp.json"), "mcpServers", Json::obj(vec![("command", Json::s(&b)), ("args", Json::Arr(vec![Json::s("mcp")]))]), add),
        json_mcp(
            home().join(".config/opencode/opencode.json"),
            "mcp",
            Json::obj(vec![("type", Json::s("local")), ("command", Json::Arr(vec![Json::s(&b), Json::s("mcp")])), ("enabled", Json::Bool(true))]),
            add,
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(unix)]
extern "C" {
    fn getuid() -> u32;
}

#[cfg(unix)]
fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn run(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

#[cfg(unix)]
fn service(bin: &Path, add: bool) -> Option<String> {
    // carry hilo's own settings into the service (tests run with an isolated home and no Keychain)
    let envs: Vec<(String, String)> = ["HILO_HOME", "HILO_KEYCHAIN", "HILO_NOTIFY"].iter().filter_map(|k| Some((k.to_string(), std::env::var(k).ok()?))).collect();
    if cfg!(target_os = "macos") {
        let p = home().join(format!("Library/LaunchAgents/{LABEL}.plist"));
        let domain = format!("gui/{}", unsafe { getuid() });
        // The label is per user, not per HOME: only touch the loaded job if it is ours (our plist exists).
        if p.exists() {
            run("launchctl", &["bootout", &format!("{domain}/{LABEL}")]);
        }
        if !add {
            return std::fs::remove_file(&p).ok().map(|_| format!("removed {}", p.display()));
        }
        let env: String = envs.iter().map(|(k, v)| format!("<key>{k}</key><string>{}</string>", xml(v))).collect();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>{}</string>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>{env}</dict>
</dict></plist>
"#,
            xml(&bin.to_string_lossy()),
            // outside the secrets dir: the log holds no secrets and agents may read it
            xml(&home().join("Library/Logs/hilo.log").to_string_lossy())
        );
        std::fs::create_dir_all(p.parent()?).ok()?;
        std::fs::write(&p, plist).ok()?;
        let ok = run("launchctl", &["bootstrap", &domain, &p.to_string_lossy()]);
        Some(if ok { format!("launchd service {} (starts at login)", p.display()) } else { format!("wrote {} but launchctl refused it; run `hilo daemon` yourself", p.display()) })
    } else {
        let p = home().join(".config/systemd/user/hilo.service");
        if !add {
            if !p.exists() {
                return None;
            }
            run("systemctl", &["--user", "disable", "--now", "hilo.service"]);
            return std::fs::remove_file(&p).ok().map(|_| format!("removed {}", p.display()));
        }
        let env: String = envs.iter().map(|(k, v)| format!("Environment={k}={v}\n")).collect();
        let unit = format!(
            "[Unit]\nDescription=hilo: same code on all your machines\nAfter=network-online.target\n\n[Service]\nExecStart={} daemon\nRestart=always\nRestartSec=2\n{env}\n[Install]\nWantedBy=default.target\n",
            sh_quote(bin)
        );
        std::fs::create_dir_all(p.parent()?).ok()?;
        std::fs::write(&p, unit).ok()?;
        let ok = run("systemctl", &["--user", "daemon-reload"]) && run("systemctl", &["--user", "enable", "--now", "hilo.service"]);
        let user = std::env::var("USER").unwrap_or_default();
        let linger = run("loginctl", &["enable-linger", &user]);
        Some(match (ok, linger) {
            (true, true) => format!("systemd user service {} (runs without a login session)", p.display()),
            (true, false) => format!("systemd user service {} (enable lingering to keep it running after logout: `sudo loginctl enable-linger {user}`)", p.display()),
            _ => format!("wrote {} but systemctl failed; run `hilo daemon` yourself", p.display()),
        })
    }
}

/// Windows: a per-user Run key, which needs no admin (an ONLOGON scheduled task does). The daemon
/// lets go of the console window it gets there (see `leave_own_console`).
// ponytail: no restart if the daemon dies (launchd KeepAlive, systemd Restart=always), and HILO_HOME and
// friends aren't carried (set them as user environment variables); a Scheduled Task XML could do both.
#[cfg(windows)]
fn service(bin: &Path, add: bool) -> Option<String> {
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    if !add {
        if !run("reg", &["delete", KEY, "/v", "hilo", "/f"]) {
            return None;
        }
        // stop the running daemon too (like `systemctl disable --now`), or ~/.hilo stays locked
        let me = format!("PID ne {}", std::process::id());
        run("taskkill", &["/F", "/IM", "hilo.exe", "/FI", &me]);
        return Some(format!(r"removed {KEY}\hilo"));
    }
    let cmd = format!("\"{}\" daemon", bin.display());
    if !run("reg", &["add", KEY, "/v", "hilo", "/t", "REG_SZ", "/d", &cmd, "/f"]) {
        return Some("could not write the Run key; run `hilo daemon` yourself".into());
    }
    let log = crate::config::path("daemon.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log).ok();
    let mut c = Command::new(bin);
    c.arg("daemon").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log.map_or(std::process::Stdio::null(), Into::into));
    let started = crate::detach(&mut c).spawn().is_ok();
    Some(if started { format!(r"{KEY}\hilo (starts at logon)") } else { format!(r"wrote {KEY}\hilo but could not start it; run `hilo daemon` yourself") })
}

pub fn install(bin: &Path, with_service: bool) -> Vec<String> {
    let mut v: Vec<String> = vec![];
    v.extend(claude(bin, true));
    v.extend(codex(bin, true));
    v.extend(mcp_clients(bin, true));
    // HILO_NO_SERVICE=1: tests drive the wizard without touching the real launchd/systemd job
    if with_service && std::env::var("HILO_NO_SERVICE").as_deref() != Ok("1") {
        v.extend(service(bin, true));
    }
    v
}

pub fn uninstall(bin: &Path) -> Vec<String> {
    let mut v: Vec<String> = vec![];
    v.extend(service(bin, false));
    v.extend(claude(bin, false));
    v.extend(codex(bin, false));
    v.extend(mcp_clients(bin, false));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claude_settings_roundtrip() {
        let src = r#"{"model":"x","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"other"}]}]},"permissions":{"deny":["Bash(rm:*)"]}}"#;
        let mut j = Json::parse(src).unwrap();
        // add
        let hooks = j.entry("hooks", Json::Obj(vec![]));
        if let Json::Arr(a) = hooks.entry("PreToolUse", Json::Arr(vec![])) {
            a.push(Json::parse(r#"{"matcher":"Edit","hooks":[{"type":"command","command":"'/bin/hilo' hook pre"}]}"#).unwrap());
        }
        if let Json::Arr(d) = j.entry("permissions", Json::Obj(vec![])).entry("deny", Json::Arr(vec![])) {
            d.push(Json::s(DENY[0]));
        }
        strip_claude(&mut j);
        assert_eq!(j, Json::parse(src).unwrap());
    }
}
