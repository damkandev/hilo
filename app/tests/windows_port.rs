//! The app builds and works on Windows, and nothing changes on macOS.
//!
//! Run it as CI runs the app, from outside the repo (its .cargo/config.toml is for the CLI):
//! `cd / && cargo test --manifest-path <repo>/app/Cargo.toml`
//!
//! main.rs is a binary and its platform functions are private, so there are three kinds of test:
//! - the contract: `windows_port/contract.rs` is appended to a copy of main.rs as a test module and
//!   run by a nested `cargo test` on the same target dir (only the app itself is rebuilt). Each of
//!   its tests is reported here under its own name.
//! - the Windows build: a nested `cargo check --target x86_64-pc-windows-msvc`.
//! - what a Mac can't run (code behind `#[cfg]`, window options): read from main.rs itself.
use std::ops::Range;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

const APP: &str = env!("CARGO_MANIFEST_DIR");
const WINDOWS: &str = "x86_64-pc-windows-msvc";

// ---------- nested cargo ----------

/// cargo from the filesystem root, on the target dir of this run: dependencies are already built.
fn cargo() -> Command {
    let mut c = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    c.current_dir(Path::new(APP).ancestors().last().unwrap());
    c.env("CARGO_TARGET_DIR", Path::new(env!("CARGO_TARGET_TMPDIR")).parent().unwrap());
    c.env("CARGO_TERM_COLOR", "never");
    // cargo ran this test with its package's variables set. A nested cargo that inherits them takes
    // them for a change of environment and rebuilds every build script that watches one (ring's
    // does), and gpui after it.
    for (k, _) in std::env::vars_os() {
        let name = k.to_string_lossy();
        if ["CARGO_PKG_", "CARGO_MANIFEST_", "CARGO_BIN_", "CARGO_CRATE_", "CARGO_PRIMARY_"].iter().any(|p| name.starts_with(p)) {
            c.env_remove(&k);
        }
    }
    c
}

fn output(mut c: Command) -> (bool, String) {
    let o = c.output().expect("cargo runs");
    (o.status.success(), format!("{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

/// The compiler's errors, without the pages of build log around them.
fn errors(out: &str) -> String {
    out.lines().skip_while(|l| !l.starts_with("error")).take(80).collect::<Vec<_>>().join("\n")
}

/// Writes only what changed, so the nested build stays incremental.
fn put(to: &Path, bytes: &[u8]) {
    if std::fs::read(to).ok().as_deref() != Some(bytes) {
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::write(to, bytes).unwrap();
    }
}

fn mirror(from: &Path, to: &Path) {
    for e in std::fs::read_dir(from).unwrap_or_else(|e| panic!("{}: {e}", from.display())).flatten() {
        let (p, to) = (e.path(), to.join(e.file_name()));
        if p.is_dir() {
            mirror(&p, &to)
        } else {
            put(&to, &std::fs::read(&p).unwrap())
        }
    }
}

/// libtest's output for the contract, run once: a copy of the app whose main.rs ends with
/// `windows_port/contract.rs` as a test module.
fn contract_output() -> &'static str {
    static OUT: OnceLock<String> = OnceLock::new();
    OUT.get_or_init(|| {
        let (app, copy) = (Path::new(APP), Path::new(env!("CARGO_TARGET_TMPDIR")).join("hilo-app-contract"));
        for f in ["Cargo.toml", "Cargo.lock"] {
            put(&copy.join(f), &std::fs::read(app.join(f)).unwrap());
        }
        // main.rs embeds assets/metal.png
        mirror(&app.join("assets"), &copy.join("assets"));
        let main = std::fs::read_to_string(app.join("src/main.rs")).expect("app/src/main.rs");
        put(&copy.join("src/main.rs"), format!("{main}\n#[cfg(test)]\nmod hilo_contract {{\n{}\n}}\n", include_str!("windows_port/contract.rs")).as_bytes());
        let mut c = cargo();
        c.args(["test", "--locked", "--bins"]);
        if !cfg!(debug_assertions) {
            c.arg("--release");
        }
        // The copy is the same package as the app, so cargo would give both the same file in the
        // target dir and take one's test binary for the other's. A profile setting of its own (one
        // that changes nothing else) gives the copy its own.
        for profile in ["dev", "release"] {
            c.arg("--config").arg(format!("profile.{profile}.package.hilo-app.codegen-units=255"));
        }
        c.arg("--manifest-path").arg(copy.join("Cargo.toml")).args(["--", "hilo_contract::"]);
        output(c).1
    })
}

/// Passes when the contract test `name` passed against main.rs.
fn contract(name: &str) {
    let out = contract_output();
    let head = format!("test hilo_contract::{name} ... ");
    match out.lines().find_map(|l| l.strip_prefix(head.as_str())) {
        Some("ok") => {}
        Some(_) => {
            let from = format!("---- hilo_contract::{name} stdout ----");
            let why: Vec<&str> = out.lines().skip_while(|l| *l != from).skip(1).take_while(|l| !l.starts_with("---- ") && *l != "failures:").collect();
            panic!("{name} fails against app/src/main.rs:\n{}", why.join("\n"));
        }
        None => panic!("{name} did not run: app/src/main.rs does not build with tests/windows_port/contract.rs as a test module (is the contract there?):\n{}", errors(out)),
    }
}

macro_rules! contract {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                contract(stringify!($name));
            }
        )*
    };
}

