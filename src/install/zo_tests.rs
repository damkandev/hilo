//! Zo Computer: `is_zo`, `zo_detected`, `zo_instructions` and `linux_service`.
//! Child module of `install` (sees its private items). Wire it from src/install.rs with:
//!     #[cfg(test)]
//!     mod zo_tests;
#![cfg(unix)]

use super::*;
use std::path::{Path, PathBuf};

const BIN: &str = "/opt/hilo/bin/hilo";

fn tmp_home() -> PathBuf {
    let d = std::env::temp_dir().join(format!("hilo-zo-{}", crate::crypto::hex(&crate::crypto::random::<6>())));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn unit_path(home: &Path) -> PathBuf {
    home.join(".config/systemd/user/hilo.service")
}

/// The keys linux_service must carry, as defined in this process's environment, in order.
fn env_pairs() -> Vec<(String, String)> {
    ["HILO_HOME", "HILO_KEYCHAIN", "HILO_NOTIFY"].iter().filter_map(|k| Some((k.to_string(), std::env::var(k).ok()?))).collect()
}

type Calls = Vec<(String, Vec<String>)>;

/// Runs linux_service with a fake runner (answer decides each command's result) and a fake start.
fn call(home: &Path, add: bool, zo: bool, answer: impl Fn(&str, &[&str]) -> bool, start_result: Result<(), String>) -> (Option<String>, Calls, usize) {
    let mut calls: Calls = vec![];
    let mut starts = 0usize;
    let out = {
        let mut run = |c: &str, a: &[&str]| {
            calls.push((c.to_string(), a.iter().map(|s| s.to_string()).collect()));
            answer(c, a)
        };
        let mut start = || {
            starts += 1;
            start_result.clone()
        };
        linux_service(home, Path::new(BIN), add, zo, &mut run, &mut start)
    };
    (out, calls, starts)
}

fn cmd(c: &str, a: &[&str]) -> (String, Vec<String>) {
    (c.to_string(), a.iter().map(|s| s.to_string()).collect())
}

fn has_line(text: &str, want: &str) -> bool {
    text.lines().any(|l| l.trim() == want)
}

fn line_index(text: &str, want: &str) -> Option<usize> {
    text.lines().position(|l| l.trim() == want)
}

// ---------- is_zo / zo_detected ----------

#[test]
fn is_zo_truth_table() {
    assert!(is_zo(true, false), "workspace and no user systemd is Zo");
    assert!(!is_zo(true, true), "workspace but user systemd usable is not Zo");
    assert!(!is_zo(false, false), "no workspace is not Zo");
    assert!(!is_zo(false, true), "no workspace is not Zo");
}

#[test]
fn zo_detected_is_false_without_home_workspace() {
    let d: fn() -> bool = zo_detected;
    if !Path::new("/home/workspace").is_dir() {
        assert!(!d(), "without /home/workspace this is not Zo");
    } else {
        // only consistent with is_zo: workspace=true, so it is Zo iff user systemd is unusable
        let _ = d();
    }
}

// ---------- zo_instructions ----------

#[test]
fn zo_instructions_has_each_field_on_its_own_line() {
    let t = zo_instructions(Path::new(BIN), &[]);
    for want in ["label: hilo", "mode: process", &format!("entrypoint: {BIN} daemon"), "workdir: /home/workspace"] {
        assert!(has_line(&t, want), "missing line {want:?} in:\n{t}");
    }
}

#[test]
fn zo_instructions_entrypoint_uses_bin_display() {
    let bin = Path::new("/somewhere/else/hilo-9");
    let t = zo_instructions(bin, &[]);
    assert!(has_line(&t, &format!("entrypoint: {} daemon", bin.display())), "{t}");
}

#[test]
fn zo_instructions_lists_envs_in_order() {
    let envs = vec![
        ("HILO_HOME".to_string(), "/home/workspace/.hilo-x".to_string()),
        ("HILO_KEYCHAIN".to_string(), "0".to_string()),
        ("HILO_NOTIFY".to_string(), "off".to_string()),
    ];
    let t = zo_instructions(Path::new(BIN), &envs);
    let idx: Vec<usize> = envs.iter().map(|(k, v)| line_index(&t, &format!("{k}={v}")).unwrap_or_else(|| panic!("missing {k}={v} in:\n{t}"))).collect();
    assert!(idx.windows(2).all(|w| w[0] < w[1]), "envs out of order: {idx:?}\n{t}");

    // the order is the one received, not a fixed one
    let rev: Vec<(String, String)> = envs.iter().rev().cloned().collect();
    let t = zo_instructions(Path::new(BIN), &rev);
    let idx: Vec<usize> = rev.iter().map(|(k, v)| line_index(&t, &format!("{k}={v}")).unwrap()).collect();
    assert!(idx.windows(2).all(|w| w[0] < w[1]), "envs out of order: {idx:?}\n{t}");
}

#[test]
fn zo_instructions_does_not_name_absent_keys() {
    let t = zo_instructions(Path::new(BIN), &[("HILO_HOME".to_string(), "/x/y".to_string())]);
    assert!(has_line(&t, "HILO_HOME=/x/y"), "{t}");
    assert!(!t.contains("HILO_KEYCHAIN"), "{t}");
    assert!(!t.contains("HILO_NOTIFY"), "{t}");

    let t = zo_instructions(Path::new(BIN), &[]);
    for k in ["HILO_HOME", "HILO_KEYCHAIN", "HILO_NOTIFY"] {
        assert!(!t.contains(k), "{k} named with no envs:\n{t}");
    }
}

// ---------- linux_service on Zo ----------

#[test]
fn zo_add_writes_no_unit_runs_nothing_starts_once() {
    let home = tmp_home();
    let (out, calls, starts) = call(&home, true, true, |_, _| true, Ok(()));
    assert!(!unit_path(&home).exists(), "Zo must not get a systemd unit");
    assert!(calls.is_empty(), "Zo must not run systemctl/loginctl: {calls:?}");
    assert_eq!(starts, 1, "start must be called exactly once");
    let out = out.expect("Zo returns the instructions");
    let want = zo_instructions(Path::new(BIN), &env_pairs());
    assert!(out.contains(&want), "output lacks zo_instructions(bin, envs):\n--- got\n{out}\n--- want\n{want}");
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn zo_add_carries_the_defined_env_keys() {
    let home = tmp_home();
    let (out, _, _) = call(&home, true, true, |_, _| true, Ok(()));
    let out = out.unwrap();
    for k in ["HILO_HOME", "HILO_KEYCHAIN", "HILO_NOTIFY"] {
        match std::env::var(k) {
            Ok(v) => assert!(has_line(&out, &format!("{k}={v}")), "missing {k}={v}:\n{out}"),
            Err(_) => assert!(!out.contains(k), "{k} is not set but is named:\n{out}"),
        }
    }
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn zo_add_start_failure_is_reported_with_hilo_up() {
    let home = tmp_home();
    let err = "could not spawn: e-7f3a";
    let (out, calls, starts) = call(&home, true, true, |_, _| true, Err(err.to_string()));
    assert!(calls.is_empty(), "{calls:?}");
    assert_eq!(starts, 1);
    assert!(!unit_path(&home).exists());
    let out = out.expect("still returns the instructions");
    assert!(out.contains(&zo_instructions(Path::new(BIN), &env_pairs())), "{out}");
    assert!(out.contains(err), "start's error missing:\n{out}");
    assert!(out.contains("hilo up"), "`hilo up` hint missing:\n{out}");
    std::fs::remove_dir_all(&home).unwrap();
}

// ---------- linux_service off Zo: today's systemd behaviour ----------

fn expected_unit() -> String {
    let env: String = env_pairs().iter().map(|(k, v)| format!("Environment={k}={v}\n")).collect();
    format!(
        "[Unit]\nDescription=hilo: same code on all your machines\nAfter=network-online.target\n\n[Service]\nExecStart={} daemon\nRestart=always\nRestartSec=2\n{env}\n[Install]\nWantedBy=default.target\n",
        sh_quote(Path::new(BIN))
    )
}

fn user() -> String {
    std::env::var("USER").unwrap_or_default()
}

#[test]
fn systemd_add_writes_unit_and_enables_it() {
    let home = tmp_home();
    let (out, calls, starts) = call(&home, true, false, |_, _| true, Ok(()));
    let p = unit_path(&home);
    assert_eq!(std::fs::read_to_string(&p).unwrap(), expected_unit());
    let u = user();
    assert_eq!(
        calls,
        vec![cmd("systemctl", &["--user", "daemon-reload"]), cmd("systemctl", &["--user", "enable", "--now", "hilo.service"]), cmd("loginctl", &["enable-linger", &u])]
    );
    assert_eq!(starts, 0, "systemd path must not call start");
    assert_eq!(out, Some(format!("systemd user service {} (runs without a login session)", p.display())));
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn systemd_add_without_linger() {
    let home = tmp_home();
    let (out, calls, starts) = call(&home, true, false, |c, _| c != "loginctl", Ok(()));
    let p = unit_path(&home);
    let u = user();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(starts, 0);
    assert_eq!(
        out,
        Some(format!("systemd user service {} (enable lingering to keep it running after logout: `sudo loginctl enable-linger {u}`)", p.display()))
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn systemd_add_systemctl_failure() {
    let home = tmp_home();
    let (out, calls, starts) = call(&home, true, false, |c, a| !(c == "systemctl" && a.contains(&"daemon-reload")), Ok(()));
    let p = unit_path(&home);
    let u = user();
    // daemon-reload fails, so enable is skipped (&&) but linger is still attempted
    assert_eq!(calls, vec![cmd("systemctl", &["--user", "daemon-reload"]), cmd("loginctl", &["enable-linger", &u])]);
    assert_eq!(starts, 0);
    assert_eq!(std::fs::read_to_string(&p).unwrap(), expected_unit());
    assert_eq!(out, Some(format!("wrote {} but systemctl failed; run `hilo daemon` yourself", p.display())));
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn systemd_add_enable_failure() {
    let home = tmp_home();
    let (out, calls, _) = call(&home, true, false, |c, a| !(c == "systemctl" && a.contains(&"enable")), Ok(()));
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(out, Some(format!("wrote {} but systemctl failed; run `hilo daemon` yourself", unit_path(&home).display())));
    std::fs::remove_dir_all(&home).unwrap();
}

// ---------- linux_service removal (add=false), Zo or not ----------

#[test]
fn remove_without_unit_is_none_and_runs_nothing() {
    for zo in [true, false] {
        let home = tmp_home();
        let (out, calls, starts) = call(&home, false, zo, |_, _| true, Ok(()));
        assert_eq!(out, None, "zo={zo}");
        assert!(calls.is_empty(), "zo={zo}: {calls:?}");
        assert_eq!(starts, 0, "zo={zo}");
        assert!(!unit_path(&home).exists());
        std::fs::remove_dir_all(&home).unwrap();
    }
}

#[test]
fn remove_with_unit_disables_and_deletes_it() {
    for zo in [true, false] {
        let home = tmp_home();
        let p = unit_path(&home);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "[Unit]\n").unwrap();
        let (out, calls, starts) = call(&home, false, zo, |_, _| true, Ok(()));
        assert_eq!(calls, vec![cmd("systemctl", &["--user", "disable", "--now", "hilo.service"])], "zo={zo}");
        assert_eq!(starts, 0, "zo={zo}");
        assert!(!p.exists(), "zo={zo}: unit not removed");
        assert_eq!(out, Some(format!("removed {}", p.display())), "zo={zo}");
        std::fs::remove_dir_all(&home).unwrap();
    }
}

// ---------- reuse of main.rs's daemon starters ----------

#[test]
fn daemon_starters_keep_their_signatures() {
    // compile-time: service() reuses these; never called here (would launch the test binary)
    let _: fn() -> Result<(), String> = crate::spawn_daemon;
    let _: fn() -> bool = crate::daemon_up;
}
