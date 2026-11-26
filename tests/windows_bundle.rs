//! app/bundle.ps1 builds app/dist/hilo-windows.zip: hilo-app.exe and hilo.exe, side by side.
//!
//! Two kinds of test:
//! - reading the script: what it must say (and must not), comments apart from code. These run anywhere.
//! - running the script (`runs_*`): in a throwaway repo with a fake `cargo` first on the PATH, so no
//!   real build happens. They need PowerShell (`pwsh`, or `powershell` on Windows); without one they
//!   only check the script is there and say so on stderr.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

fn read(rel: &str) -> String {
    let p = Path::new(ROOT).join(rel);
    let s = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    s.trim_start_matches('\u{feff}').replace("\r\n", "\n")
}

// ---------- reading the script ----------

/// One (code, comment) pair per line: `# …` and `<# … #>` go to the comment, the rest is code.
fn split(src: &str) -> Vec<(String, String)> {
    let mut block = false;
    src.lines()
        .map(|line| {
            let cs: Vec<char> = line.chars().collect();
            let (mut code, mut comment) = (String::new(), String::new());
            let mut quote = None;
            let mut i = 0;
            while i < cs.len() {
                let c = cs[i];
                let next = cs.get(i + 1).copied();
                if block {
                    if c == '#' && next == Some('>') {
                        block = false;
                        i += 1;
                    } else {
                        comment.push(c);
                    }
                } else if let Some(q) = quote {
                    code.push(c);
                    if c == q {
                        quote = None;
                    }
                } else if c == '<' && next == Some('#') {
                    block = true;
                    i += 1;
                } else if c == '#' && (i == 0 || cs[i - 1].is_whitespace() || "({;".contains(cs[i - 1])) {
                    comment.extend(&cs[i + 1..]);
                    break;
                } else {
                    if c == '\'' || c == '"' {
                        quote = Some(c);
                    }
                    code.push(c);
                }
                i += 1;
            }
            (code, comment)
        })
        .collect()
}

/// The code alone, one entry per line of the script. A statement continued with a backtick is
/// joined onto its first line (the lines it took stay, empty), so one statement is one entry.
fn code() -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut open: Option<usize> = None;
    for (c, _) in split(&read("app/bundle.ps1")) {
        let cont = c.trim_end().ends_with('`');
        let piece = c.trim_end().trim_end_matches('`').to_string();
        match open {
            Some(i) => {
                out[i] = format!("{} {}", out[i], piece.trim());
                out.push(String::new());
            }
            None => out.push(piece),
        }
        if !cont {
            open = None;
        } else if open.is_none() {
            open = Some(out.len() - 1);
        }
    }
    out
}

fn comments() -> String {
    split(&read("app/bundle.ps1")).into_iter().map(|(_, c)| c).collect::<Vec<_>>().join("\n")
}

fn all(code: &[String]) -> String {
    code.join("\n")
}

/// Lowercase words of a line: letters, digits and `-`.
fn words(line: &str) -> Vec<String> {
    line.to_lowercase().split(|c: char| !(c.is_alphanumeric() || c == '-')).filter(|w| !w.is_empty()).map(str::to_string).collect()
}

fn has_word(line: &str, any: &[&str]) -> bool {
    words(line).iter().any(|w| any.contains(&w.as_str()))
}

fn has(line: &str, needle: &str) -> bool {
    line.to_lowercase().contains(&needle.to_lowercase())
}

/// The line of the one statement holding every needle in `with` and none in `without`.
fn only_line(code: &[String], what: &str, with: &[&str], without: &[&str]) -> usize {
    let hits: Vec<usize> =
        (0..code.len()).filter(|&i| with.iter().all(|n| code[i].contains(n)) && !without.iter().any(|n| code[i].contains(n))).collect();
    assert_eq!(hits.len(), 1, "expected exactly one {what}, found {} in:\n{}", hits.len(), all(code));
    hits[0]
}

/// The CLI build: `cargo build --release --offline --locked`, no manifest path.
fn cli_build(code: &[String]) -> usize {
    only_line(code, "CLI build (`cargo build --release --offline --locked`)", &["--release", "--offline"], &["--manifest-path"])
}

/// The app build: `cargo build --release --locked --manifest-path <root>\app\Cargo.toml`.
fn app_build(code: &[String]) -> usize {
    only_line(code, "app build (`cargo build --release --locked --manifest-path …`)", &["--release", "--manifest-path"], &[])
}

