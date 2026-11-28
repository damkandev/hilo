//! Symlink primitives of `fsops`: `link_payload` / `parse_link`, `link_ok`, `write_link`, and
//! `write_mode` replacing a symlink instead of failing with ELOOP.
//! Child module of `fsops`. Wire it at the end of src/fsops.rs with exactly:
//!     #[cfg(test)] #[path = "fsops_symlink_tests.rs"] mod symlink_tests;
//! Only std. The tests that touch the disk are Unix-only.

use super::*;

const PREFIX: &[u8] = b"\0hilo-symlink\0";

// ---- link_payload / parse_link ----

#[test]
fn link_payload_is_prefix_plus_target_bytes() {
    assert_eq!(PREFIX.len(), 14);
    assert_eq!(link_payload("AGENTS.md"), b"\0hilo-symlink\0AGENTS.md".to_vec());
    assert_eq!(link_payload(""), PREFIX.to_vec());
    // nothing is normalized, trimmed or validated on the way in
    for t in ["../docs/x.md", "./a//b/", " con espacios \n", "a\\b", "/etc/passwd", "ñandú/árbol.md", "a\0b"] {
        let p = link_payload(t);
        assert_eq!(&p[..14], PREFIX, "{t:?}");
        assert_eq!(&p[14..], t.as_bytes(), "{t:?}");
    }
}

#[test]
fn parse_link_round_trips_any_target() {
    // parse_link does not validate the target: targets link_ok refuses come back too
    for t in ["AGENTS.md", "", "../docs/x.md", "sub/dir", "/etc/passwd", "..", "a\0b", "a\\b", "C:x", "ñandú/árbol.md", ".git/config", "./a//b/", "\0hilo-symlink\0"] {
        assert_eq!(parse_link(&link_payload(t)), Some(t), "{t:?}");
    }
}

#[test]
fn parse_link_fixed_cases() {
    assert_eq!(parse_link(b"\0hilo-symlink\0"), Some(""));
    assert_eq!(parse_link(b"\0hilo-symlink\0AGENTS.md"), Some("AGENTS.md"));
    assert_eq!(parse_link(b""), None);
    assert_eq!(parse_link(b"hola"), None);
    assert_eq!(parse_link(b"\0hilo-symlink\0\xff"), None);
}

#[test]
fn parse_link_needs_the_exact_prefix_at_the_start() {
    for bad in [
        &b"\0hilo-symlink"[..],       // 13 bytes: the closing NUL is missing
        b"\0",                        // only the first byte
        b"hilo-symlink\0x",           // no leading NUL
        b"\0HILO-SYMLINK\0x",         // the prefix is compared byte by byte
        b"\0hilo-symlink x",          // a space where the NUL goes
        b"x\0hilo-symlink\0AGENTS.md", // the prefix is not at offset 0
        b"AGENTS.md",                 // a plain file that happens to name a target
    ] {
        assert_eq!(parse_link(bad), None, "{bad:?}");
    }
}

#[test]
fn parse_link_rejects_non_utf8_rest() {
    let mut d = link_payload("ok/");
    d.extend_from_slice(&[0xc3, 0x28]); // invalid two-byte sequence after a valid start
    assert_eq!(parse_link(&d), None);
    let mut d = link_payload("a");
    d.push(0xe2); // truncated multi-byte character at the end
    assert_eq!(parse_link(&d), None);
}

// ---- link_ok ----

#[test]
fn link_ok_contract_table() {
    for (rel, target, want) in [
        ("CLAUDE.md", "AGENTS.md", true),
        ("a/b/l", "../docs/x.md", true),
        ("l", "sub/dir", true),
        ("l", "/etc/passwd", false),
        ("a/l", "../../fuera", false),
        ("l", "..", false),
        ("l", "a/../../x", false),
        ("l", ".git/config", false),
        ("l", "", false),
        ("l", "a\0b", false),
    ] {
        assert_eq!(link_ok(rel, target), want, "link_ok({rel:?}, {target:?})");
    }
}

#[test]
fn link_ok_refuses_backslash_colon_and_absolute() {
    for target in ["a\\b", "..\\x", "\\\\server\\share", "C:x", "C:/Windows", "a:b", "sub/x:stream", "/", "/a", "//a"] {
        assert!(!link_ok("l", target), "link_ok(\"l\", {target:?})");
        assert!(!link_ok("a/b/l", target), "link_ok(\"a/b/l\", {target:?})");
    }
}

