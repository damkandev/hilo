//! `hilo` with no arguments (the wizard) and `hilo setup HOST`: set up the other machine over SSH
//! and pair this one with it. The join code travels over the SSH connection, so nobody copies it.
#![forbid(unsafe_code)]
use crate::config::Config;
use crate::tui::{self, bold, choice, dim, fg, Progress, CYAN};
use crate::{config, ipc, Opts};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ---------- addresses ----------

/// Prints `ip=<addr> <iface>` for every IPv4 address, on Linux and macOS.
const IP_SCRIPT: &str = r#"ts=$(tailscale ip -4 2>/dev/null | head -1); [ -z "$ts" ] && ts=$(/Applications/Tailscale.app/Contents/MacOS/Tailscale ip -4 2>/dev/null | head -1)
[ -n "$ts" ] && echo "ip=$ts tailscale"
if command -v ip >/dev/null 2>&1; then ip -4 -o addr show | awk '{split($4,a,"/"); print "ip=" a[1] " " $2}'
else ifconfig 2>/dev/null | awk '/^[a-z]/{i=$1; sub(":","",i)} /inet /{print "ip=" $2 " " i}'; fi
"#;

/// Addresses another machine could dial: Tailscale first, then the LAN. Containers and loopback out.
fn rank(lines: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = lines
        .lines()
        .filter_map(|l| l.strip_prefix("ip="))
        .filter_map(|l| {
            let (ip, iface) = l.split_once(' ')?;
            let addr: std::net::Ipv4Addr = ip.parse().ok()?;
            let o = addr.octets();
            let ts = o[0] == 100 && (64..128).contains(&o[1]);
            let virt = ["lo", "docker", "br-", "veth", "virbr", "cni", "flannel", "vmnet", "bridge", "podman"].iter().any(|p| iface.starts_with(p));
            ((ts || addr.is_private()) && !virt).then(|| (ip.to_string(), if ts { "tailscale".to_string() } else { iface.to_string() }))
        })
        .collect();
    v.sort_by_key(|(ip, kind)| (kind != "tailscale", !ip.starts_with("192.168."), ip.clone()));
    v.dedup_by(|a, b| a.0 == b.0);
    v
}

pub fn local_ips() -> Vec<(String, String)> {
    let out = Command::new("sh").arg("-c").arg(IP_SCRIPT).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    rank(&out)
}

// ---------- ssh ----------

fn ssh_hosts() -> Vec<(String, String)> {
    let home = std::env::var("HOME").unwrap_or_default();
    let text = std::fs::read_to_string(Path::new(&home).join(".ssh/config")).unwrap_or_default();
    let mut hosts: Vec<(String, String)> = vec![];
    for l in text.lines() {
        let l = l.trim();
        let (k, v) = match l.split_once(char::is_whitespace) {
            Some((k, v)) => (k.to_lowercase(), v.trim()),
            None => continue,
        };
        if k == "host" {
            hosts.extend(v.split_whitespace().filter(|h| !h.contains(['*', '?', '!'])).map(|h| (h.to_string(), String::new())));
        } else if k == "hostname" {
            if let Some(last) = hosts.last_mut().filter(|h| h.1.is_empty()) {
                last.1 = v.to_string();
            }
        }
    }
    hosts
}

/// Runs a shell script on `host` (fed on stdin, so nothing needs quoting).
fn ssh(host: &str, script: &str) -> Result<String, String> {
    let mut c = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8", host, "sh", "-s"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ssh: {e}"))?;
    c.stdin.take().unwrap().write_all(script.as_bytes()).map_err(|e| e.to_string())?;
    let o = c.wait_with_output().map_err(|e| e.to_string())?;
    if !o.status.success() {
        let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
        return Err(if err.is_empty() { format!("{host}: command failed") } else { err });
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `~/x` on the remote side, as a shell word.
fn remote_path(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", q(rest)),
        None if p == "~" => "\"$HOME\"".into(),
        None => q(p),
    }
}