fn compress(code: &[String]) -> usize {
    let hits: Vec<usize> = (0..code.len()).filter(|&i| has(&code[i], "Compress-Archive")).collect();
    assert_eq!(hits.len(), 1, "expected exactly one Compress-Archive, found {}", hits.len());
    hits[0]
}

fn fails(line: &str) -> bool {
    has_word(line, &["throw", "write-error"])
}

#[test]
fn starts_with_a_usage_comment() {
    let lines = split(&read("app/bundle.ps1"));
    let header: Vec<&str> = lines.iter().take_while(|(code, _)| code.trim().is_empty()).map(|(_, c)| c.as_str()).collect();
    let text = header.join("\n");
    assert!(!text.trim().is_empty(), "app/bundle.ps1 must start with a usage comment, like app/bundle.sh");
    assert!(text.contains("bundle.ps1"), "the opening comment must show how to run it (`app\\bundle.ps1`):\n{text}");
    assert!(has(&text, "hilo"), "the opening comment must say what it builds:\n{text}");
}

#[test]
fn sets_error_action_preference_to_stop_before_anything_runs() {
    let code = code();
    let at = code.iter().position(|l| l.contains("$ErrorActionPreference = 'Stop'")).expect("no `$ErrorActionPreference = 'Stop'` in the code");
    assert!(at < cli_build(&code) && at < app_build(&code), "`$ErrorActionPreference = 'Stop'` must come before the builds");
}

#[test]
fn takes_the_repo_root_from_psscriptroot() {
    let code = code();
    assert!(has(&all(&code), "$PSScriptRoot"), "the repo root must come from `$PSScriptRoot\\..`, not from the current directory");
}

#[test]
fn builds_the_cli_offline_and_locked_like_bundle_sh() {
    let code = code();
    let line = &code[cli_build(&code)];
    for w in ["build", "--release", "--offline", "--locked"] {
        assert!(has_word(line, &[w]), "CLI build lacks `{w}`: {line}");
    }
    assert!(code.iter().any(|l| has_word(l, &["cargo"])), "nothing calls cargo");
}

#[test]
fn builds_the_app_locked_by_manifest_path_and_not_offline() {
    let code = code();
    let line = &code[app_build(&code)];
    for w in ["build", "--release", "--locked", "--manifest-path"] {
        assert!(has_word(line, &[w]), "app build lacks `{w}`: {line}");
    }
    // gpui's dependencies are not vendored
    assert!(!has(line, "--offline"), "the app build must not be --offline: {line}");
    assert!(all(&code).contains("Cargo.toml"), "the manifest path must be <root>\\app\\Cargo.toml");
}

/// The CLI builds in the repo root, the app from outside it: each build has its own change of
/// directory before it, unless a function defined before the builds does the moving for both.
#[test]
fn the_app_builds_from_another_directory_than_the_cli() {
    const MOVES: [&str; 9] = ["set-location", "push-location", "pop-location", "cd", "sl", "chdir", "pushd", "popd", "-workingdirectory"];
    let code = code();
    let (cli, app) = (cli_build(&code), app_build(&code));
    let first = cli.min(app);
    if code[..first].iter().any(|l| has_word(l, &["function"])) && code[..first].iter().any(|l| has_word(l, &MOVES)) {
        return;
    }
    let last_move = |build: usize| (0..=build).rev().find(|&i| has_word(&code[i], &MOVES));
    let (cli, app) = (last_move(cli), last_move(app));
    assert!(cli.is_some(), "the CLI must build in the repo root wherever the script is called from: no Set-Location before it");
    assert!(app.is_some(), "the app must build from outside the repo: no Set-Location before it");
    assert_ne!(cli, app, "the app must not build in the same directory as the CLI (the repo's cargo config would apply)");
}

#[test]
fn a_comment_explains_why_the_app_builds_outside_the_repo() {
    let c = comments().to_lowercase();
    assert!(c.contains("config"), "a comment must say the app builds outside the repo's cargo config:\n{c}");
    assert!(c.contains("vendor"), "a comment must say that config swaps crates.io for ../vendor:\n{c}");
}