contract![
    os_is_a_plain_copy_enum,
    os_const_is_the_platform_it_was_built_for,
    home_windows_falls_back_to_userprofile,
    home_windows_prefers_home_over_userprofile,
    home_mac_uses_home,
    home_mac_ignores_userprofile,
    home_is_empty_without_variables,
    home_reads_no_other_variable,
    home_reads_the_real_environment,
    bin_name_ends_in_exe_only_on_windows,
    find_prefers_the_binary_next_to_the_app,
    find_falls_back_to_local_bin,
    find_falls_back_to_the_path,
    find_works_without_an_exe_dir,
    find_looks_for_the_platform_binary_name,
    find_hilo_looks_next_to_the_exe_then_in_home,
    reveal_label_names_the_file_manager,
    titlebar_is_transparent_only_on_mac,
];

/// `cargo check --locked --target x86_64-pc-windows-msvc` passes. A machine that can't build the
/// dependencies for Windows (no target, no C SDK for a build script) can't tell: that is printed,
/// not failed, and the PR has to say so.
#[test]
fn compiles_for_windows() {
    let mut c = cargo();
    c.args(["check", "--locked", "--manifest-path"]).arg(Path::new(APP).join("Cargo.toml")).args(["--target", WINDOWS]);
    let (ok, out) = output(c);
    if ok {
        return;
    }
    // the app's own errors, or a Cargo.lock that no longer matches Cargo.toml
    let ours = out.contains("could not compile `hilo-app`") || out.contains("but --locked was passed");
    assert!(!ours, "app/ does not build for {WINDOWS}:\n{}", errors(&out));
    eprintln!("NOT VERIFIED: cargo check --target {WINDOWS} failed before reaching app/src/main.rs (toolchain or SDK, not the code):\n{}", errors(&out));
}

// ---------- reading main.rs ----------

struct Func {
    name: String,
    /// where its `fn` is
    at: usize,
    /// its braces and what is between them
    body: Range<usize>,
}

/// app/src/main.rs in two views with the same offsets as the file.
struct Src {
    raw: String,
    /// comments blanked
    code: String,
    /// comments and the contents of string and char literals blanked: every bracket in it is real
    bare: String,
    fns: Vec<Func>,
    /// every `#[cfg(…)]`: its text without whitespace, and the code it applies to
    gates: Vec<(String, Range<usize>)>,
}

fn ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Where `word` appears as a whole word.
fn words(hay: &str, word: &str) -> Vec<usize> {
    let b = hay.as_bytes();
    hay.match_indices(word).map(|(i, _)| i).filter(|&i| (i == 0 || !ident(b[i - 1])) && b.get(i + word.len()).is_none_or(|&c| !ident(c))).collect()
}

/// Without whitespace, so formatting doesn't matter.
fn tight(s: &str) -> String {
    s.split_whitespace().collect()
}