#[test]
fn link_ok_refuses_escaping_the_root() {
    for (rel, target) in [
        ("l", "../x"),
        ("l", "a/b/../../../x"),
        ("a/l", "../../x"),
        ("a/l", "x/../../../y"),
        ("a/b/l", "../../../x"),
        ("a/b/l", "../../.."),
        ("a/b/l", "../../../a/b/x"), // leaves the root and comes back: still refused
    ] {
        assert!(!link_ok(rel, target), "link_ok({rel:?}, {target:?})");
    }
}

#[test]
fn link_ok_refuses_the_root_itself() {
    for (rel, target) in [("l", "."), ("l", "./"), ("l", "a/.."), ("l", "./a/../."), ("a/l", ".."), ("a/l", "../"), ("a/b/l", "../.."), ("a/b/l", "../x/../..")] {
        assert!(!link_ok(rel, target), "link_ok({rel:?}, {target:?})");
    }
}

#[test]
fn link_ok_refuses_dot_git_in_any_case_anywhere() {
    for (rel, target) in [
        ("l", ".git"),
        ("l", ".GIT/config"),
        ("l", ".Git"),
        ("l", "sub/.git/hooks/pre-commit"),
        ("l", "sub/.gIt"),
        ("a/l", "../.git/HEAD"),
        ("a/b/l", ".git"),
        ("l", ".git/../x"), // refused when the component is pushed, even if a later `..` leaves it
    ] {
        assert!(!link_ok(rel, target), "link_ok({rel:?}, {target:?})");
    }
}

#[test]
fn link_ok_resolves_dot_dotdot_and_empty_components() {
    for (rel, target) in [
        ("l", "./a"),
        ("l", "a//b"),
        ("l", "a/"),
        ("l", "a/./b"),
        ("l", "a/../b"),
        ("l", "a/b/../../c"),
        ("a/l", "../b"),
        ("a/l", "x"),
        ("a/b/l", "../../x"), // climbs exactly to the root and goes down again
        ("a/b/l", ".."),      // resolves to `a`, inside the root
        ("a/l", "."),         // resolves to `a`
        ("l", ".gitignore"),
        ("l", ".github/workflows/ci.yml"),
        ("l", "a/.gitkeep"),
        ("l", "git"),
        ("l", "ñandú/árbol.md"),
        ("l", "con espacios/x y.md"),
    ] {
        assert!(link_ok(rel, target), "link_ok({rel:?}, {target:?})");
    }
}

// ---- write_link / write_mode on disk (Unix: real symlinks) ----

/// A scratch directory holding `repo` (the root) and room for things outside it. Removed on drop,
/// also when the test panics.
#[cfg(unix)]
struct Tmp {
    base: PathBuf,
    root: PathBuf,
}