/// `$LASTEXITCODE` is checked after each cargo: right after it, or in a function defined before
/// the builds that both go through.
#[test]
fn checks_lastexitcode_after_each_cargo() {
    let code = code();
    let text = all(&code);
    assert!(text.contains("$LASTEXITCODE"), "no `$LASTEXITCODE` check");
    assert!(code.iter().any(|l| fails(l)), "a failed cargo must end the script with `throw` or `Write-Error`");
    let mut builds = [cli_build(&code), app_build(&code)];
    builds.sort();
    let helper = code[..builds[0]].iter().any(|l| has_word(l, &["function"])) && all(&code[..builds[0]]).contains("$LASTEXITCODE");
    for (n, &b) in builds.iter().enumerate() {
        let end = if n == 0 { builds[1] } else { code.len() };
        assert!(helper || all(&code[b..end]).contains("$LASTEXITCODE"), "no `$LASTEXITCODE` check after: {}", code[b].trim());
    }
}

#[test]
fn a_failed_build_names_its_step() {
    let text = all(&code());
    assert!(text.contains("building the hilo CLI"), "the CLI build's failure must read \"building the hilo CLI failed\"");
    assert!(text.contains("building hilo-app"), "the app build's failure must read \"building hilo-app failed\"");
    assert!(text.contains("failed"), "the failure messages must end in \"failed\"");
}

#[test]
fn zips_both_executables_with_compress_archive_after_building() {
    let code = code();
    let text = all(&code);
    let zip = compress(&code);
    assert!(zip > cli_build(&code) && zip > app_build(&code), "Compress-Archive must come after both builds");
    assert!(!has(&code[zip], "-Update"), "Compress-Archive -Update would keep what an old zip had: {}", code[zip]);
    for lit in ["hilo-windows.zip", "hilo.exe", "hilo-app.exe", "release", "dist"] {
        assert!(text.contains(lit), "the code never mentions `{lit}`");
    }
}

#[test]
fn removes_an_old_zip_before_compressing() {
    let code = code();
    let zip = compress(&code);
    let removes = code[..zip].iter().any(|l| has_word(l, &["remove-item", "del", "erase", "rm", "ri", "rmdir", "rd", "delete"]));
    assert!(removes || has(&code[zip], "-Force"), "an existing app\\dist\\hilo-windows.zip must be removed before Compress-Archive");
}

#[test]
fn checks_both_executables_exist_before_compressing() {
    let code = code();
    let before = &code[..compress(&code)];
    let removes = |l: &str| has_word(l, &["remove-item", "del", "erase", "rm", "ri", "delete"]);
    // Get-Item and Resolve-Path fail by themselves, naming the path, on a missing file
    let fails_alone = before.iter().any(|l| (has(l, "Get-Item") || has(l, "Resolve-Path")) && !has(l, "$PSScriptRoot"));
    // not the `if (Test-Path $zip) { Remove-Item $zip }` of the old zip
    let asks = before.iter().any(|l| (has(l, "Test-Path") || has(l, "Exists(")) && !removes(l));
    assert!(fails_alone || asks, "no check that the two .exe exist before Compress-Archive");
    // and a way to fail that is not a build's
    let own = code.iter().any(|l| fails(l) && !l.contains("building"));
    assert!(fails_alone || own, "a missing .exe must fail with a message of its own that names it");
}

#[test]
fn prints_the_zip_path_last() {
    let code = code();
    let after = &code[compress(&code) + 1..];
    assert!(after.iter().any(|l| l.contains('$') || l.contains("hilo-windows.zip")), "the script must end by printing the zip's path, after Compress-Archive");
}

#[test]
fn makes_no_installer_and_does_not_sign_or_publish() {
    let text = all(&code()).to_lowercase();
    for no in [".msi", "msix", "msiexec", "wix", "makeappx", "appxpackage", "signtool", "authenticode", "gh release", "upload"] {
        assert!(!text.contains(no), "out of scope (no MSI/MSIX, no signing, no release): `{no}`");
    }
}

// ---------- what this unit leaves alone ----------

/// The app keeps its two dependencies: an icon may only come without new ones.
#[test]
fn app_manifest_gains_no_dependencies() {
    let toml = read("app/Cargo.toml");
    let mut section = String::new();
    let mut deps = vec![];
    for l in toml.lines().map(str::trim) {
        if l.starts_with('[') {
            section = l.to_string();
            assert!(!section.contains("dependencies") || section == "[dependencies]", "app/Cargo.toml: new dependency table {section}");
        } else if section == "[dependencies]" && !l.is_empty() && !l.starts_with('#') {
            deps.push(l.split('=').next().unwrap().trim().to_string());
        }
    }
    assert_eq!(deps, ["gpui", "serde_json"], "app/Cargo.toml: dependencies changed");
}