/// The end (exclusive) of the bracket opened at `open`.
fn close(b: &[u8], open: usize) -> usize {
    let mut depth = 0;
    for (i, c) in b.iter().enumerate().skip(open) {
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    b.len()
}

fn lex(raw: &str) -> (String, String) {
    let b = raw.as_bytes();
    let (mut code, mut bare) = (b.to_vec(), b.to_vec());
    fn blank(v: &mut [u8], r: Range<usize>) {
        for c in &mut v[r] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    }
    let mut i = 0;
    while i < b.len() {
        let rest = &b[i..];
        let hashes = rest[1..].iter().take_while(|&&c| c == b'#').count();
        i = if rest.starts_with(b"//") {
            let end = i + rest.iter().position(|&c| c == b'\n').unwrap_or(rest.len());
            blank(&mut code, i..end);
            blank(&mut bare, i..end);
            end
        } else if rest.starts_with(b"/*") {
            let (mut depth, mut j) = (1, i + 2);
            while j < b.len() && depth > 0 {
                (depth, j) = if b[j..].starts_with(b"/*") {
                    (depth + 1, j + 2)
                } else if b[j..].starts_with(b"*/") {
                    (depth - 1, j + 2)
                } else {
                    (depth, j + 1)
                };
            }
            blank(&mut code, i..j);
            blank(&mut bare, i..j);
            j
        } else if rest[0] == b'r' && rest.get(1 + hashes) == Some(&b'"') && (i == 0 || !ident(b[i - 1]) || b[i - 1] == b'b') {
            // r"…", r#"…"#
            let from = i + 2 + hashes;
            let end = format!("\"{}", "#".repeat(hashes));
            let to = raw[from..].find(&end).map_or(b.len(), |k| from + k);
            blank(&mut bare, from..to);
            (to + end.len()).min(b.len())
        } else if rest[0] == b'"' {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let j = j.min(b.len());
            blank(&mut bare, i + 1..j);
            j + 1
        } else if rest[0] == b'\'' && rest.get(1) == Some(&b'\\') {
            // '\n', '\''
            let j = i + 3 + b[(i + 3).min(b.len())..].iter().position(|&c| c == b'\'').unwrap_or(0);
            blank(&mut bare, i + 1..j.min(b.len()));
            j + 1
        } else if rest[0] == b'\'' {
            // 'x' is a char, 'a a lifetime
            let n = raw[i + 1..].chars().next().map_or(1, char::len_utf8);
            if b.get(i + 1 + n) == Some(&b'\'') {
                blank(&mut bare, i + 1..i + 1 + n);
                i + 2 + n
            } else {
                i + 1
            }
        } else {
            i + 1
        };
    }
    (String::from_utf8(code).unwrap(), String::from_utf8(bare).unwrap())
}

fn funcs(bare: &str) -> Vec<Func> {
    let b = bare.as_bytes();
    words(bare, "fn")
        .into_iter()
        .filter_map(|at| {
            let from = at + 2 + (bare[at + 2..].len() - bare[at + 2..].trim_start().len());
            let name: String = bare[from..].chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            // `fn(…)` is a type, and a signature ending in `;` has no body
            let mut depth = 0;
            for i in from..b.len() {
                match b[i] {
                    _ if name.is_empty() => return None,
                    b'(' | b'[' => depth += 1,
                    b')' | b']' => depth -= 1,
                    b';' if depth == 0 => return None,
                    b'{' if depth == 0 => return Some(Func { name, at, body: i..close(b, i) }),
                    _ => {}
                }
            }
            None
        })
        .collect()
}

/// Where the item, statement, field or match arm starting at `from` ends.
fn gated_end(bare: &str, from: usize) -> usize {
    let b = bare.as_bytes();
    let first: String = bare[from..].chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    // a field, a match arm or an argument ends at its comma; items and `let` have commas of their own (generics)
    let comma_ends = !["fn", "pub", "mod", "impl", "extern", "unsafe", "const", "static", "use", "struct", "enum", "trait", "type", "async", "let"].contains(&first.as_str());
    let mut depth = 0;
    for i in from..b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth < 0 {
                    return i;
                }
                // a closing brace ends it, unless the expression goes on: `} else {`, `}.x()`, `};`
                let next = bare[i + 1..].trim_start();
                if depth == 0 && b[i] == b'}' && !(next.starts_with("else") || next.starts_with(['.', '?', ';']) || (comma_ends && next.starts_with(','))) {
                    return i + 1;
                }
            }
            b';' if depth == 0 => return i + 1,
            b',' if depth == 0 && comma_ends => return i + 1,
            _ => {}
        }
    }
    b.len()
}

fn gates(code: &str, bare: &str) -> Vec<(String, Range<usize>)> {
    let b = bare.as_bytes();
    bare.match_indices("#[cfg(")
        .map(|(at, _)| {
            let mut from = close(b, at + 1);
            let attr = tight(&code[at..from]);
            // other attributes may sit between this one and what it applies to
            loop {
                from += bare[from..].len() - bare[from..].trim_start().len();
                if !bare[from..].starts_with("#[") {
                    break;
                }
                from = close(b, from + 1);
            }
            (attr, at..gated_end(bare, from))
        })
        .collect()
}