/// Extra environment for remote hilo commands (tests use it to isolate a second hilo on a real server).
fn remote_env() -> String {
    std::env::var("HILO_REMOTE_ENV").unwrap_or_default()
}

#[derive(Clone, Default)]
pub struct Remote {
    host: String,
    os: String,
    arch: String,
    pretty: String,
    hilo: Option<String>,
    info: HashMap<String, String>,
    ips: Vec<(String, String)>,
    dirs: Vec<String>,
}

impl Remote {
    fn role(&self) -> &str {
        self.info.get("role").map_or("", String::as_str)
    }
    fn hilo(&self, args: &str) -> Result<String, String> {
        let h = self.hilo.clone().ok_or("hilo is not installed there")?;
        ssh(&self.host, &format!("env {} {} {args}", remote_env(), q(&h)))
    }
}

fn probe(host: &str) -> Result<Remote, String> {
    let script = format!(
        r#"echo "os=$(uname -s)"; echo "arch=$(uname -m)"
if [ -r /etc/os-release ]; then . /etc/os-release; echo "pretty=$PRETTY_NAME"; fi
[ "$(uname -s)" = Darwin ] && echo "pretty=macOS $(sw_vers -productVersion)"
h=$(command -v hilo 2>/dev/null); [ -z "$h" ] && [ -x "$HOME/.local/bin/hilo" ] && h="$HOME/.local/bin/hilo"
if [ -n "$h" ]; then echo "hilo=$h"; env {env} "$h" info 2>/dev/null | sed 's/^/info./'; v=$("$h" --version 2>/dev/null); echo "info.version=${{v#hilo }}"; fi
{IP_SCRIPT}
ls -1 "$HOME" | sed 's/^/dir=/'
"#,
        env = remote_env()
    );
    let out = ssh(host, &script)?;
    let mut r = Remote { host: host.into(), ..Default::default() };
    for l in out.lines() {
        let Some((k, v)) = l.split_once('=') else { continue };
        match k {
            "os" => r.os = v.into(),
            "arch" => r.arch = v.into(),
            "pretty" => r.pretty = v.into(),
            "hilo" => r.hilo = Some(v.into()),
            "dir" => r.dirs.push(v.into()),
            k if k.starts_with("info.") => {
                r.info.insert(k[5..].into(), v.into());
            }
            _ => {}
        }
    }
    r.ips = rank(&out);
    if r.pretty.is_empty() {
        r.pretty = r.os.clone();
    }
    Ok(r)
}

fn triple(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("Linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("Linux", "aarch64" | "arm64") => "aarch64-unknown-linux-musl",
        ("Darwin", "arm64") => "aarch64-apple-darwin",
        ("Darwin", "x86_64") => "x86_64-apple-darwin",
        _ => return None,
    })
}

/// Same version as this one, installed in ~/.local/bin on the remote.
fn ensure_hilo(r: &mut Remote, p: &Progress) -> Result<String, String> {
    let want = env!("CARGO_PKG_VERSION");
    if r.info.get("version").map(String::as_str) == Some(want) {
        return Ok(format!("hilo {want} already on {}", r.host));
    }
    let t = triple(&r.os, &r.arch).ok_or(format!("no hilo build for {} {}", r.os, r.arch))?;
    let here = triple(std::env::consts::OS.replace("macos", "Darwin").replace("linux", "Linux").as_str(), std::env::consts::ARCH).unwrap_or("");
    let local: Option<PathBuf> = if t == here {
        Some(crate::bin())
    } else {
        std::env::var_os("HILO_DIST").map(|d| PathBuf::from(d).join(format!("hilo-{t}"))).filter(|p| p.exists())
    };
    match local {
        Some(file) => {
            p.set("copying the binary");
            ssh(&r.host, "mkdir -p \"$HOME/.local/bin\"")?;
            let st = Command::new("scp").args(["-q", "-o", "BatchMode=yes"]).arg(&file).arg(format!("{}:.local/bin/hilo.new", r.host)).status().map_err(|e| e.to_string())?;
            if !st.success() {
                return Err("scp failed".into());
            }
            ssh(&r.host, "chmod +x \"$HOME/.local/bin/hilo.new\" && mv \"$HOME/.local/bin/hilo.new\" \"$HOME/.local/bin/hilo\"")?;
        }
        None => {
            p.set("downloading the release");
            ssh(&r.host, &format!("curl -fsSL https://raw.githubusercontent.com/damkandev/hilo/main/install.sh | HILO_VERSION=v{want} sh"))
                .map_err(|e| format!("could not install hilo there ({e}). Install it on {} and run this again.", r.host))?;
        }
    }
    // a running daemon picks up the new binary
    let _ = ssh(&r.host, "systemctl --user try-restart hilo 2>/dev/null; launchctl kickstart -k gui/$(id -u)/dev.hilo.daemon 2>/dev/null; true");
    let fresh = probe(&r.host)?;
    *r = fresh;
    Ok(format!("hilo {want} installed on {}", r.host))
}

