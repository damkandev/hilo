mod anchor;
mod config;
mod crypto;
mod daemon;
mod fsops;
mod git;
mod grep;
mod hooks;
mod install;
mod ipc;
mod json;
mod lease;
mod mcp;
mod net;
mod noise;
mod scan;
mod secret;
mod setup;
mod store;
mod sync;
mod tui;
mod vault;
mod watch;
mod wire;

use config::Config;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// UTC timestamp: `2026-11-13 14:37:33` for logs, `20261113-143733-123456789` for ref names.
pub fn stamp(compact: bool) -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    let s = date(d.as_secs());
    if compact {
        format!("{}-{:09}", s.replace(['-', ':'], "").replace(' ', "-"), d.subsec_nanos())
    } else {
        s
    }
}

/// `2026-11-13 14:37:33` (UTC) for unix seconds.
pub fn date(secs: u64) -> String {
    let secs = secs as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    format!("{year}-{month:02}-{day:02} {h:02}:{m:02}:{s:02}")
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { eprintln!("{} hilo: {}", $crate::stamp(false), format!($($t)*)) };
}

/// Desktop notification, best effort (HILO_NOTIFY=0 turns it off).
pub fn notify(msg: &str) {
    if std::env::var("HILO_NOTIFY").as_deref() == Ok("0") {
        return;
    }
    let msg = msg.to_string();
    std::thread::spawn(move || {
        if cfg!(target_os = "macos") {
            let q = format!("\"{}\"", msg.replace('\\', "\\\\").replace('"', "\\\""));
            let _ = std::process::Command::new("osascript").args(["-e", &format!("display notification {q} with title \"hilo\"")]).status();
        } else {
            let _ = std::process::Command::new("notify-send").args(["hilo", &msg]).status();
        }
    });
}

const USAGE: &str = "hilo: the same code, live on all your machines

  hilo                                      the setup wizard (in a terminal)
  hilo setup HOST [--folder DIR]            set up HOST over SSH and pair this machine with it

Or by hand:

  hilo init [--listen IP:PORT] [--public]   on the reachable machine (usually the server): print a join code
  hilo join CODE                            on every other machine

Both link the folder you run them in (or --folder DIR): every repo in it syncs, and new repos you
create there appear on the other machines on their own. The rest is optional:

  hilo add FOLDER                           link another folder (or a single repo)

Cloud agents (Claude Code on the web, Codex, Cursor, Codespaces, dev containers…):

  hilo token create [--env] [--addr H:P]    on the anchor: a reusable, revocable token for sandboxes
      [--path P] [--expires 24h] [--read-only]   only one folder or repo, for a while, without writing
  hilo up [--read-only]                     in the sandbox's setup script, with HILO_TOKEN set
  hilo token ls | revoke ID

  hilo status [--latency]                   machines, worktrees, leases, conflicts, waiting approvals
  hilo env set KEY=value | unset KEY        change a variable everywhere (add --file apps/web/.env)
  hilo env local KEY | prod KEY             keep a key per machine | only on this one, only via `hilo run`
  hilo env ls | history [KEY] | revert KEY [N]
  hilo run -- CMD ...                       run with the vault's variables, without writing files
  hilo grep PATTERN [DIR] [-i]              search the synced files (literal text, -i ignores case)
  hilo mcp                                  MCP server for agents: leases and file tools (stdio)
  hilo approve [PATH | --all | --deny PATH] review changes to paths that run code
  hilo log PATH                             every version of a file hilo kept, newest first
  hilo restore PATH [--at N | --conflict]   bring a version back (default: the newest that differs)
  hilo trash [DIR]                          files deleted here that hilo can bring back
  hilo trust MACHINE read-only|full         stop (or allow) a machine writing here
  hilo invite                               a new join code (anchor)
  hilo unpair MACHINE                       cut a machine off and rotate the vault key (anchor)
  hilo uninstall                            remove service, hooks and config