#[cfg(unix)]
impl Tmp {
    fn new() -> Tmp {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("hilo-symlink-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base); // a leftover of an older run with the same pid
        let root = base.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        Tmp { base, root }
    }

    /// Writes a plain file under the root with std, creating its parents.
    fn file(&self, rel: &str, data: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    /// Makes a symlink under the root with std (not with the code under test).
    fn link(&self, rel: &str, target: &str) {
        std::os::unix::fs::symlink(target, self.root.join(rel)).unwrap();
    }
}

#[cfg(unix)]
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Sorted entry names of a directory.
#[cfg(unix)]
fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[cfg(unix)]
fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

#[cfg(unix)]
fn is_regular(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file())
}

#[cfg(unix)]
fn link_target(p: &Path) -> String {
    std::fs::read_link(p).unwrap().to_str().unwrap().to_string()
}

#[cfg(unix)]
fn text(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

/// No hilo temp file anywhere under `dir`.
#[cfg(unix)]
fn assert_no_tmp(dir: &Path) {
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().into_owned();
        assert!(!name.starts_with(TMP_PREFIX), "temp file left behind: {}", e.path().display());
        if e.file_type().unwrap().is_dir() {
            assert_no_tmp(&e.path());
        }
    }
}

#[test]
#[cfg(unix)]
fn write_link_creates_a_symlink_with_the_exact_target() {
    let t = Tmp::new();
    t.file("AGENTS.md", "reglas");
    write_link(&t.root, "CLAUDE.md", "AGENTS.md").unwrap();
    let l = t.root.join("CLAUDE.md");
    assert!(is_link(&l));
    assert_eq!(link_target(&l), "AGENTS.md");
    assert_eq!(text(&l), "reglas"); // the link works: reading through it reaches the target
    assert_eq!(names(&t.root), ["AGENTS.md", "CLAUDE.md"]); // and no temp name is left
}

#[test]
#[cfg(unix)]
fn write_link_keeps_the_target_text_verbatim() {
    let t = Tmp::new();
    for (rel, target) in [("l1", "sub/dir"), ("l2", "./docs//x.md"), ("l3", "a/../b"), ("l4", "sub/"), ("l5", "ñandú/árbol.md"), ("l6", "con espacios/x y.md")] {
        write_link(&t.root, rel, target).unwrap();
        assert_eq!(link_target(&t.root.join(rel)), target, "{rel}");
    }
    assert_no_tmp(&t.root);
}

#[test]
#[cfg(unix)]
fn write_link_allows_a_dangling_target() {
    let t = Tmp::new();
    write_link(&t.root, "l", "todavia/no/existe.md").unwrap();
    let l = t.root.join("l");
    assert!(is_link(&l));
    assert_eq!(link_target(&l), "todavia/no/existe.md");
    assert!(!l.exists()); // following it finds nothing
    assert_eq!(names(&t.root), ["l"]); // the target's parents are not created
}

#[test]
#[cfg(unix)]
fn write_link_creates_missing_parents_and_stores_a_relative_target() {
    let t = Tmp::new();
    t.file("a/docs/x.md", "doc");
    write_link(&t.root, "a/b/l", "../docs/x.md").unwrap();
    let l = t.root.join("a/b/l");
    assert!(std::fs::symlink_metadata(t.root.join("a/b")).unwrap().file_type().is_dir()); // a real directory, not a link
    assert!(is_link(&l));
    assert_eq!(link_target(&l), "../docs/x.md"); // stored as given, not resolved against the root
    assert_eq!(text(&l), "doc");
    // deeper, with nothing there yet
    write_link(&t.root, "n1/n2/n3/l", "../../../a/docs/x.md").unwrap();
    assert_eq!(link_target(&t.root.join("n1/n2/n3/l")), "../../../a/docs/x.md");
    assert_eq!(text(&t.root.join("n1/n2/n3/l")), "doc");
    assert_no_tmp(&t.root);
}

#[test]
#[cfg(unix)]
fn write_link_replaces_a_regular_file() {
    let t = Tmp::new();
    t.file("d/f", "contenido viejo");
    t.file("d/otro", "otro");
    write_link(&t.root, "d/f", "otro").unwrap();
    let f = t.root.join("d/f");
    assert!(is_link(&f));
    assert_eq!(link_target(&f), "otro");
    assert!(is_regular(&t.root.join("d/otro")));
    assert_eq!(text(&t.root.join("d/otro")), "otro");
    assert_eq!(names(&t.root.join("d")), ["f", "otro"]);
}

#[test]
#[cfg(unix)]
fn write_link_replaces_a_symlink_and_leaves_its_old_target_alone() {
    let t = Tmp::new();
    t.file("a.md", "soy a");
    t.file("b.md", "soy b");
    t.link("l", "a.md");
    write_link(&t.root, "l", "b.md").unwrap();
    let l = t.root.join("l");
    assert!(is_link(&l));
    assert_eq!(link_target(&l), "b.md");
    assert_eq!(text(&l), "soy b");
    // the old target was neither followed nor touched
    assert!(is_regular(&t.root.join("a.md")));
    assert_eq!(text(&t.root.join("a.md")), "soy a");
    assert!(is_regular(&t.root.join("b.md")));
    assert_eq!(text(&t.root.join("b.md")), "soy b");
    assert_eq!(names(&t.root), ["a.md", "b.md", "l"]);
    // writing the same link again is fine
    write_link(&t.root, "l", "b.md").unwrap();
    assert_eq!(link_target(&l), "b.md");
    assert_eq!(names(&t.root), ["a.md", "b.md", "l"]);
}

#[test]
#[cfg(unix)]
fn write_link_replaces_a_symlink_to_a_directory_without_entering_it() {
    let t = Tmp::new();
    t.file("d1/x", "x");
    t.file("d2/y", "y");
    t.link("l", "d1");
    write_link(&t.root, "l", "d2").unwrap();
    assert!(is_link(&t.root.join("l")));
    assert_eq!(link_target(&t.root.join("l")), "d2");
    assert_eq!(names(&t.root.join("d1")), ["x"]); // nothing was created inside the old target
    assert_eq!(names(&t.root.join("d2")), ["y"]);
    assert_eq!(names(&t.root), ["d1", "d2", "l"]);
}

#[test]
#[cfg(unix)]
fn write_link_replaces_a_dangling_symlink() {
    let t = Tmp::new();
    t.link("l", "no-existe");
    write_link(&t.root, "l", "tampoco").unwrap();
    assert_eq!(link_target(&t.root.join("l")), "tampoco");
    assert_eq!(names(&t.root), ["l"]);
}

#[test]
#[cfg(unix)]
fn write_link_invalid_target_is_invalid_input_and_creates_nothing() {
    let t = Tmp::new();
    for target in ["/etc/passwd", "..", "../fuera", "a/../../x", ".git/config", "sub/.GIT/hooks", "", "a\0b", "a\\b", "C:x", "."] {
        let e = write_link(&t.root, "l", target).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "target {target:?}");
        assert_eq!(names(&t.root), Vec::<String>::new(), "target {target:?}");
    }
    let e = write_link(&t.root, "a/l", "../../fuera").unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(names(&t.root), Vec::<String>::new()); // not even the parent directory
    assert_eq!(names(&t.base), ["repo"]); // and nothing next to the root
}