// ---------- the plan ----------

#[derive(Clone)]
pub struct Plan {
    pub host: String,
    pub local_folder: PathBuf,
    pub remote_folder: String,
    pub listen: Option<String>,
    pub service: bool,
}

fn code_in(s: &str) -> Option<String> {
    s.split_whitespace().find(|w| w.starts_with("HILO-") && w.contains('@')).map(str::to_string)
}

fn wait_port(addr: &str, secs: u64) -> bool {
    let Ok(sa) = addr.parse::<std::net::SocketAddr>() else { return false };
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if std::net::TcpStream::connect_timeout(&sa, Duration::from_secs(1)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

fn progress() -> HashMap<String, String> {
    ipc::call(&ipc::args(&["progress"]), Duration::from_secs(2))
        .map(|(_, t)| t.split(' ').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.into(), v.into())).collect())
        .unwrap_or_default()
}

fn start_local_daemon_if_needed(service: bool) {
    if !service && progress().is_empty() {
        let log = std::fs::File::create(config::path("daemon.log")).ok();
        let _ = Command::new(crate::bin()).arg("daemon").stdin(Stdio::null()).stdout(Stdio::null()).stderr(log.map_or(Stdio::null(), Stdio::from)).spawn();
    }
}

/// Everything after the questions. Each step is a spinner line.
pub fn run(plan: Plan, remote: Remote) -> Result<(), String> {
    let mut r = remote;
    let host = plan.host.clone();
    let mut rr = r.clone();
    r = tui::step(&format!("Installing hilo on {host}"), move |p| {
        let msg = ensure_hilo(&mut rr, p)?;
        Ok((rr, msg))
    })?;

    let local_role = Config::load().get("role").unwrap_or_default().to_string();
    let my_key = config::key().ok().map(|k| crate::crypto::hex(&crate::noise::public_key(&k)));
    let local_anchor_key = Config::load().get("anchor_key").unwrap_or_default().to_string();
    let rf = remote_path(&plan.remote_folder);
    let ns = if plan.service { "" } else { " --no-service" };

    // who is the anchor, and a code to pair the other side
    let (remote_is_anchor, code) = match (local_role.as_str(), r.role()) {
        ("anchor", "") => {
            let cfg = Config::load();
            let t = config::new_token().map_err(|e| e.to_string())?;
            (false, Some(config::format_code(&t, cfg.get("listen").unwrap_or_default())))
        }
        ("anchor", "member") if r.info.get("anchor_key") == my_key.as_ref() => (false, None),
        (_, "member") => return Err(format!("{host} is already paired with another anchor")),
        ("member", "") => return Err("this machine is paired with another anchor: run the wizard there".into()),
        ("member", "anchor") if r.info.get("key").map(String::as_str) != Some(local_anchor_key.as_str()) => {
            return Err(format!("this machine is paired with another anchor, not {host}"))
        }
        (_, "anchor") => {
            let (rr, rf2) = (r.clone(), rf.clone());
            let code = tui::step(&format!("Linking {} on {host}", plan.remote_folder), move |_| {
                ssh(&rr.host, &format!("mkdir -p {rf2}"))?;
                rr.hilo(&format!("add {rf2}"))?;
                let code = if local_role.is_empty() { code_in(&rr.hilo("invite")?) } else { None };
                Ok((code, format!("{} is the anchor; folder linked there", rr.host)))
            })?;
            (true, code)
        }
        _ => {
            // fresh remote: it becomes the anchor
            let ip = match &plan.listen {
                Some(l) => l.clone(),
                None => format!("{}:7710", r.ips.first().ok_or(format!("{host} has no private address to listen on"))?.0),
            };
            let (rr, rf2, ip2) = (r.clone(), rf.clone(), ip.clone());
            let code = tui::step(&format!("Starting the anchor on {host}"), move |p| {
                ssh(&rr.host, &format!("mkdir -p {rf2}"))?;
                p.set(&ip2);
                let out = rr.hilo(&format!("init --listen {ip2} --name {} --folder {rf2}{ns}", q(&rr.host)))?;
                let code = code_in(&out).ok_or("init printed no code")?;
                if ns.is_empty() {
                    Ok((code, format!("{} is the anchor at {ip2}", rr.host)))
                } else {
                    let h = rr.hilo.clone().unwrap_or_default();
                    ssh(&rr.host, &format!("nohup env {} {} daemon >/tmp/hilo-daemon.log 2>&1 </dev/null &", remote_env(), q(&h)))?;
                    Ok((code, format!("{} is the anchor at {ip2} (daemon started by hand)", rr.host)))
                }
            })?;
            let ip3 = ip.clone();
            tui::step(&format!("Checking this machine can reach {ip}"), move |_| {
                if wait_port(&ip3, 15) { Ok(((), format!("{ip3} is reachable"))) } else { Err(format!("can't reach {ip3} from here (firewall? different network?)")) }
            })?;
            (true, Some(code))
        }
    };

    // pair the side that isn't paired yet
    let folder = plan.local_folder.clone();
    let service = plan.service;
    if remote_is_anchor {
        tui::step("Pairing this machine", move |_| {
            match code {
                Some(c) => {
                    crate::join_core(&c, &Opts { folder: Some(folder.clone()), service, ..Default::default() })?;
                }
                None => {
                    let path = std::fs::canonicalize(&folder).map_err(|e| e.to_string())?;
                    if ipc::call(&ipc::args(&["add", &path.to_string_lossy()]), Duration::from_secs(30)).is_err() {
                        let mut cfg = Config::load();
                        cfg.add("folder", &path.to_string_lossy());
                        cfg.save().map_err(|e| e.to_string())?;
                    }
                }
            }
            start_local_daemon_if_needed(service);
            Ok(((), format!("this machine is paired; {} linked", tui::tilde(&folder))))
        })?;
    } else {
        let rr = r.clone();
        let rf2 = rf.clone();
        tui::step(&format!("Pairing {host} with this machine"), move |_| {
            ssh(&rr.host, &format!("mkdir -p {rf2}"))?;
            match code {
                Some(c) => {
                    rr.hilo(&format!("join {c} --name {} --folder {rf2}{ns}", q(&rr.host)))?;
                    if !ns.is_empty() {
                        let h = rr.hilo.clone().unwrap_or_default();
                        ssh(&rr.host, &format!("nohup env {} {} daemon >/tmp/hilo-daemon.log 2>&1 </dev/null &", remote_env(), q(&h)))?;
                    }
                }
                None => {
                    rr.hilo(&format!("add {rf2}"))?;
                }
            }
            let path = std::fs::canonicalize(&folder).map_err(|e| e.to_string())?;
            let _ = ipc::call(&ipc::args(&["add", &path.to_string_lossy()]), Duration::from_secs(30));
            Ok(((), format!("{} joined this machine", rr.host)))
        })?;
    }
    let mut cfg = Config::load();
    cfg.set("ssh", &host);
    let _ = cfg.save();

    // until both sides have every repo
    let rr = r.clone();
    let mine = crate::repos_in(&plan.local_folder).len();
    tui::step("Syncing", move |p| {
        let end = Instant::now() + Duration::from_secs(600);
        loop {
            let here = progress();
            let there: HashMap<String, String> = rr
                .hilo("info")
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.into(), v.into()))
                .collect();
            let pending: usize = here.get("pending").and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
            let caught = here.get("caught").map(String::as_str) == Some("1");
            let theirs: usize = there.get("repos").and_then(|v| v.parse().ok()).unwrap_or(0);
            if caught && pending == 0 && theirs >= mine {
                return Ok(((), format!("in sync: {mine} repos on both machines")));
            }
            p.set(if !caught {
                "connecting…".to_string()
            } else if pending > 0 {
                format!("sending {pending} changes")
            } else {
                format!("{} has {theirs} of {mine} repos", rr.host)
            });
            if Instant::now() > end {
                return Err("still syncing after 10 minutes; check `hilo status`".into());
            }
            std::thread::sleep(Duration::from_millis(400));
        }
    })?;

    tui::card(&[
        format!("{}  {}", bold(&tui::gradient("hilo is running")), dim("on this machine and on ") + &bold(&host)),
        String::new(),
        format!("{}  {}  {}", tui::tilde(&plan.local_folder), fg(CYAN, "⇄"), format!("{host}:{}", plan.remote_folder)),
        String::new(),
        dim("New repos in that folder appear on the other machine on their own."),
        dim("Agents get told before they collide; .env files follow along."),
        String::new(),
        format!("{}  {}", fg(CYAN, "hilo status "), dim("see what's going on")),
        format!("{}  {}", fg(CYAN, "hilo add DIR"), dim("link another folder")),
    ]);
    Ok(())
}