#[test]
fn app_lockfile_has_the_same_packages() {
    let lock = read("app/Cargo.lock");
    assert_eq!(lock.matches("[[package]]").count(), 709, "app/Cargo.lock changed: this unit adds no packages");
    // gpui already brings resource compilers: none of them becomes a dependency of the app itself
    let app = lock.split("[[package]]").find(|p| p.contains("name = \"hilo-app\"")).expect("hilo-app in app/Cargo.lock");
    let deps: Vec<&str> = app.split("dependencies = [").nth(1).unwrap_or("").split(']').next().unwrap().split(',').map(|d| d.trim().trim_matches('"')).filter(|d| !d.is_empty()).collect();
    assert_eq!(deps, ["gpui", "serde_json"], "app/Cargo.lock: hilo-app's dependencies changed");
}

#[test]
fn bundle_sh_is_still_the_macos_script() {
    let sh = read("app/bundle.sh");
    assert!(sh.starts_with("#!/usr/bin/env bash\n# Builds hilo.app (the app plus the hilo CLI inside it) and a .dmg to drag it to Applications."));
    for lit in [
        "cargo build --release --offline --locked",
        r#"(cd / && cargo build --release --locked --manifest-path "$ROOT/app/Cargo.toml")"#,
        "hilo-macos.dmg",
        r#"echo "$OUT/hilo-macos.dmg""#,
    ] {
        assert!(sh.contains(lit), "app/bundle.sh lost `{lit}`");
    }
    assert!(sh.trim_end().ends_with(r#"echo "$OUT/hilo-macos.dmg""#), "app/bundle.sh has something new at the end");
    for no in [".exe", ".ps1", "windows", "Windows", ".zip"] {
        assert!(!sh.contains(no), "app/bundle.sh must stay untouched: found `{no}`");
    }
}

/// Not built in CI and not published: no workflow knows about the zip or the script.
#[test]
fn workflows_do_not_know_about_the_windows_bundle() {
    let dir = Path::new(ROOT).join(".github").join("workflows");
    let mut seen = 0;
    for e in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())).flatten() {
        let text = std::fs::read_to_string(e.path()).unwrap_or_default();
        seen += 1;
        for no in ["bundle.ps1", "hilo-windows", "hilo-app.exe"] {
            assert!(!text.contains(no), "{}: `{no}` (the zip is not built or published by CI)", e.path().display());
        }
    }
    assert!(seen >= 2, ".github/workflows lost a workflow");
    assert!(read("app/bundle.ps1").contains("hilo-windows.zip")); // and the script is there for the check to mean something
}

// ---------- running the script ----------

/// Stands in for cargo: logs each `build` (step, directory, manifest, arguments), writes to stderr
/// as cargo does, and leaves the .exe where cargo would, unless told to fail or to leave it out.
const FAKE_CARGO: &str = r#"
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("build") {
        println!("cargo 1.0.0 (fake)");
        return;
    }
    let mut manifest = None;
    for (i, a) in args.iter().enumerate() {
        if let Some(v) = a.strip_prefix("--manifest-path=") {
            manifest = Some(v.to_string());
        } else if a == "--manifest-path" {
            manifest = args.get(i + 1).cloned();
        }
    }
    // PowerShell on macOS and Linux hands `\` to programs as it is
    let manifest = manifest.map(|m| PathBuf::from(if cfg!(windows) { m } else { m.replace('\\', "/") }));
    let manifest = manifest.map(|m| std::fs::canonicalize(&m).unwrap_or(m));
    let step = if manifest.is_some() { "app" } else { "cli" };
    let cwd = std::env::current_dir().unwrap();
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(var("FAKE_CARGO_LOG")).unwrap();
    let shown = manifest.as_ref().map(|m| m.display().to_string()).unwrap_or("-".into());
    writeln!(log, "{step}\t{}\t{shown}\t{}", cwd.display(), args.join(" ")).unwrap();
    eprintln!("   Compiling hilo v0.0.0 (fake)");
    if var("FAKE_CARGO_FAIL") == step {
        eprintln!("error: could not compile (fake)");
        std::process::exit(101);
    }
    if var("FAKE_CARGO_OMIT") != step {
        let (dir, name, bytes): (PathBuf, &str, &[u8]) = match &manifest {
            Some(m) => (m.parent().unwrap().to_path_buf(), "hilo-app.exe", b"fake hilo-app, the longer one\n"),
            None => (cwd, "hilo.exe", b"fake hilo\n"),
        };
        let out = dir.join("target").join("release");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join(name), bytes).unwrap();
    }
    eprintln!("    Finished `release` profile (fake)");
}
"#;
const CLI_EXE_LEN: u32 = 10;
const APP_EXE_LEN: u32 = 30;