#[test]
#[cfg(unix)]
fn write_link_invalid_target_leaves_what_was_there() {
    let t = Tmp::new();
    t.file("f", "intacto");
    t.file("a.md", "soy a");
    t.link("l", "a.md");
    for target in ["/etc/passwd", "../fuera", ".git/config", ""] {
        assert_eq!(write_link(&t.root, "f", target).unwrap_err().kind(), io::ErrorKind::InvalidInput, "{target:?}");
        assert_eq!(write_link(&t.root, "l", target).unwrap_err().kind(), io::ErrorKind::InvalidInput, "{target:?}");
    }
    assert!(is_regular(&t.root.join("f")));
    assert_eq!(text(&t.root.join("f")), "intacto");
    assert!(is_link(&t.root.join("l")));
    assert_eq!(link_target(&t.root.join("l")), "a.md");
    assert_eq!(names(&t.root), ["a.md", "f", "l"]);
}

#[test]
#[cfg(unix)]
fn write_link_refuses_a_symlinked_parent_inside_the_root() {
    let t = Tmp::new();
    std::fs::create_dir(t.root.join("real")).unwrap();
    t.link("p", "real");
    assert!(write_link(&t.root, "p/l", "x").is_err());
    assert!(std::fs::symlink_metadata(t.root.join("real/l")).is_err());
    assert_eq!(names(&t.root.join("real")), Vec::<String>::new()); // no link and no temp file
    assert!(is_link(&t.root.join("p")));
    assert_eq!(link_target(&t.root.join("p")), "real");
    assert_eq!(names(&t.root), ["p", "real"]);
}

#[test]
#[cfg(unix)]
fn write_link_refuses_a_symlinked_parent_at_any_depth() {
    let t = Tmp::new();
    std::fs::create_dir_all(t.root.join("a")).unwrap();
    std::fs::create_dir_all(t.root.join("real/sub")).unwrap();
    t.link("a/p", "../real");
    // the link is the direct parent
    assert!(write_link(&t.root, "a/p/l", "x").is_err());
    // the link is higher up, with an existing directory below it
    assert!(write_link(&t.root, "a/p/sub/l", "x").is_err());
    // the link is higher up and the directories below it would have to be created
    assert!(write_link(&t.root, "a/p/nuevo/l", "x").is_err());
    assert_eq!(names(&t.root.join("real")), ["sub"]);
    assert_eq!(names(&t.root.join("real/sub")), Vec::<String>::new());
    assert_eq!(names(&t.root.join("a")), ["p"]);
    assert_eq!(link_target(&t.root.join("a/p")), "../real");
}