Flags for init/join/setup: --folder DIR, --no-service (don't install launchd/systemd), --name NAME.
init only: --bind IP:PORT when the anchor listens on a different address than the one it advertises (NAT, containers).";

/// What `init` / `join` (and the wizard) were asked to set up.
#[derive(Default, Clone)]
pub struct Opts {
    pub name: Option<String>,
    pub folder: Option<PathBuf>,
    pub service: bool,
    pub listen: Option<String>,
    pub bind: Option<String>,
    pub public: bool,
}

impl Opts {
    fn from_args(args: &[String]) -> Opts {
        Opts {
            name: flag(args, "--name"),
            folder: flag(args, "--folder").map(PathBuf::from),
            service: !args.iter().any(|a| a == "--no-service"),
            listen: flag(args, "--listen"),
            bind: flag(args, "--bind"),
            public: args.iter().any(|a| a == "--public"),
        }
    }
}

/// Repos directly inside `dir` (what a linked folder syncs).
pub fn repos_in(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| p.join(".git").is_dir())
        .collect();
    v.sort();
    v
}

/// One repo if `path` is a repo's top level; otherwise a linked folder (every repo in it, new ones too).
fn register(cfg: &mut Config, path: &Path) -> Result<String, String> {
    let path = std::fs::canonicalize(path).map_err(|_| format!("{}: no such folder", path.display()))?;
    let top = git::toplevel(&path).and_then(|t| std::fs::canonicalize(t).ok());
    if top.as_deref() == Some(path.as_path()) {
        git::repo_id(&path).map_err(|_| format!("{} needs at least one commit", path.display()))?;
        cfg.add("repo", &path.to_string_lossy());
        return Ok(format!("repo     {}", tui::tilde(&path)));
    }
    let label = daemon::folder_label(&path);
    let taken = cfg.all("folder").into_iter().filter_map(|f| std::fs::canonicalize(f).ok()).any(|f| f != path && daemon::folder_label(&f) == label);
    if taken {
        return Err(format!("another linked folder is already called \"{label}\""));
    }
    cfg.add("folder", &path.to_string_lossy());
    Ok(format!("folder   {} ({} repos; new ones sync on their own)", tui::tilde(&path), repos_in(&path).len()))
}

fn daemon_up() -> bool {
    ipc::call(&ipc::args(&["progress"]), std::time::Duration::from_secs(1)).is_ok()
}

/// Folder to register: --folder, or where you are.
fn target_folder(o: &Opts) -> PathBuf {
    o.folder.clone().unwrap_or_else(|| std::env::current_dir().expect("cwd"))
}

/// If a daemon is already running, tell it about the folder (it only reads the config at start).
fn tell_daemon(path: &Path) {
    let _ = ipc::call(&ipc::args(&["add", &path.to_string_lossy()]), std::time::Duration::from_secs(30));
}

fn is_private(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v) => {
            let o = v.octets();
            v.is_private() || v.is_loopback() || (o[0] == 100 && (64..128).contains(&o[1]))
        }
        std::net::IpAddr::V6(v) => v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00,
    }
}

fn flag(args: &[String], f: &str) -> Option<String> {
    args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned()
}

pub fn bin() -> PathBuf {
    std::env::current_exe().and_then(std::fs::canonicalize).unwrap_or_else(|_| PathBuf::from("hilo"))
}

fn setup_common(o: &Opts, cfg: &mut Config) -> Result<(), String> {
    config::ensure_home().map_err(|e| e.to_string())?;
    config::key().map_err(|e| format!("cannot store the machine key: {e}"))?;
    if let Some(n) = &o.name {
        cfg.set("name", n);
    }
    if cfg.get("name").is_none() {
        cfg.set("name", &config::name());
    }
    Ok(())
}

fn wait_for_daemon() -> bool {
    (0..40).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        daemon_up()
    })
}