fn tmp() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("windows_bundle")
}

/// `pwsh`, or `powershell` (Windows PowerShell); None where neither runs.
fn powershell() -> Option<&'static str> {
    static FOUND: OnceLock<Option<&'static str>> = OnceLock::new();
    *FOUND.get_or_init(|| {
        ["pwsh", "powershell"].into_iter().find(|p| {
            Command::new(p).args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "exit 0"]).output().map(|o| o.status.success()).unwrap_or(false)
        })
    })
}

/// The directory holding the fake `cargo`, compiled once.
fn fake_cargo() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tmp().join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("fake_cargo.rs");
        std::fs::write(&src, FAKE_CARGO).unwrap();
        let exe = dir.join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
        let o = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition", "2021", "-o"])
            .arg(&exe)
            .arg(&src)
            .output()
            .expect("rustc runs");
        assert!(o.status.success(), "the fake cargo doesn't compile:\n{}", String::from_utf8_lossy(&o.stderr));
        dir
    })
}

/// A throwaway repo with the real app/bundle.ps1 in it, and a directory elsewhere to call it from.
struct Lab {
    repo: PathBuf,
    elsewhere: PathBuf,
    log: PathBuf,
}

struct Outcome {
    ok: bool,
    stdout: String,
    /// stdout and stderr
    all: String,
    /// one (step, directory, manifest, arguments) per `cargo build`
    builds: Vec<[String; 4]>,
}

impl Lab {
    /// None without PowerShell. The script is read first: it has to exist either way.
    fn new(name: &str) -> Option<Lab> {
        let script = read("app/bundle.ps1");
        if powershell().is_none() {
            eprintln!("{name}: skipped, no pwsh or powershell on the PATH to run app/bundle.ps1");
            return None;
        }
        let dir = tmp().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        let lab = Lab { repo: dir.join("repo"), elsewhere: dir.join("elsewhere"), log: dir.join("cargo.log") };
        std::fs::create_dir_all(lab.repo.join("app")).unwrap();
        std::fs::create_dir_all(&lab.elsewhere).unwrap();
        std::fs::write(lab.repo.join("app").join("bundle.ps1"), script).unwrap();
        for f in ["Cargo.toml", "Cargo.lock", "app/Cargo.toml", "app/Cargo.lock"] {
            std::fs::copy(Path::new(ROOT).join(f), lab.repo.join(f)).unwrap();
        }
        // canonical, to compare with what the fake cargo saw
        Some(Lab { repo: lab.repo.canonicalize().unwrap(), elsewhere: lab.elsewhere.canonicalize().unwrap(), log: lab.log })
    }

    fn zip(&self) -> PathBuf {
        self.repo.join("app").join("dist").join("hilo-windows.zip")
    }

    /// What a build leaves behind, for the runs where the fake cargo fails before writing it.
    fn leave_old_executables(&self) {
        for (dir, name) in [(self.repo.clone(), "hilo.exe"), (self.repo.join("app"), "hilo-app.exe")] {
            let out = dir.join("target").join("release");
            std::fs::create_dir_all(&out).unwrap();
            std::fs::write(out.join(name), b"old").unwrap();
        }
    }