#[test]
#[cfg(unix)]
fn write_link_refuses_a_parent_symlinked_outside_the_root() {
    let t = Tmp::new();
    let outside = t.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, t.root.join("evil")).unwrap();
    assert!(write_link(&t.root, "evil/l", "x").is_err());
    assert!(write_link(&t.root, "evil/sub/l", "x").is_err());
    assert_eq!(names(&outside), Vec::<String>::new());
    assert_eq!(names(&t.root), ["evil"]);
}

#[test]
#[cfg(unix)]
fn write_link_refuses_a_rel_outside_the_root_or_inside_dot_git() {
    let t = Tmp::new();
    for rel in ["../fuera", "a/../../fuera", "/fuera", ".git/l", "sub/.GIT/l", ""] {
        assert!(write_link(&t.root, rel, "x").is_err(), "rel {rel:?}");
    }
    assert_eq!(names(&t.root), Vec::<String>::new());
    assert_eq!(names(&t.base), ["repo"]); // nothing next to the root either
}

#[test]
#[cfg(unix)]
fn write_link_error_over_a_directory_leaves_it_and_no_temp_file() {
    let t = Tmp::new();
    t.file("d/dentro", "sigue");
    t.file("f", "archivo");
    // a link can't take the place of a directory
    assert!(write_link(&t.root, "d", "f").is_err());
    assert!(std::fs::symlink_metadata(t.root.join("d")).unwrap().file_type().is_dir());
    assert_eq!(text(&t.root.join("d/dentro")), "sigue");
    // nor live under a regular file
    assert!(write_link(&t.root, "f/l", "x").is_err());
    assert!(is_regular(&t.root.join("f")));
    assert_eq!(text(&t.root.join("f")), "archivo");
    assert_eq!(names(&t.root), ["d", "f"]);
    assert_eq!(names(&t.root.join("d")), ["dentro"]);
    assert_no_tmp(&t.root);
}

#[test]
#[cfg(unix)]
fn write_mode_replaces_a_symlink_with_a_regular_file() {
    let t = Tmp::new();
    t.file("destino.md", "viejo");
    t.link("l", "destino.md");
    write_mode(&t.root, "l", Some(b"nuevo"), 0o644).unwrap();
    let l = t.root.join("l");
    assert!(is_regular(&l)); // the link itself became a file
    assert_eq!(text(&l), "nuevo");
    // nothing was written through the link
    assert!(is_regular(&t.root.join("destino.md")));
    assert_eq!(text(&t.root.join("destino.md")), "viejo");
    assert_eq!(names(&t.root), ["destino.md", "l"]);
}

#[test]
#[cfg(unix)]
fn write_replaces_a_symlink_too() {
    // `write` is `write_mode` with the default mode: same rule
    let t = Tmp::new();
    t.file("d/destino", "viejo");
    t.link("d/l", "destino");
    write(&t.root, "d/l", Some(b"nuevo")).unwrap();
    assert!(is_regular(&t.root.join("d/l")));
    assert_eq!(text(&t.root.join("d/l")), "nuevo");
    assert_eq!(text(&t.root.join("d/destino")), "viejo");
    assert_eq!(names(&t.root.join("d")), ["destino", "l"]);
}

#[test]
#[cfg(unix)]
fn write_mode_over_a_dangling_symlink_does_not_create_its_target() {
    let t = Tmp::new();
    t.link("l", "no-existe");
    write_mode(&t.root, "l", Some(b"dato"), 0o644).unwrap();
    assert!(is_regular(&t.root.join("l")));
    assert_eq!(text(&t.root.join("l")), "dato");
    assert!(std::fs::symlink_metadata(t.root.join("no-existe")).is_err());
    assert_eq!(names(&t.root), ["l"]);
}

