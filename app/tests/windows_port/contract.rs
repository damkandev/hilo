// The contract of main.rs's platform functions: each takes the platform as a parameter, so both
// platforms are tested on whichever one runs the tests.
//
// These functions are private to the binary, so this file is not a test target of its own:
// tests/windows_port.rs appends it to a copy of main.rs as `#[cfg(test)] mod hilo_contract { … }`,
// runs it with a nested `cargo test` and reports every test here under the same name.
use super::*;
use std::path::{Path, PathBuf};

const BOTH: [Os; 2] = [Os::Mac, Os::Windows];

/// An environment with just these variables.
fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |k| vars.iter().find(|(name, _)| *name == k).map(|(_, v)| v.to_string())
}

/// Where install.sh puts the CLI.
fn local_bin(home: &str, os: Os) -> PathBuf {
    Path::new(home).join(".local").join("bin").join(bin_name(os))
}

// ---------- Os, OS ----------

#[test]
fn os_is_a_plain_copy_enum() {
    fn derives<T: Clone + Copy + std::fmt::Debug + PartialEq + Eq>(_: T) {}
    derives(Os::Mac);
    derives(Os::Windows);
    assert_ne!(Os::Mac, Os::Windows);
    assert_eq!(format!("{:?} {:?}", Os::Mac, Os::Windows), "Mac Windows");
}

#[test]
fn os_const_is_the_platform_it_was_built_for() {
    const BUILT_FOR: Os = OS;
    assert_eq!(BUILT_FOR, if cfg!(windows) { Os::Windows } else { Os::Mac });
}

// ---------- home_in, home ----------

#[test]
fn home_windows_falls_back_to_userprofile() {
    assert_eq!(home_in(Os::Windows, |k| (k == "USERPROFILE").then(|| r"C:\Users\ana".to_string())), PathBuf::from(r"C:\Users\ana"));
    assert_eq!(home_in(Os::Windows, env(&[("USERPROFILE", r"C:\Users\ana")])), PathBuf::from(r"C:\Users\ana"));
}

#[test]
fn home_windows_prefers_home_over_userprofile() {
    assert_eq!(home_in(Os::Windows, |k| Some(format!("/{k}"))), PathBuf::from("/HOME"));
    assert_eq!(home_in(Os::Windows, env(&[("USERPROFILE", r"C:\Users\ana"), ("HOME", "/c/Users/ana")])), PathBuf::from("/c/Users/ana"));
}

#[test]
fn home_mac_uses_home() {
    assert_eq!(home_in(Os::Mac, env(&[("HOME", "/Users/ana")])), PathBuf::from("/Users/ana"));
    assert_eq!(home_in(Os::Mac, |k| Some(format!("/{k}"))), PathBuf::from("/HOME"));
}

#[test]
fn home_mac_ignores_userprofile() {
    assert_eq!(home_in(Os::Mac, |k| (k == "USERPROFILE").then(|| "x".to_string())), PathBuf::new());
    assert_eq!(home_in(Os::Mac, env(&[("USERPROFILE", r"C:\Users\ana")])), PathBuf::new());
}

#[test]
fn home_is_empty_without_variables() {
    for os in BOTH {
        assert_eq!(home_in(os, |_| None), PathBuf::new(), "{os:?}");
    }
}

#[test]
fn home_reads_no_other_variable() {
    for os in BOTH {
        let others = |k: &str| (k != "HOME" && k != "USERPROFILE").then(|| "/elsewhere".to_string());
        assert_eq!(home_in(os, others), PathBuf::new(), "{os:?}");
    }
}

#[test]
fn home_reads_the_real_environment() {
    assert_eq!(home(), home_in(OS, |k| std::env::var(k).ok()));
    // on macOS nothing changes: still $HOME, or empty
    #[cfg(not(windows))]
    assert_eq!(home(), PathBuf::from(std::env::var("HOME").unwrap_or_default()));
}