// ---------- entry points ----------

fn guess_folder() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    if !crate::repos_in(&cwd).is_empty() {
        return cwd;
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    ["codigo", "Codigo", "code", "Code", "projects", "Projects", "dev", "src"]
        .iter()
        .map(|d| home.join(d))
        .find(|p| p.is_dir())
        .unwrap_or(cwd)
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~") {
        Some(rest) => PathBuf::from(format!("{}{rest}", std::env::var("HOME").unwrap_or_default())),
        None => PathBuf::from(p),
    }
}

/// The same folder on the remote: same path under home, matching an existing dir regardless of case.
fn remote_default(local: &Path, r: &Remote) -> String {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let rel = local.strip_prefix(&home).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| local.file_name().unwrap_or_default().to_string_lossy().into_owned());
    let first = rel.split('/').next().unwrap_or(&rel);
    let fixed = r.dirs.iter().find(|d| d.eq_ignore_ascii_case(first)).cloned().unwrap_or_else(|| first.to_string());
    format!("~/{}", [fixed.as_str()].into_iter().chain(rel.split('/').skip(1)).collect::<Vec<_>>().join("/"))
}

/// `hilo setup HOST [--folder DIR] [--remote-folder DIR] [--listen IP:PORT] [--no-service]`
pub fn command(args: &[String]) -> Result<(), String> {
    let flag = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let Some(host) = args.first().filter(|a| !a.starts_with("--")).cloned() else {
        return if tui::interactive() { wizard() } else { Err("usage: hilo setup HOST [--folder DIR]".into()) };
    };
    tui::guard_interrupt();
    let local_folder = std::fs::canonicalize(expand(&flag("--folder").unwrap_or_else(|| guess_folder().to_string_lossy().into()))).map_err(|e| format!("folder: {e}"))?;
    let h = host.clone();
    let remote = tui::step(&format!("Connecting to {host}"), move |_| {
        let r = probe(&h)?;
        let msg = format!("connected to {} — {}, {}", r.host, r.pretty, r.arch);
        Ok((r, msg))
    })?;
    let remote_folder = flag("--remote-folder").unwrap_or_else(|| remote_default(&local_folder, &remote));
    let plan = Plan { host, local_folder, remote_folder, listen: flag("--listen"), service: !args.iter().any(|a| a == "--no-service") };
    run(plan, remote)
}