/// A `#[cfg]` that keeps code out of the Windows build.
fn not_on_windows(attr: &str) -> bool {
    (attr.contains(r#"target_os="macos""#) && !attr.contains(r#"not(target_os="macos")"#)) || attr.contains("not(windows)") || attr.contains(r#"not(target_os="windows")"#) || attr == "#[cfg(unix)]"
}

fn only_on_windows(attr: &str) -> bool {
    attr.contains("windows") && !attr.contains("not(windows)") && !attr.contains(r#"not(target_os="windows")"#)
}

fn only_in_tests(attr: &str) -> bool {
    attr.contains("test") && !attr.contains("not(test)")
}

fn src() -> &'static Src {
    static SRC: OnceLock<Src> = OnceLock::new();
    SRC.get_or_init(|| {
        let raw = std::fs::read_to_string(Path::new(APP).join("src/main.rs")).expect("app/src/main.rs");
        let (code, bare) = lex(&raw);
        let (fns, gates) = (funcs(&bare), gates(&code, &bare));
        Src { raw, code, bare, fns, gates }
    })
}

impl Src {
    fn line(&self, at: usize) -> usize {
        self.raw[..at].matches('\n').count() + 1
    }

    /// Is the code at `at` behind a `#[cfg]` of this kind?
    fn behind(&self, at: usize, kind: fn(&str) -> bool) -> bool {
        self.gates.iter().any(|(attr, r)| kind(attr) && r.contains(&at))
    }

    /// Where the app's code (not its comments, not its tests) has `word`.
    fn uses(&self, word: &str) -> Vec<usize> {
        words(&self.code, word).into_iter().filter(|&at| !self.behind(at, only_in_tests)).collect()
    }

    /// The one function called `name`, tests aside.
    fn func(&self, name: &str) -> &Func {
        let found: Vec<&Func> = self.fns.iter().filter(|f| f.name == name && !self.behind(f.at, only_in_tests)).collect();
        assert_eq!(found.len(), 1, "app/src/main.rs should define `fn {name}` once, it has {}", found.len());
        found[0]
    }

    /// The code of `fn name`, without whitespace.
    fn body(&self, name: &str) -> String {
        tight(&self.code[self.func(name).body.clone()])
    }

    /// The innermost function around `at`.
    fn around(&self, at: usize) -> Option<&Func> {
        self.fns.iter().filter(|f| f.body.contains(&at)).min_by_key(|f| f.body.len())
    }
}

#[test]
fn windows_subsystem_attribute_opens_the_file() {
    // comments (the //! doc) may come before it, code may not
    let first = src().code.lines().map(tight).find(|l| !l.is_empty()).unwrap_or_default();
    assert_eq!(first, r#"#![cfg_attr(all(windows,not(debug_assertions)),windows_subsystem="windows")]"#, "the first line of code in app/src/main.rs");
}

#[test]
fn os_is_declared_as_in_the_contract() {
    let code = tight(&src().code);
    assert!(code.contains("enumOs{Mac,Windows}") || code.contains("enumOs{Mac,Windows,}"), "app/src/main.rs has no `enum Os {{ Mac, Windows }}`");
    assert!(code.contains("constOS:Os="), "app/src/main.rs has no `const OS: Os`");
}

#[test]
fn pure_functions_take_the_platform_and_never_ask_cfg() {
    let s = src();
    for name in ["home_in", "bin_name", "find_hilo_in", "reveal_label", "transparent_titlebar"] {
        let f = s.func(name);
        assert!(words(&s.bare[f.body.clone()], "cfg").is_empty(), "`{name}` asks cfg: it must decide from its `os` parameter alone");
        assert!(!s.gates.iter().any(|(_, r)| r.contains(&f.at)), "`{name}` is behind a #[cfg]: there must be one definition, for every platform");
        assert!(s.bare[f.at..f.body.start].contains("Os"), "`{name}` does not take an `Os`");
    }
}

#[test]
fn home_and_find_hilo_pass_the_platform_const() {
    let s = src();
    assert!(s.body("home").contains("home_in(OS,"), "`home()` should be `home_in(OS, |k| std::env::var(k).ok())`");
    let find = s.body("find_hilo");
    for piece in ["find_hilo_in(OS,", "current_exe()", "home()", "is_file"] {
        assert!(find.contains(piece), "`find_hilo()` should call find_hilo_in with OS, current_exe()'s directory, home() and is_file: no `{piece}` in it");
    }
}

#[test]
fn show_in_finder_is_written_only_in_reveal_label() {
    let s = src();
    let page = s.body("folders_page");
    assert!(page.contains("reveal_label("), "folders_page does not call reveal_label for its button");
    let label = &s.func("reveal_label").body;
    for text in ["Show in Finder", "Show in Explorer"] {
        for (at, _) in s.code.match_indices(text).filter(|(at, _)| !s.behind(*at, only_in_tests)) {
            assert!(label.contains(&at), "line {}: \"{text}\" is written outside reveal_label", s.line(at));
        }
    }
}

#[test]
fn titlebar_follows_the_platform() {
    let s = src();
    let code = tight(&s.code);
    let calls = s.uses("transparent_titlebar").into_iter().filter(|&at| !s.bare[..at].trim_end().ends_with("fn")).count();
    assert!(calls > 0, "nothing calls transparent_titlebar: the window still gets the Mac titlebar on Windows");
    // the Mac window, as it was
    for piece in [r#"title:Some("hilo".into())"#, "appears_transparent:", "traffic_light_position:", "point(px(9.),px(5.))"] {
        assert!(code.replace(".0)", ".)").contains(piece), "the Mac TitlebarOptions changed: no `{piece}` in app/src/main.rs");
    }
    // the menu and cmd-q stay, for every platform or behind a cfg
    for piece in ["set_menus(", r#""cmd-q""#] {
        assert!(code.contains(piece), "`{piece}` is gone from app/src/main.rs");
    }
}

#[test]
fn launchd_pieces_are_mac_only() {
    let s = src();
    for word in ["getuid", "launchctl", "LaunchAgents", "Library"] {
        let uses = s.uses(word);
        assert!(!uses.is_empty(), "`{word}` is gone from app/src/main.rs: macOS must start the daemon as before");
        for at in uses {
            assert!(s.behind(at, not_on_windows), "line {}: `{word}` is compiled on Windows, it must be behind #[cfg(target_os = \"macos\")]", s.line(at));
        }
    }
    for at in s.uses("libc") {
        assert!(s.behind(at, not_on_windows), "line {}: libc is compiled on Windows", s.line(at));
    }
}

#[test]
fn windows_spawns_the_daemon() {
    let s = src();
    // `"daemon"` as an argument, in a function that spawns, both in code Windows compiles
    let spawned = s.code.match_indices("\"daemon\"").map(|(at, _)| at).filter(|&at| !s.behind(at, only_in_tests) && !s.behind(at, not_on_windows)).any(|at| {
        s.around(at).is_some_and(|f| words(&s.bare[f.body.clone()], "spawn").iter().any(|i| !s.behind(f.body.start + i, not_on_windows)))
    });
    assert!(spawned, "on Windows nothing spawns `<bin> daemon`: start_daemon needs a #[cfg(windows)] branch that does");
}

#[test]
fn subprocesses_open_no_console_window_on_windows() {
    let s = src();
    let flags = s.uses("creation_flags");
    assert!(!flags.is_empty(), "no subprocess sets creation_flags(0x08000000) (CREATE_NO_WINDOW)");
    for &at in &flags {
        assert!(s.behind(at, only_on_windows), "line {}: creation_flags must be behind #[cfg(windows)]", s.line(at));
    }
    let no_window = s.code.match_indices("0x").any(|(i, _)| {
        let digits: String = s.code[i + 2..].chars().take_while(|c| c.is_ascii_hexdigit() || *c == '_').filter(|c| *c != '_').collect();
        u64::from_str_radix(&digits, 16) == Ok(0x0800_0000)
    });
    assert!(no_window, "creation_flags is not given 0x08000000 (CREATE_NO_WINDOW)");
    // every Command that Windows builds (run's, start's, the daemon's) sets the flag: in the same
    // function, or through a helper that does
    let helpers: Vec<&str> = flags.iter().filter_map(|&at| s.around(at)).map(|f| f.name.as_str()).collect();
    let commands: Vec<usize> = s.uses("Command::new").into_iter().filter(|&at| !s.behind(at, not_on_windows)).collect();
    assert!(!commands.is_empty(), "no Command::new is compiled on Windows");
    for at in commands {
        let f = s.around(at).expect("Command::new inside a function");
        let body = &s.bare[f.body.clone()];
        let quiet = !words(body, "creation_flags").is_empty() || helpers.iter().any(|h| !words(body, h).is_empty());
        assert!(quiet, "line {}: the subprocess started in `{}` gets no creation_flags(0x08000000) on Windows", s.line(at), f.name);
    }
}

#[test]
fn default_folder_is_the_desktop_with_its_onedrive_note() {
    let s = src();
    // the offset of every non-whitespace byte, to find the expression however it is wrapped
    let kept: Vec<usize> = s.code.bytes().enumerate().filter(|(_, c)| !c.is_ascii_whitespace()).map(|(i, _)| i).collect();
    let at = tight(&s.code).find(r#"home().join("Desktop").join(LABEL)"#).map(|i| kept[i]).expect("the default folder is no longer home().join(\"Desktop\").join(LABEL)");
    assert!(!s.behind(at, only_in_tests) && !s.gates.iter().any(|(_, r)| r.contains(&at)), "the default folder must be the same on both platforms, not behind a #[cfg]");

    let (raw, code): (Vec<&str>, Vec<&str>) = (s.raw.lines().collect(), s.code.lines().collect());
    // a line's comment is what `code` blanked
    let comment = |i: usize| raw[i].bytes().zip(code[i].bytes()).position(|(a, b)| a != b).map_or("", |k| &raw[i][k..]);
    let only_comment = |i: usize| code[i].trim().is_empty() && !comment(i).is_empty();
    // the comment lines right above and below, and the one at the end of the line
    let line = s.line(at) - 1;
    let (mut first, mut last) = (line, line);
    while first > 0 && only_comment(first - 1) {
        first -= 1;
    }
    while last + 1 < raw.len() && only_comment(last + 1) {
        last += 1;
    }
    let notes: Vec<&str> = (first..=last).map(comment).collect();
    let from = notes.iter().position(|c| c.trim_start_matches(['/', '*', '!', ' ', '\t']).starts_with("ponytail:")).unwrap_or_else(|| panic!("no comment starting with `ponytail:` next to the default folder (line {})", line + 1));
    let note = notes[from..].join(" ");
    for word in ["Windows", "OneDrive", "FOLDERID_Desktop"] {
        assert!(note.contains(word), "the ponytail: comment next to the default folder does not mention {word}: {note}");
    }
}

#[test]
fn brushed_metal_look_is_untouched() {
    let code = tight(&src().code);
    for piece in [
        r#"constUI:&str="LucidaGrande";"#,
        r#"constMONO:&str="Monaco";"#,
        "constSTRIPE:u32=0xedf3fe;",
        "constSELECT:u32=0x3875d7;",
        "constSOURCE:u32=0xe7edf6;",
        r#"include_bytes!("../assets/metal.png")"#,
    ] {
        assert!(code.contains(piece), "the brushed metal look changed: no `{piece}` in app/src/main.rs");
    }
}

/// Cargo.toml as it was: no dependency added, none per platform. (Cargo.lock: the nested cargo
/// runs above are `--locked`.)
#[test]
fn no_dependency_is_added() {
    let toml = std::fs::read_to_string(Path::new(APP).join("Cargo.toml")).unwrap();
    let lines: Vec<String> = toml.lines().map(tight).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    let tables: Vec<&str> = lines.iter().map(String::as_str).filter(|l| l.starts_with('[')).collect();
    assert_eq!(tables, ["[package]", "[workspace]", "[dependencies]", "[profile.release]"], "app/Cargo.toml's tables changed");
    let deps: Vec<&str> = lines.iter().map(String::as_str).skip_while(|l| *l != "[dependencies]").skip(1).take_while(|l| !l.starts_with('[')).collect();
    assert_eq!(deps, [r#"gpui={version="=0.2.2",features=["runtime_shaders"]}"#, r#"serde_json="1""#], "app/Cargo.toml's dependencies changed");
}

#[test]
fn main_rs_has_its_own_tests() {
    let s = src();
    let tests = s.gates.iter().find(|(attr, r)| only_in_tests(attr) && tight(&s.bare[r.clone()]).contains("modtests{")).map(|(_, r)| &s.code[r.clone()]).expect("app/src/main.rs has no `#[cfg(test)] mod tests`");
    assert!(tight(tests).contains("usesuper::*;"), "`mod tests` should `use super::*`");
    assert!(tests.matches("#[test]").count() >= 4, "`mod tests` should test home_in, find_hilo_in, reveal_label and transparent_titlebar");
    for piece in ["home_in", "find_hilo_in", "reveal_label", "transparent_titlebar", "USERPROFILE", "Show in Finder", "Show in Explorer"] {
        assert!(tests.contains(piece), "`mod tests` never mentions {piece}");
    }
}