#[test]
#[cfg(unix)]
fn write_mode_over_a_symlink_pointing_outside_writes_nothing_outside() {
    let t = Tmp::new();
    let outside = t.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secreto"), "no tocar").unwrap();
    std::os::unix::fs::symlink(outside.join("secreto"), t.root.join("a")).unwrap(); // existing file outside
    std::os::unix::fs::symlink(outside.join("nuevo"), t.root.join("b")).unwrap(); // would be created outside
    write_mode(&t.root, "a", Some(b"pwn"), 0o644).unwrap();
    write_mode(&t.root, "b", Some(b"pwn"), 0o644).unwrap();
    assert!(is_regular(&t.root.join("a")) && is_regular(&t.root.join("b")));
    assert_eq!(text(&t.root.join("a")), "pwn");
    assert_eq!(text(&t.root.join("b")), "pwn");
    assert_eq!(text(&outside.join("secreto")), "no tocar");
    assert_eq!(names(&outside), ["secreto"]);
}

#[test]
#[cfg(unix)]
fn write_mode_over_a_symlink_to_a_directory_leaves_the_directory() {
    let t = Tmp::new();
    t.file("dir/x", "x");
    t.link("l", "dir");
    write_mode(&t.root, "l", Some(b"ahora archivo"), 0o644).unwrap();
    assert!(is_regular(&t.root.join("l")));
    assert_eq!(text(&t.root.join("l")), "ahora archivo");
    assert!(std::fs::symlink_metadata(t.root.join("dir")).unwrap().file_type().is_dir());
    assert_eq!(names(&t.root.join("dir")), ["x"]);
    assert_eq!(text(&t.root.join("dir/x")), "x");
}

#[test]
#[cfg(unix)]
fn write_mode_still_refuses_a_symlinked_parent() {
    let t = Tmp::new();
    std::fs::create_dir(t.root.join("real")).unwrap();
    t.link("p", "real");
    let outside = t.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, t.root.join("evil")).unwrap();
    assert!(write_mode(&t.root, "p/f", Some(b"pwn"), 0o644).is_err());
    assert!(write_mode(&t.root, "evil/f", Some(b"pwn"), 0o644).is_err());
    assert!(write_mode(&t.root, "evil/sub/f", Some(b"pwn"), 0o644).is_err());
    assert_eq!(names(&t.root.join("real")), Vec::<String>::new());
    assert_eq!(names(&outside), Vec::<String>::new());
    assert_eq!(names(&t.root), ["evil", "p", "real"]);
}

#[test]
#[cfg(unix)]
fn the_four_replacements_in_a_row_on_one_path() {
    let t = Tmp::new();
    t.file("a.md", "soy a");
    t.file("b.md", "soy b");
    let p = t.root.join("x");
    // nothing → file → file
    write_mode(&t.root, "x", Some(b"v1"), 0o644).unwrap();
    write_mode(&t.root, "x", Some(b"v2"), 0o644).unwrap();
    assert!(is_regular(&p));
    assert_eq!(text(&p), "v2");
    // file → link
    write_link(&t.root, "x", "a.md").unwrap();
    assert!(is_link(&p));
    assert_eq!(link_target(&p), "a.md");
    // link → link
    write_link(&t.root, "x", "b.md").unwrap();
    assert!(is_link(&p));
    assert_eq!(link_target(&p), "b.md");
    // link → file
    write_mode(&t.root, "x", Some(b"v3"), 0o644).unwrap();
    assert!(is_regular(&p));
    assert_eq!(text(&p), "v3");
    // neither target ever changed, and no temp name survived
    assert_eq!(text(&t.root.join("a.md")), "soy a");
    assert_eq!(text(&t.root.join("b.md")), "soy b");
    assert_eq!(names(&t.root), ["a.md", "b.md", "x"]);
}

#[test]
#[cfg(unix)]
fn write_mode_file_over_file_still_keeps_the_mode() {
    let t = Tmp::new();
    write_mode(&t.root, "bin/run", Some(b"#!/bin/sh\n"), 0o755).unwrap();
    assert_eq!(std::fs::metadata(t.root.join("bin/run")).unwrap().permissions().mode() & 0o777, 0o755);
    write_mode(&t.root, "bin/run", Some(b"#!/bin/sh\necho hi\n"), 0o644).unwrap(); // the existing mode wins
    assert_eq!(std::fs::metadata(t.root.join("bin/run")).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(text(&t.root.join("bin/run")), "#!/bin/sh\necho hi\n");
    assert_eq!(names(&t.root.join("bin")), ["run"]);
}