fn cancelled(_: tui::Cancelled) -> String {
    "cancelled".into()
}

pub fn wizard() -> Result<(), String> {
    tui::guard_interrupt();
    tui::banner("the same code, live on all your machines");
    let cfg = Config::load();
    if cfg.get("role").is_some() {
        return menu(&cfg);
    }
    let folder = ask_folder(&guess_folder())?;
    let hosts = ssh_hosts();
    let mut choices: Vec<tui::Choice> = hosts.iter().map(|(a, h)| choice(a, &format!("{h}  · from ~/.ssh/config"))).collect();
    choices.push(choice("Another machine over SSH…", "user@host"));
    choices.push(choice("I have a join code", "from `hilo` on the other machine"));
    choices.push(choice("This machine is the server", "show a code for the others"));
    let n = hosts.len();
    let pick = tui::select("Where is your other machine?", &choices, 0).map_err(cancelled)?;
    match pick {
        i if i < n => ssh_flow(&hosts[i].0, folder),
        i if i == n => {
            let h = tui::input("SSH host", "user@host").map_err(cancelled)?;
            ssh_flow(&h, folder)
        }
        i if i == n + 1 => {
            let code = tui::input("Join code", "HILO-…").map_err(cancelled)?;
            let f = folder.clone();
            tui::step("Pairing this machine", move |_| {
                crate::join_core(code.trim(), &Opts { folder: Some(f), service: true, ..Default::default() })?;
                Ok(((), "paired".to_string()))
            })?;
            tui::card(&[bold(&tui::gradient("hilo is running")), String::new(), format!("{} {}", tui::tilde(&folder), dim("is linked; `hilo status` shows the rest"))]);
            Ok(())
        }
        _ => server_flow(folder),
    }
}