/// Becomes (or stays) the anchor. Returns a fresh join code and what was done.
pub fn init_core(o: &Opts) -> Result<(String, Vec<String>), String> {
    let mut cfg = Config::load();
    if cfg.get("role") == Some("member") {
        return Err("this machine already joined another anchor".into());
    }
    let listen = match o.listen.clone().or_else(|| cfg.get("listen").map(str::to_string)) {
        Some(l) => l,
        None => format!("{}:7710", setup::local_ips().first().map(|i| i.0.clone()).ok_or("no private address found; pass --listen IP:PORT")?),
    };
    let sa: std::net::SocketAddr = listen.parse().map_err(|_| format!("--listen wants IP:PORT, got {listen}"))?;
    if sa.ip().is_unspecified() {
        return Err("--listen needs the address other machines will dial, not 0.0.0.0".into());
    }
    if !is_private(sa.ip()) && !o.public {
        return Err(format!("{} is a public address; add --public if you really want the anchor exposed", sa.ip()));
    }
    let was_running = daemon_up();
    setup_common(o, &mut cfg)?;
    let mut lines = vec![format!("identity {} ({})", cfg.get("name").unwrap_or_default(), if cfg!(target_os = "macos") && std::env::var("HILO_KEYCHAIN").as_deref() != Ok("0") { "Keychain" } else { "~/.hilo/secrets" })];
    cfg.set("role", "anchor");
    cfg.set("listen", &listen);
    if let Some(b) = &o.bind {
        b.parse::<std::net::SocketAddr>().map_err(|_| format!("--bind wants IP:PORT, got {b}"))?;
        cfg.set("bind", b); // behind NAT/containers: listen here, advertise --listen
    }
    let folder = target_folder(o);
    lines.push(register(&mut cfg, &folder)?);
    cfg.save().map_err(|e| e.to_string())?;
    if vault::epoch() == 0 {
        vault::new_epoch().map_err(|e| e.to_string())?;
    }
    lines.extend(install::install(&bin(), o.service));
    if was_running {
        tell_daemon(&folder);
    } else if o.service && !wait_for_daemon() {
        lines.push("the daemon isn't running yet: start it with `hilo daemon`".into());
    }
    let t = config::new_token().map_err(|e| e.to_string())?;
    Ok((config::format_code(&t, &listen), lines))
}

fn init(args: &[String]) -> Result<(), String> {
    let (code, lines) = init_core(&Opts::from_args(args))?;
    println!("hilo init");
    lines.iter().for_each(|l| println!("  {l}"));
    println!("\nOn the other machine, within 10 minutes:\n\n  hilo join {code}\n");
    Ok(())
}

fn invite() -> Result<(), String> {
    let cfg = Config::load();
    if cfg.get("role") != Some("anchor") {
        return Err("only the anchor hands out codes".into());
    }
    let t = config::new_token().map_err(|e| e.to_string())?;
    println!("hilo join {}", config::format_code(&t, cfg.get("listen").unwrap_or_default()));
    Ok(())
}

/// Pairs with the anchor in `code`.
pub fn join_core(code: &str, o: &Opts) -> Result<Vec<String>, String> {
    // a one-time code, or a reusable cloud token
    let (psk, addr) = match (config::parse_code(code), config::parse_cloud(code)) {
        (Some((t, a)), _) => (config::token_psk(&t), a),
        (_, Some((s, a))) => (config::cloud_psk(&s), a),
        _ => return Err("that does not look like a hilo code or token".into()),
    };
    let mut cfg = Config::load();
    if cfg.get("role") == Some("anchor") {
        return Err("this machine is an anchor; it cannot join another".into());
    }
    let was_running = daemon_up();
    setup_common(o, &mut cfg)?;
    let key = config::key().map_err(|e| e.to_string())?;
    let mut s = net::dial(&addr).map_err(|e| format!("{addr}: {e}"))?;
    let failed = "pairing failed: wrong, used, expired or revoked code (run `hilo invite` on the anchor for a new one)";
    let (mut tx, mut rx, anchor_key) = net::initiate(&mut s, key, &psk).map_err(|_| failed)?;
    // XX's initiator can't tell if its last message was accepted: wait for an encrypted answer.
    net::send(&mut s, &mut tx, &wire::Msg::Hello { last_seq: u64::MAX, name: cfg.get("name").unwrap_or("new").into() }).map_err(|_| failed)?;
    loop {
        match net::recv(&mut s, &mut rx) {
            Ok(wire::Msg::CaughtUp) => break,
            Ok(wire::Msg::VaultKey { epoch, key }) => vault::store_key(epoch, &key).map_err(|e| e.to_string())?,
            Ok(_) => {}
            Err(_) => return Err(failed.into()),
        }
    }
    drop(s);
    let mut lines = vec![format!("anchor   {addr} ({})", &crypto::hex(&anchor_key)[..8])];
    cfg.set("role", "member");
    cfg.set("anchor", &addr);
    cfg.set("anchor_key", &crypto::hex(&anchor_key));
    let folder = target_folder(o);
    lines.push(register(&mut cfg, &folder)?);
    cfg.save().map_err(|e| e.to_string())?;
    lines.extend(install::install(&bin(), o.service));
    if was_running {
        // an old daemon (other anchor or none) must restart to pick up the new pairing
        lines.push("restart the daemon to use the new anchor".into());
    } else if o.service {
        wait_for_daemon();
    }
    Ok(lines)
}