    /// `pwsh -File <repo>/app/bundle.ps1`, from another directory, with the fake cargo first on the PATH.
    fn run(&self, env: &[(&str, &str)]) -> Outcome {
        let ps = powershell().unwrap();
        let path = std::env::join_paths(std::iter::once(fake_cargo().to_path_buf()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()))).unwrap();
        let mut c = Command::new(ps);
        c.args(["-NoLogo", "-NoProfile", "-NonInteractive"]);
        if cfg!(windows) {
            c.args(["-ExecutionPolicy", "Bypass"]);
        }
        c.arg("-File").arg(self.repo.join("app").join("bundle.ps1"));
        c.current_dir(&self.elsewhere).env("PATH", path).env("FAKE_CARGO_LOG", &self.log);
        c.env("NO_COLOR", "1").env("POWERSHELL_TELEMETRY_OPTOUT", "1").env("POWERSHELL_UPDATECHECK", "Off");
        c.env_remove("CARGO_TARGET_DIR").env_remove("CARGO_BUILD_TARGET");
        // what Windows always has, for a PowerShell on macOS or Linux
        for (k, v) in [("TEMP", std::env::temp_dir()), ("TMP", std::env::temp_dir()), ("USERPROFILE", std::env::temp_dir()), ("SystemRoot", "/".into())] {
            if std::env::var_os(k).is_none() {
                c.env(k, v);
            }
        }
        c.envs(env.iter().copied());
        let o = c.output().expect("powershell runs");
        let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
        let all = format!("{stdout}\n{}", String::from_utf8_lossy(&o.stderr));
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        let builds = log.lines().map(|l| l.split('\t').map(str::to_string).collect::<Vec<_>>().try_into().expect("four fields")).collect();
        Outcome { ok: o.status.success(), stdout, all, builds }
    }
}