// ---------- bin_name, find_hilo_in, find_hilo ----------

#[test]
fn bin_name_ends_in_exe_only_on_windows() {
    let (win, mac): (&'static str, &'static str) = (bin_name(Os::Windows), bin_name(Os::Mac));
    assert_eq!(win, "hilo.exe");
    assert_eq!(mac, "hilo");
}

#[test]
fn find_prefers_the_binary_next_to_the_app() {
    for os in BOTH {
        // both exist: the bundled one wins
        assert_eq!(find_hilo_in(os, Some(Path::new("/app")), Path::new("/h"), |_| true), Path::new("/app").join(bin_name(os)), "{os:?}");
        let bundled = Path::new("/app").join(bin_name(os));
        assert_eq!(find_hilo_in(os, Some(Path::new("/app")), Path::new("/h"), |p| p == bundled.as_path()), bundled, "{os:?}");
    }
}

#[test]
fn find_falls_back_to_local_bin() {
    for os in BOTH {
        let local = local_bin("/h", os);
        assert_eq!(find_hilo_in(os, Some(Path::new("/app")), Path::new("/h"), |p| p == local.as_path()), local, "{os:?}");
    }
}

#[test]
fn find_falls_back_to_the_path() {
    assert_eq!(find_hilo_in(Os::Mac, Some(Path::new("/app")), Path::new("/h"), |_| false), PathBuf::from("hilo"));
    assert_eq!(find_hilo_in(Os::Windows, Some(Path::new("/app")), Path::new("/h"), |_| false), PathBuf::from("hilo.exe"));
}

#[test]
fn find_works_without_an_exe_dir() {
    for os in BOTH {
        let local = local_bin("/h", os);
        assert_eq!(find_hilo_in(os, None, Path::new("/h"), |p| p == local.as_path()), local, "{os:?}");
        assert_eq!(find_hilo_in(os, None, Path::new("/h"), |_| true), local, "{os:?}");
        assert_eq!(find_hilo_in(os, None, Path::new("/h"), |_| false), PathBuf::from(bin_name(os)), "{os:?}");
    }
    let exe = Path::new("/h").join(".local").join("bin").join("hilo.exe");
    assert_eq!(find_hilo_in(Os::Windows, None, Path::new("/h"), |p| p == exe.as_path()), exe);
}

#[test]
fn find_looks_for_the_platform_binary_name() {
    // only the other platform's binaries are on disk: nothing is found, the PATH name comes back
    let mac = [Path::new("/app").join("hilo"), Path::new("/h").join(".local").join("bin").join("hilo")];
    assert_eq!(find_hilo_in(Os::Windows, Some(Path::new("/app")), Path::new("/h"), |p| mac.iter().any(|m| m.as_path() == p)), PathBuf::from("hilo.exe"));
    let win = [Path::new("/app").join("hilo.exe"), Path::new("/h").join(".local").join("bin").join("hilo.exe")];
    assert_eq!(find_hilo_in(Os::Mac, Some(Path::new("/app")), Path::new("/h"), |p| win.iter().any(|w| w.as_path() == p)), PathBuf::from("hilo"));
}

#[test]
fn find_hilo_looks_next_to_the_exe_then_in_home() {
    let exe = std::env::current_exe().ok();
    assert_eq!(find_hilo(), find_hilo_in(OS, exe.as_deref().and_then(Path::parent), &home(), |p| p.is_file()));
}

// ---------- reveal_label, transparent_titlebar ----------

#[test]
fn reveal_label_names_the_file_manager() {
    let (mac, win): (&'static str, &'static str) = (reveal_label(Os::Mac), reveal_label(Os::Windows));
    assert_eq!(mac, "Show in Finder");
    assert_eq!(win, "Show in Explorer");
}

#[test]
fn titlebar_is_transparent_only_on_mac() {
    assert!(transparent_titlebar(Os::Mac));
    assert!(!transparent_titlebar(Os::Windows));
}