fn join(args: &[String]) -> Result<(), String> {
    let code = args.first().filter(|c| !c.starts_with("--")).ok_or("usage: hilo join CODE")?;
    let lines = join_core(code, &Opts::from_args(args))?;
    println!("hilo join");
    lines.iter().for_each(|l| println!("  {l}"));
    println!("\nPaired. Your agents' changes now reach the other machines on their own.");
    Ok(())
}

// ---------- cloud agents ----------

pub const INSTALL_URL: &str = "https://raw.githubusercontent.com/damkandev/hilo/main/install.sh";

/// What to paste into a cloud agent platform.
pub fn cloud_instructions(token: &str) -> Vec<String> {
    vec![
        "1. Add a secret (environment variable) to the platform:".into(),
        format!("   HILO_TOKEN={token}"),
        "2. Add this line to its setup script:".into(),
        format!("   curl -fsSL {INSTALL_URL} | sh -s -- up"),
        "3. If the sandbox limits the network, allow the anchor's address.".into(),
        "   hilo goes through HTTPS_PROXY / ALL_PROXY when they are set.".into(),
    ]
}

fn is_private_addr(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
    host.parse::<std::net::IpAddr>().is_ok_and(is_private)
}

/// `30m`, `24h`, `7d` -> seconds.
fn duration(s: &str) -> Option<u64> {
    let (n, unit) = [("m", 60), ("h", 3600), ("d", 86400)].into_iter().find_map(|(u, k)| Some((s.strip_suffix(u)?, k)))?;
    n.parse::<u64>().ok()?.checked_mul(unit).filter(|&x| x > 0)
}

/// `--path` on the anchor -> what the token may see: a folder of a drive, or a whole repo.
fn token_scope(cfg: &Config, p: &str) -> Result<config::Scope, String> {
    let path = std::fs::canonicalize(p).map_err(|e| format!("{p}: {e}"))?;
    let dir = if path.is_dir() { path.clone() } else { path.parent().map(Path::to_path_buf).unwrap_or_default() };
    let repo = git::toplevel(&dir).and_then(|t| std::fs::canonicalize(t).ok());
    for d in cfg.all("drive") {
        // `drive = <label> <absolute path>`: the path starts at the first " /"
        let Some((label, root)) = d.split_once(" /") else { continue };
        let root = std::fs::canonicalize(format!("/{root}")).unwrap_or_else(|_| PathBuf::from(format!("/{root}")));
        let Ok(rest) = path.strip_prefix(&root) else { continue };
        if repo.as_ref().is_some_and(|r| r.starts_with(&root)) {
            break; // a repo inside the drive: the drive skips it, it syncs as a repo
        }
        let prefix = rest.to_str().ok_or(format!("{}: not UTF-8", path.display()))?;
        return Ok(config::Scope { unit: format!("drive:{}", label.trim()), prefix: prefix.to_string() });
    }
    let top = repo.ok_or(format!("{} is not inside a drive or a repo", path.display()))?;
    if top != path {
        eprintln!("note: a repo is shared whole; the token sees all of {}", top.display());
    }
    let id = git::repo_id(&top).map_err(|e| format!("{}: {e}", top.display()))?;
    Ok(config::Scope { unit: format!("repo:{id}"), prefix: String::new() })
}