/// PowerShell wraps its errors to the console's width, mid-word in Windows PowerShell: compare
/// without whitespace, colors or the `|` gutter.
fn says(output: &str, needle: &str) -> bool {
    fn squeeze(s: &str) -> String {
        let mut out = String::new();
        let mut cs = s.chars();
        while let Some(c) = cs.next() {
            if c == '\u{1b}' {
                for c in cs.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else if !c.is_whitespace() && c != '|' {
                out.push(c);
            }
        }
        out
    }
    squeeze(output).contains(&squeeze(needle))
}

/// (name, uncompressed size) of each entry, from the zip's central directory.
fn zip_entries(path: &Path) -> Vec<(String, u32)> {
    let b = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as usize;
    let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let end = (0..b.len().saturating_sub(21)).rev().find(|&i| b[i..i + 4] == *b"PK\x05\x06").unwrap_or_else(|| panic!("{} is not a zip", path.display()));
    let mut at = u32_at(end + 16) as usize;
    (0..u16_at(end + 10))
        .map(|_| {
            assert_eq!(b[at..at + 4], *b"PK\x01\x02", "bad central directory in {}", path.display());
            let (name, extra, comment) = (u16_at(at + 28), u16_at(at + 30), u16_at(at + 32));
            let entry = (String::from_utf8_lossy(&b[at + 46..at + 46 + name]).into_owned(), u32_at(at + 24));
            at += 46 + name + extra + comment;
            entry
        })
        .collect()
}

fn assert_zip_is_the_two_executables(lab: &Lab) {
    let mut entries = zip_entries(&lab.zip());
    entries.sort();
    assert_eq!(entries, [("hilo-app.exe".to_string(), APP_EXE_LEN), ("hilo.exe".to_string(), CLI_EXE_LEN)], "hilo-windows.zip must hold exactly hilo-app.exe and hilo.exe at its root");
}

#[test]
fn runs_from_any_directory_and_zips_both_executables_at_the_root() {
    let Some(lab) = Lab::new("ok") else { return };
    let o = lab.run(&[]);
    assert!(o.ok, "app/bundle.ps1 failed:\n{}", o.all);
    assert!(lab.zip().is_file(), "no app/dist/hilo-windows.zip:\n{}", o.all);
    assert_zip_is_the_two_executables(&lab);
    assert!(!lab.elsewhere.join("hilo-windows.zip").exists() && !lab.elsewhere.join("app").exists(), "the script wrote where it was called from, not in the repo");
}

#[test]
fn runs_and_prints_the_zip_path_last() {
    let Some(lab) = Lab::new("prints") else { return };
    let o = lab.run(&[]);
    assert!(o.ok, "app/bundle.ps1 failed:\n{}", o.all);
    let last = o.stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or_default().trim().replace('\\', "/");
    assert!(last.ends_with("app/dist/hilo-windows.zip"), "the last line printed must be the zip's path, got `{last}`:\n{}", o.all);
}

#[test]
fn runs_the_cli_build_in_the_repo_root_offline_and_locked() {
    let Some(lab) = Lab::new("cli") else { return };
    let o = lab.run(&[]);
    assert!(o.ok, "app/bundle.ps1 failed:\n{}", o.all);
    let cli: Vec<&[String; 4]> = o.builds.iter().filter(|b| b[0] == "cli").collect();
    assert_eq!(cli.len(), 1, "expected one CLI build: {:?}", o.builds);
    assert_eq!(Path::new(&cli[0][1]), lab.repo, "the CLI must build in the repo root");
    let mut args: Vec<&str> = cli[0][3].split(' ').collect();
    args.sort();
    assert_eq!(args, ["--locked", "--offline", "--release", "build"], "the CLI builds with `cargo build --release --offline --locked`");
}

#[test]
fn runs_the_app_build_by_manifest_path_from_outside_the_repo() {
    let Some(lab) = Lab::new("app") else { return };
    let o = lab.run(&[]);
    assert!(o.ok, "app/bundle.ps1 failed:\n{}", o.all);
    let app: Vec<&[String; 4]> = o.builds.iter().filter(|b| b[0] == "app").collect();
    assert_eq!(app.len(), 1, "expected one app build: {:?}", o.builds);
    let [_, cwd, manifest, args] = app[0];
    assert!(!Path::new(cwd).starts_with(&lab.repo), "the app must build from outside the repo (its cargo config swaps crates.io for vendor/), not from {cwd}");
    assert_eq!(Path::new(manifest), lab.repo.join("app").join("Cargo.toml"), "--manifest-path must be <root>\\app\\Cargo.toml");
    let args: Vec<&str> = args.split(' ').collect();
    for a in ["build", "--release", "--locked"] {
        assert!(args.contains(&a), "the app build lacks `{a}`: {args:?}");
    }
    assert!(!args.contains(&"--offline"), "the app build must not be --offline: {args:?}");
}

#[test]
fn runs_and_replaces_an_old_zip() {
    let Some(lab) = Lab::new("old-zip") else { return };
    std::fs::create_dir_all(lab.zip().parent().unwrap()).unwrap();
    std::fs::write(lab.zip(), b"an old zip, not even a zip").unwrap();
    let o = lab.run(&[]);
    assert!(o.ok, "app/bundle.ps1 failed with an old app/dist/hilo-windows.zip in place:\n{}", o.all);
    assert_zip_is_the_two_executables(&lab);
}

#[test]
fn runs_and_stops_when_the_cli_build_fails() {
    let Some(lab) = Lab::new("cli-fails") else { return };
    lab.leave_old_executables();
    let o = lab.run(&[("FAKE_CARGO_FAIL", "cli")]);
    assert!(!o.ok, "cargo failed building the CLI and the script exited 0:\n{}", o.all);
    assert!(says(&o.all, "building the hilo CLI failed"), "the error must read \"building the hilo CLI failed\":\n{}", o.all);
    assert!(!lab.zip().exists(), "a zip was made after a failed build");
}

#[test]
fn runs_and_stops_when_the_app_build_fails() {
    let Some(lab) = Lab::new("app-fails") else { return };
    lab.leave_old_executables();
    let o = lab.run(&[("FAKE_CARGO_FAIL", "app")]);
    assert!(!o.ok, "cargo failed building hilo-app and the script exited 0:\n{}", o.all);
    assert!(says(&o.all, "building hilo-app failed"), "the error must read \"building hilo-app failed\":\n{}", o.all);
    assert!(!lab.zip().exists(), "a zip was made after a failed build");
}

#[test]
fn runs_and_names_hilo_exe_when_it_is_missing() {
    let Some(lab) = Lab::new("no-cli") else { return };
    let o = lab.run(&[("FAKE_CARGO_OMIT", "cli")]);
    assert!(!o.ok, "target\\release\\hilo.exe is missing and the script exited 0:\n{}", o.all);
    assert!(says(&o.all, "hilo.exe"), "the error must name hilo.exe:\n{}", o.all);
    assert!(!lab.zip().exists(), "a zip was made without hilo.exe");
}

#[test]
fn runs_and_names_hilo_app_exe_when_it_is_missing() {
    let Some(lab) = Lab::new("no-app") else { return };
    let o = lab.run(&[("FAKE_CARGO_OMIT", "app")]);
    assert!(!o.ok, "app\\target\\release\\hilo-app.exe is missing and the script exited 0:\n{}", o.all);
    assert!(says(&o.all, "hilo-app.exe"), "the error must name hilo-app.exe:\n{}", o.all);
    assert!(!lab.zip().exists(), "a zip was made without hilo-app.exe");
}