fn ask_folder(default: &Path) -> Result<PathBuf, String> {
    loop {
        let a = tui::input("Which folder should stay in sync?", &tui::tilde(default)).map_err(cancelled)?;
        let p = expand(a.trim());
        match std::fs::canonicalize(&p) {
            Ok(p) if p.is_dir() => {
                let repos = crate::repos_in(&p);
                let names: Vec<String> = repos.iter().take(6).map(|r| r.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect();
                let more = if repos.len() > 6 { format!(" +{}", repos.len() - 6) } else { String::new() };
                println!(
                    "  {}",
                    dim(&if repos.is_empty() { "no repos yet; the ones you create here will sync".to_string() } else { format!("{} repos: {}{more}", repos.len(), names.join(", ")) })
                );
                return Ok(p);
            }
            _ => println!("  {}", fg(tui::YELLOW, &format!("{} doesn't exist", p.display()))),
        }
    }
}

fn ssh_flow(host: &str, folder: PathBuf) -> Result<(), String> {
    let h = host.to_string();
    let remote = tui::step(&format!("Connecting to {host}"), move |_| {
        let r = probe(&h)?;
        let msg = format!("connected to {} — {}, {}", r.host, r.pretty, r.arch);
        Ok((r, msg))
    })?;
    let rdefault = remote_default(&folder, &remote);
    let remote_folder = tui::input(&format!("Folder on {host}?"), &rdefault).map_err(cancelled)?;
    let mut listen = None;
    if remote.role().is_empty() && Config::load().get("role") != Some("anchor") && remote.ips.len() > 1 {
        let cs: Vec<tui::Choice> = remote.ips.iter().map(|(ip, k)| choice(ip, k)).collect();
        let i = tui::select(&format!("Which address of {host} should the others dial?"), &cs, 0).map_err(cancelled)?;
        listen = Some(format!("{}:7710", remote.ips[i].0));
    }
    println!();
    println!("  {}  {}  {}", bold(&tui::tilde(&folder)), fg(CYAN, "⇄"), bold(&format!("{host}:{remote_folder}")));
    if remote.role().is_empty() && Config::load().get("role").is_none() {
        let ip = listen.clone().unwrap_or_else(|| format!("{}:7710", remote.ips.first().map_or("?", |i| i.0.as_str())));
        println!("  {}", dim(&format!("{host} becomes the anchor ({ip}); hilo runs in the background on both")));
    }
    println!();
    if !tui::confirm("Set it up?", true).map_err(cancelled)? {
        return Ok(());
    }
    println!();
    run(Plan { host: host.into(), local_folder: folder, remote_folder, listen, service: true }, remote)
}

fn server_flow(folder: PathBuf) -> Result<(), String> {
    let ips = local_ips();
    if ips.is_empty() {
        return Err("no private address on this machine for the others to dial".into());
    }
    let cs: Vec<tui::Choice> = ips.iter().map(|(ip, k)| choice(ip, k)).collect();
    let i = if ips.len() > 1 { tui::select("Which address should the others dial?", &cs, 0).map_err(cancelled)? } else { 0 };
    let listen = format!("{}:7710", ips[i].0);
    let f = folder.clone();
    let code = tui::step("Starting the anchor", move |_| {
        let (code, _) = crate::init_core(&Opts { folder: Some(f), listen: Some(listen.clone()), service: true, ..Default::default() })?;
        Ok((code, format!("anchor at {listen}")))
    })?;
    tui::card(&[dim("On the other machine run `hilo` and choose \"I have a join code\":"), String::new(), bold(&tui::gradient(&code)), String::new(), dim("valid for 10 minutes, once")]);
    tui::step("Waiting for the other machine", |_| {
        let end = Instant::now() + Duration::from_secs(600);
        while Instant::now() < end {
            if let Some(online) = progress().get("online") {
                let others: Vec<&str> = online.split(',').filter(|n| !n.is_empty() && *n != config::name()).collect();
                if !others.is_empty() {
                    return Ok(((), format!("{} joined", others.join(", "))));
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err("nobody joined in 10 minutes; `hilo invite` makes a new code".into())
    })
}

/// A reusable token for cloud sandboxes, made on the anchor (here or over SSH).
fn cloud_flow(cfg: &Config) -> Result<(), String> {
    let here = cfg.get("role") == Some("anchor");
    let remote = match (here, cfg.get("ssh")) {
        (true, _) => None,
        (false, Some(h)) => {
            let h = h.to_string();
            Some(tui::step(&format!("Connecting to the anchor ({h})"), move |_| {
                let r = probe(&h)?;
                let m = format!("connected to {}", r.host);
                Ok((r, m))
            })?)
        }
        (false, None) => return Err("tokens are made on the anchor: run `hilo` there".into()),
    };
    let listen = match &remote {
        None => cfg.get("listen").unwrap_or_default().to_string(),
        Some(r) => r.info.get("listen").cloned().unwrap_or_default(),
    };
    let host = listen.rsplit_once(':').map_or(listen.as_str(), |(h, _)| h);
    let private = host.parse::<std::net::IpAddr>().is_ok_and(|ip| match ip {
        std::net::IpAddr::V4(v) => v.is_private() || v.is_loopback() || (v.octets()[0] == 100 && (64..128).contains(&v.octets()[1])),
        std::net::IpAddr::V6(v) => v.is_loopback(),
    });
    if private {
        println!("  {}", fg(tui::YELLOW, &format!("The anchor listens on {listen}, a private address. Cloud sandboxes can't reach it.")));
        println!("  {}", dim("Give them a public way in: a VPS as anchor, a port forward on your router, or a domain."));
    }
    let addr = tui::input("Address cloud agents should dial", &listen).map_err(cancelled)?;
    let name = tui::input("Name for this token", "cloud agents").map_err(cancelled)?;
    let env = tui::confirm("Give them the project's .env values?", false).map_err(cancelled)?;
    let args = format!("token create --name {} --addr {}{}", q(&name.replace(' ', "-")), q(&addr), if env { " --env" } else { "" });
    let code = tui::step("Creating the token", move |_| {
        let out = match &remote {
            Some(r) => r.hilo(&args)?,
            None => {
                let o = Command::new(crate::bin()).args(args.split(' ').map(|a| a.trim_matches('\''))).output().map_err(|e| e.to_string())?;
                String::from_utf8_lossy(&o.stdout).into_owned()
            }
        };
        let code = code_in(&out).ok_or("no token came back")?;
        Ok((code, "token ready; revoke it any time with `hilo token revoke`".to_string()))
    })?;
    let mut lines = vec![bold(&tui::gradient("Cloud agents can join")), String::new()];
    lines.extend(crate::cloud_instructions(&code).into_iter().map(|l| if l.starts_with("   ") { fg(CYAN, &l) } else { l }));
    lines.push(String::new());
    lines.push(dim("Each sandbox joins as its own machine; its branch shows up here as it works."));
    tui::card(&lines);
    Ok(())
}

fn menu(cfg: &Config) -> Result<(), String> {
    let role = cfg.get("role").unwrap_or_default();
    let peer = cfg.get("ssh").map(str::to_string);
    println!("  {}", dim(&format!("{} is set up as the {role}.", config::name())));
    println!();
    let mut cs = vec![choice("Link another folder", "every repo in it syncs")];
    cs.push(choice("Set up another machine over SSH", ""));
    cs.push(choice("Let cloud agents in", "Claude Code on the web, Codex, Cursor, Codespaces, dev containers"));
    cs.push(choice("Show status", ""));
    cs.push(choice("Exit", ""));
    match tui::select("What do you want to do?", &cs, 0).map_err(cancelled)? {
        0 => {
            let folder = ask_folder(&guess_folder())?;
            let f = folder.clone();
            tui::step("Linking", move |_| {
                let (c, t) = ipc::call(&ipc::args(&["add", &f.to_string_lossy()]), Duration::from_secs(30)).map_err(|_| "the hilo daemon isn't running".to_string())?;
                if c == 0 { Ok(((), t)) } else { Err(t) }
            })?;
            if let Some(host) = peer {
                let r = probe(&host)?;
                let rf = tui::input(&format!("Same folder on {host}?"), &remote_default(&folder, &r)).map_err(cancelled)?;
                let rr = r.clone();
                tui::step(&format!("Linking it on {host}"), move |_| {
                    ssh(&rr.host, &format!("mkdir -p {}", remote_path(&rf)))?;
                    Ok(((), rr.hilo(&format!("add {}", remote_path(&rf)))?.trim().to_string()))
                })?;
            } else {
                println!("  {}", dim("Run `hilo add <folder>` on the other machine too (same folder name)."));
            }
            Ok(())
        }
        1 => {
            let h = match ssh_hosts() {
                hs if !hs.is_empty() => {
                    let cs: Vec<tui::Choice> = hs.iter().map(|(a, h)| choice(a, h)).collect();
                    hs[tui::select("Which machine?", &cs, 0).map_err(cancelled)?].0.clone()
                }
                _ => tui::input("SSH host", "user@host").map_err(cancelled)?,
            };
            let folder = cfg.all("folder").first().map(PathBuf::from).unwrap_or_else(guess_folder);
            ssh_flow(&h, folder)
        }
        2 => cloud_flow(cfg),
        3 => {
            match ipc::call(&ipc::args(&["status"]), Duration::from_secs(10)) {
                Ok((_, t)) => println!("{t}"),
                Err(_) => println!("the hilo daemon isn't running"),
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn addresses() {
        let out = "ip=127.0.0.1 lo\nip=192.168.1.20 wlp4s0\nip=172.17.0.1 docker0\nip=172.18.0.1 br-37143d8de6c8\nip=100.101.1.2 tailscale\nip=10.0.0.5 eth0\nip=8.8.8.8 eth1\n";
        let v: Vec<String> = rank(out).into_iter().map(|x| x.0).collect();
        assert_eq!(v, ["100.101.1.2", "192.168.1.20", "10.0.0.5"]);
        assert_eq!(remote_path("~/codigo"), "\"$HOME\"/'codigo'");
        let r = Remote { dirs: vec!["code".into(), "codigo".into()], ..Default::default() };
        let home = std::env::var("HOME").unwrap();
        assert_eq!(remote_default(&PathBuf::from(format!("{home}/Codigo")), &r), "~/codigo");
        assert_eq!(remote_default(&PathBuf::from(format!("{home}/work/x")), &r), "~/work/x");
    }
}