/// `drive:notes/plans, read-only, expires in 6 h` for `hilo token create` and `ls`.
fn token_limits(t: &config::CloudToken) -> String {
    let mut v = vec![match &t.scope {
        None => "everything".to_string(),
        Some(s) if s.prefix.is_empty() => s.unit.clone(),
        Some(s) => format!("{}/{}", s.unit, s.prefix),
    }];
    if t.ro {
        v.push("read-only".into());
    }
    let now = config::now();
    match t.expires {
        0 => {}
        e if e <= now => v.push("EXPIRED".into()),
        e if e - now < 3600 => v.push(format!("expires in {} min", (e - now).div_ceil(60))),
        e if e - now < 2 * 86400 => v.push(format!("expires in {} h", (e - now) / 3600)),
        e => v.push(format!("expires in {} days", (e - now) / 86400)),
    }
    v.join(", ")
}

/// `hilo token create | ls | revoke ID` (on the anchor).
fn token(args: &[String]) -> Result<(), String> {
    let cfg = Config::load();
    if cfg.get("role") != Some("anchor") {
        return Err("cloud tokens are made on the anchor (run this there, or use `hilo` and pick \"Let cloud agents in\")".into());
    }
    match args.first().map(String::as_str) {
        Some("create") | None => {
            let name = flag(args, "--name").unwrap_or_else(|| "cloud".into());
            let env = args.iter().any(|a| a == "--env");
            let addr = flag(args, "--addr").or_else(|| cfg.get("listen").map(str::to_string)).ok_or("no address: pass --addr HOST:PORT")?;
            let scope = flag(args, "--path").map(|p| token_scope(&cfg, &p)).transpose()?;
            let expires = match flag(args, "--expires") {
                Some(d) => config::now() + duration(&d).ok_or(format!("--expires {d}: use something like 30m, 24h or 7d"))?,
                None => 0,
            };
            let ro = args.iter().any(|a| a == "--read-only");
            let t = config::new_cloud_token(&name, env, scope, expires, ro).map_err(|e| e.to_string())?;
            let code = config::format_cloud(&t.secret, &addr);
            println!("token  {code}");
            println!("id     {} ({name}, {})", t.id, if env { "gets .env values" } else { "no .env values" });
            println!("sees   {}\n", token_limits(&t));
            // a read-only sandbox's own tools refuse writes too (the anchor turns them down either way)
            cloud_instructions(&code).iter().for_each(|l| println!("{l}{}", if ro && l.ends_with(" up") { " --read-only" } else { "" }));
            if is_private_addr(&addr) {
                eprintln!("\nnote: {addr} is a private address; cloud sandboxes can't reach it. Pass --addr with a public host:port (a VPS, a port forward, a domain).");
            }
            println!("\nRevoke it any time with `hilo token revoke {}`.", t.id);
            Ok(())
        }
        Some("ls") => {
            let ts = config::cloud_tokens();
            if ts.is_empty() {
                println!("no cloud tokens");
            }
            let peers = config::peers();
            for t in ts {
                let n = peers.iter().filter(|p| p.cloud_token() == Some(t.id.as_str())).count();
                println!("{}  {:<16} {}  {} machine(s)  {}{}", t.id, t.name, if t.env { "env   " } else { "no env" }, n, token_limits(&t), if t.revoked { "  REVOKED" } else { "" });
            }
            Ok(())
        }
        Some("revoke") => {
            let id = args.get(1).ok_or("usage: hilo token revoke ID")?;
            if daemon_up() {
                return daemon_call(&["revoke", id]);
            }
            let mut ts = config::cloud_tokens();
            let t = ts.iter_mut().find(|t| &t.id == id).ok_or(format!("no cloud token {id}"))?;
            t.revoked = true;
            config::save_cloud_tokens(&ts).map_err(|e| e.to_string())?;
            let keep: Vec<config::Peer> = config::peers().into_iter().filter(|p| p.cloud_token() != Some(id.as_str())).collect();
            config::save_peers(&keep).map_err(|e| e.to_string())?;
            println!("token {id} revoked");
            Ok(())
        }
        _ => Err("usage: hilo token create [--name N] [--env] [--addr HOST:PORT] [--path P] [--expires 24h] [--read-only] | ls | revoke ID".into()),
    }
}

/// Starts `hilo daemon` in the background, detached from this shell (no launchd/systemd in a sandbox).
fn spawn_daemon() -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    extern "C" {
        fn setsid() -> i32;
    }
    config::ensure_home().map_err(|e| e.to_string())?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(config::path("daemon.log")).map_err(|e| e.to_string())?;
    let mut c = std::process::Command::new(bin());
    c.arg("daemon").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log);
    unsafe {
        c.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
    c.spawn().map_err(|e| e.to_string())?;
    Ok(())
}

/// `hilo up`: for setup scripts of cloud agents and dev containers. Joins with HILO_TOKEN (once),
/// links the repo or folder you're in, starts the daemon in the background and waits until in sync.
fn up(args: &[String]) -> Result<(), String> {
    let folder = flag(args, "--folder").map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let folder = std::fs::canonicalize(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    let cfg = Config::load();
    if cfg.get("role").is_none() {
        let token = flag(args, "--token").or_else(|| std::env::var("HILO_TOKEN").ok()).ok_or("set HILO_TOKEN (from `hilo token create` on the anchor)")?;
        let name = flag(args, "--name").or_else(|| std::env::var("HILO_NAME").ok()).unwrap_or_else(|| format!("cloud-{}", config::name()));
        join_core(token.trim(), &Opts { name: Some(name), folder: Some(folder.clone()), service: false, ..Default::default() })?;
        println!("hilo: joined {}", Config::load().get("anchor").unwrap_or_default());
    } else if daemon_up() {
        tell_daemon(&folder);
    } else {
        let mut cfg = cfg;
        register(&mut cfg, &folder)?;
        cfg.save().map_err(|e| e.to_string())?;
    }
    // the anchor turns down this machine's writes either way; this makes its own tools refuse them first
    if args.iter().any(|a| a == "--read-only") || std::env::var("HILO_READONLY").as_deref() == Ok("1") {
        let mut cfg = Config::load();
        cfg.set("scope_ro", "1");
        cfg.save().map_err(|e| e.to_string())?;
    }
    if !daemon_up() {
        spawn_daemon()?;
        if !wait_for_daemon() {
            return Err("the daemon did not start (see `hilo status`)".into());
        }
    }
    if args.iter().any(|a| a == "--no-wait") {
        return Ok(());
    }
    let end = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let p: std::collections::HashMap<String, String> = ipc::call(&ipc::args(&["progress"]), std::time::Duration::from_secs(2))
            .map(|(_, t)| t.split(' ').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.into(), v.into())).collect())
            .unwrap_or_default();
        if p.get("caught").map(String::as_str) == Some("1") && p.get("pending").map(String::as_str) == Some("0") {
            println!("hilo: {} in sync ({} repo(s), online: {})", tui::tilde(&folder), p.get("repos").map_or("0", String::as_str), p.get("online").map_or("-", String::as_str));
            return Ok(());
        }
        if std::time::Instant::now() > end {
            return Err("not in sync after 2 minutes: can this sandbox reach the anchor? (`hilo status`)".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// `hilo add PATH`: link a folder (or one repo) on this machine.
fn add(args: &[String]) -> Result<(), String> {
    let p = args.first().ok_or("usage: hilo add FOLDER")?;
    let path = std::fs::canonicalize(p).map_err(|_| format!("{p}: no such folder"))?;
    if daemon_up() {
        return daemon_call(&["add", &path.to_string_lossy()]);
    }
    let mut cfg = Config::load();
    let line = register(&mut cfg, &path)?;
    cfg.save().map_err(|e| e.to_string())?;
    println!("{line}");
    Ok(())
}

/// Machine-readable facts, for `hilo setup` probing over SSH.
fn info() {
    let cfg = Config::load();
    println!("version={}", env!("CARGO_PKG_VERSION"));
    for k in ["role", "name", "listen", "anchor", "anchor_key"] {
        println!("{k}={}", cfg.get(k).unwrap_or_default());
    }
    if cfg.get("role").is_some() {
        if let Ok(k) = config::key() {
            println!("key={}", crypto::hex(&noise::public_key(&k)));
        }
    }
    for f in cfg.all("folder") {
        println!("folder={f}");
    }
    if let Ok((_, t)) = ipc::call(&ipc::args(&["progress"]), std::time::Duration::from_secs(2)) {
        for kv in t.split(' ') {
            println!("{kv}");
        }
    }
}

/// A CLI path made absolute against the cwd (the daemon's cwd is not ours).
fn abs(p: &str) -> String {
    std::env::current_dir().map_or(PathBuf::from(p), |c| c.join(p)).to_string_lossy().into_owned()
}

fn daemon_call(a: &[&str]) -> Result<(), String> {
    match ipc::call(&ipc::args(a), std::time::Duration::from_secs(60)) {
        Ok((0, t)) => {
            println!("{t}");
            Ok(())
        }
        Ok((_, t)) => Err(t),
        Err(_) => Err("the hilo daemon isn't running (start it with `hilo daemon`)".into()),
    }
}

fn status(args: &[String]) -> Result<(), String> {
    let mut a = vec!["status"];
    if args.iter().any(|x| x == "--latency") {
        a.push("--latency");
    }
    if daemon_call(&a).is_ok() {
        return Ok(());
    }
    let cfg = Config::load();
    let Some(role) = cfg.get("role") else {
        println!("not set up: run `hilo init` or `hilo join`");
        return Ok(());
    };
    println!("machine   {} ({role}) — daemon not running", config::name());
    for r in cfg.all("repo") {
        println!("repo      {r} [{}] conflicts: {}", git::gitdir(Path::new(r)).ok().and_then(|g| git::branch(&g)).unwrap_or("detached".into()), git::conflict_branches(Path::new(r)));
    }
    Ok(())
}

fn env(args: &[String]) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?.to_string_lossy().into_owned();
    let file = flag(args, "--file").unwrap_or_default();
    let rest: Vec<&str> = {
        let mut v = vec![];
        let mut skip = false;
        for a in args {
            if skip {
                skip = false;
            } else if a == "--file" {
                skip = true;
            } else {
                v.push(a.as_str());
            }
        }
        v
    };
    let (cmd, mut params): (&str, Vec<String>) = match rest.as_slice() {
        ["set", kv] => {
            let (k, v) = kv.split_once('=').ok_or("usage: hilo env set KEY=value")?;
            ("set", vec![k.into(), v.into()])
        }
        [cmd, more @ ..] => (cmd, more.iter().map(|s| s.to_string()).collect()),
        [] => ("ls", vec![]),
    };
    let mut a = vec!["env".to_string(), cwd, file, cmd.to_string()];
    a.append(&mut params);
    let a: Vec<&str> = a.iter().map(String::as_str).collect();
    daemon_call(&a)
}

/// Strict mode: variables go to the process only, never to a file.
fn run_cmd(args: &[String]) -> Result<i32, String> {
    let cmd = match args.first().map(String::as_str) {
        Some("--") => &args[1..],
        _ => args,
    };
    let (prog, rest) = cmd.split_first().ok_or("usage: hilo run -- CMD ...")?;
    let cwd = std::fs::canonicalize(std::env::current_dir().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let root = git::toplevel(&cwd).ok_or("not inside a git repo")?;
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let repo = git::repo_id(&root).map_err(|e| e.to_string())?;
    let store = vault::Store::load(&repo);
    let keys = vault::keys();
    let rel = cwd.strip_prefix(&root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let mut dirs = vec![String::new()];
    let mut acc = String::new();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        acc = if acc.is_empty() { part.into() } else { format!("{acc}/{part}") };
        dirs.push(acc.clone());
    }
    let mut vars = std::collections::BTreeMap::new();
    for d in dirs {
        for name in [".env", ".env.local"] {
            let f = if d.is_empty() { name.to_string() } else { format!("{d}/{name}") };
            for (k, v) in store.effective(&keys, &f, true) {
                vars.insert(k, vault::unquote(&v));
            }
        }
    }
    let status = std::process::Command::new(prog).args(rest).envs(vars).status().map_err(|e| format!("{prog}: {e}"))?;
    Ok(status.code().unwrap_or(1))
}

fn uninstall() -> Result<(), String> {
    println!("hilo uninstall");
    for line in install::uninstall(&bin()) {
        println!("  {line}");
    }
    let _ = std::fs::remove_file(ipc::socket_path());
    let epochs = vault::epoch();
    for name in ["machine".to_string(), "local".into()].into_iter().chain((1..=epochs).map(|e| format!("vault-{e}"))) {
        let _ = secret::delete(&name);
    }
    let home = config::home();
    for e in std::fs::read_dir(&home).into_iter().flatten().flatten() {
        let p = e.path();
        let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
    }
    println!("  removed keys and {}", home.display());
    println!("\nYour repos are plain git repos again. Conflict branches (hilo/conflict/*) are still there for you.");
    Ok(())
}

extern "C" {
    fn signal(sig: i32, handler: usize) -> usize;
}

fn main() -> ExitCode {
    // `hilo status | head` should just stop, like any CLI (Rust ignores SIGPIPE by default)
    unsafe { signal(13, 0) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    let res = match args.first().map(String::as_str) {
        Some("init") => init(rest),
        Some("join") => join(rest),
        Some("invite") => invite(),
        Some("add") => add(rest),
        Some("token") => token(rest),
        Some("up") => up(rest),
        Some("info") => {
            info();
            Ok(())
        }
        Some("setup") => setup::command(rest),
        Some("daemon") => daemon::run().map_err(|e| e.to_string()),
        Some("status") => status(rest),
        Some("env") => env(rest),
        Some("run") => match run_cmd(rest) {
            Ok(code) => return ExitCode::from(code as u8),
            Err(e) => Err(e),
        },
        Some("log") => match rest {
            [p] => daemon_call(&["log", &abs(p)]),
            _ => Err("usage: hilo log PATH".into()),
        },
        Some("restore") => match rest {
            [p, mode @ ..] => {
                let mut a = vec!["restore".to_string(), abs(p)];
                a.extend(mode.iter().cloned());
                daemon_call(&a.iter().map(String::as_str).collect::<Vec<_>>())
            }
            _ => Err("usage: hilo restore PATH [--at N | --conflict]".into()),
        },
        Some("trash") => match rest {
            [] => daemon_call(&["trash", &abs("."), "--unit"]),
            [d] => daemon_call(&["trash", &abs(d)]),
            _ => Err("usage: hilo trash [DIR]".into()),
        },
        Some("approve") => {
            let mut a = vec!["approve"];
            a.extend(rest.iter().map(String::as_str));
            daemon_call(&a)
        }
        Some("trust") => match rest {
            [m, mode] if mode == "read-only" || mode == "full" => daemon_call(&["trust", m, mode]).or_else(|_| {
                let mut cfg = Config::load();
                let mut ro: Vec<String> = cfg.all("readonly").into_iter().map(str::to_string).filter(|n| n != m).collect();
                if mode == "read-only" {
                    ro.push(m.clone());
                }
                cfg.remove("readonly");
                ro.iter().for_each(|r| cfg.add("readonly", r));
                cfg.save().map_err(|e| e.to_string())?;
                println!("{m}: {mode}");
                Ok(())
            }),
            _ => Err("usage: hilo trust MACHINE read-only|full".into()),
        },
        Some("unpair") => match rest {
            [m] => daemon_call(&["unpair", m]),
            _ => Err("usage: hilo unpair MACHINE".into()),
        },
        Some("grep") => {
            let ic = rest.iter().any(|a| a == "-i");
            let pos: Vec<&String> = rest.iter().filter(|a| *a != "-i").collect();
            match pos[..] {
                [pat] | [pat, _] => {
                    let dir = std::path::absolute(pos.get(1).map_or(".", |d| d.as_str())).map_err(|e| e.to_string());
                    dir.and_then(|d| daemon_call(&["grep", pat, &d.to_string_lossy(), if ic { "i" } else { "" }]))
                }
                _ => Err("usage: hilo grep PATTERN [DIR] [-i]".into()),
            }
        }
        Some("uninstall") => uninstall(),
        Some("hook") => return ExitCode::from(hooks::run(rest.first().map_or("", String::as_str)) as u8),
        Some("mcp") => return ExitCode::from(mcp::run() as u8),
        Some("--version" | "-V") => {
            println!("hilo {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        None if tui::interactive() => setup::wizard(),
        _ => {
            println!("{USAGE}");
            Ok(())
        }
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hilo: {e}");
            ExitCode::FAILURE
        }
    }
}
