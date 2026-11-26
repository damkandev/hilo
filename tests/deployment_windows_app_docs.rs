//! docs/deployment.md and docs/deployment.es.md: the desktop app subsection under `### Windows`
//! (`#### Desktop app` / `#### App de escritorio`). Reads the docs as plain markdown.

use std::path::Path;

fn read(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())).replace("\r\n", "\n")
}

/// (doc, `## Platforms` title, desktop app title)
fn docs() -> [(String, &'static str, &'static str); 2] {
    [(read("deployment.md"), "Platforms", "Desktop app"), (read("deployment.es.md"), "Plataformas", "App de escritorio")]
}

/// The heading level of a line, outside code fences.
fn heading(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_start();
    let n = t.chars().take_while(|&c| c == '#').count();
    (n > 0 && t[n..].starts_with(' ')).then(|| (n, t[n..].trim()))
}

/// Raw markdown of a section (heading line excluded) up to the next heading of the same or higher
/// level, fence-aware. Panics unless there is exactly one such heading.
fn section_text(md: &str, level: usize, title: &str) -> String {
    let (mut fenced, mut inside, mut found) = (false, false, 0);
    let mut out = vec![];
    for l in md.lines() {
        let fence = l.trim_start().starts_with("```");
        if fence {
            fenced = !fenced;
        }
        let h = if fenced || fence { None } else { heading(l) };
        if matches!(h, Some((n, t)) if n == level && t == title) {
            found += 1;
            inside = true;
            continue;
        }
        if inside && matches!(h, Some((n, _)) if n <= level) {
            inside = false;
        }
        if inside {
            out.push(l);
        }
    }
    assert_eq!(found, 1, "expected exactly one `{} {title}` heading, found {found}", "#".repeat(level));
    out.join("\n")
}

fn windows(md: &str, platforms: &str) -> String {
    section_text(&section_text(md, 2, platforms), 3, "Windows")
}

/// The desktop app subsection, with `\` turned into `/` so `~\Desktop\hilo` reads as `~/Desktop/hilo`.
fn app(md: &str, platforms: &str, title: &str) -> String {
    section_text(&windows(md, platforms), 4, title).replace('\\', "/")
}

fn code_blocks(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out = vec![];
    let mut lines = text.lines();
    while let Some(l) = lines.next() {
        if let Some(info) = l.trim_start().strip_prefix("```") {
            let body = lines.by_ref().take_while(|l| !l.trim_start().starts_with("```")).map(str::to_string).collect();
            out.push((info.trim().to_string(), body));
        }
    }
    out
}

/// Headings, code fences, tables and bullets in order: what both languages must share.
fn skeleton(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut fenced = false;
    let mut in_table = false;
    for l in text.lines() {
        let t = l.trim_start();
        let table = !fenced && t.starts_with('|');
        if let Some(info) = t.strip_prefix("```") {
            fenced = !fenced;
            if fenced {
                out.push(format!("code {}", info.trim()));
            }
        } else if fenced {
        } else if let Some((n, _)) = heading(l) {
            out.push(format!("h{n}"));
        } else if table && !in_table {
            out.push("table".into());
        } else if t.starts_with("- ") || t.starts_with("* ") {
            out.push("bullet".into());
        }
        in_table = table;
    }
    out
}

fn any(text: &str, words: &[&str]) -> bool {
    let low = text.to_lowercase();
    words.iter().any(|w| low.contains(w))
}

// ---------- position ----------

#[test]
fn desktop_app_is_a_subsection_of_windows_in_both_docs() {
    for (md, platforms, title) in docs() {
        let w = windows(&md, platforms);
        let h4: Vec<&str> = {
            let mut fenced = false;
            w.lines()
                .filter(|l| {
                    if l.trim_start().starts_with("```") {
                        fenced = !fenced;
                        return false;
                    }
                    !fenced
                })
                .filter_map(|l| heading(l).filter(|(n, _)| *n == 4).map(|(_, t)| t))
                .collect()
        };
        assert!(h4.contains(&title), "`### Windows` has no `#### {title}`: {h4:?}");
        assert!(!app(&md, platforms, title).trim().is_empty(), "`#### {title}` is empty");
    }
}

#[test]
fn each_doc_uses_its_own_language_for_the_heading() {
    let [(en, ..), (es, ..)] = docs();
    assert!(!en.contains("#### App de escritorio"), "deployment.md has the Spanish heading");
    assert!(!es.contains("#### Desktop app"), "deployment.es.md has the English heading");
}

// ---------- content ----------

#[test]
fn names_the_script_the_zip_and_both_executables() {
    for (md, platforms, title) in docs() {
        let t = app(&md, platforms, title);
        for lit in ["bundle.ps1", "hilo-windows.zip", "hilo-app.exe", "hilo.exe"] {
            assert!(t.contains(lit), "`#### {title}` lacks `{lit}`:\n{t}");
        }
    }
}

#[test]
fn says_how_to_build_it_and_what_it_needs() {
    for (md, platforms, title) in docs() {
        let t = app(&md, platforms, title);
        assert!(t.contains("app/bundle.ps1"), "`#### {title}` must give the command `app\\bundle.ps1`:\n{t}");
        assert!(t.contains("cargo"), "`#### {title}` must say it needs cargo:\n{t}");
        assert!(t.contains("MSVC"), "`#### {title}` must say it needs the MSVC toolchain:\n{t}");
    }
}

#[test]
fn says_how_to_use_it_and_where_the_app_looks_for_the_cli() {
    for (md, platforms, title) in docs() {
        let t = app(&md, platforms, title);
        let (unzip, same): (&[&str], &[&str]) = if title == "Desktop app" {
            (&["unzip", "extract", "unpack"], &["same folder", "same directory", "same place", "next to", "beside", "alongside", "together"])
        } else {
            (&["descompr", "extrae", "extraer"], &["misma carpeta", "mismo directorio", "mismo lugar", "junto", "al lado"])
        };
        assert!(any(&t, unzip), "`#### {title}` must say to unzip it:\n{t}");
        assert!(any(&t, same), "`#### {title}` must say `hilo.exe` stays in the same folder as `hilo-app.exe`:\n{t}");
        assert!(t.contains(".local/bin"), "`#### {title}` must say the app also looks in ~/.local/bin:\n{t}");
        assert!(t.contains("PATH"), "`#### {title}` must say the app also looks on the PATH:\n{t}");
    }
}

#[test]
fn says_it_is_unsigned_and_not_in_the_releases() {
    for (md, platforms, title) in docs() {
        let t = app(&md, platforms, title);
        let signed: &[&str] = if title == "Desktop app" { &["signed"] } else { &["firma"] };
        assert!(any(&t, signed), "`#### {title}` must say the zip is not signed:\n{t}");
        assert!(t.contains("SmartScreen"), "`#### {title}` must warn about SmartScreen:\n{t}");
        assert!(any(&t, &["release"]), "`#### {title}` must say it is not published in the releases:\n{t}");
    }
}

#[test]
fn mentions_the_default_folder_and_the_onedrive_redirection() {
    for (md, platforms, title) in docs() {
        let t = app(&md, platforms, title);
        assert!(t.contains("Desktop/hilo"), "`#### {title}` must name the default folder ~/Desktop/hilo:\n{t}");
        assert!(t.contains("OneDrive"), "`#### {title}` must mention OneDrive:\n{t}");
        assert!(any(&t, &["redirec", "redirig"]), "`#### {title}` must say the Desktop redirection is not followed:\n{t}");
    }
}

// ---------- both languages say the same ----------

#[test]
fn both_languages_have_the_same_structure() {
    let [(en, ep, et), (es, sp, st)] = docs();
    let (e, s) = (section_text(&windows(&en, ep), 4, et), section_text(&windows(&es, sp), 4, st));
    assert_eq!(skeleton(&e), skeleton(&s), "English and Spanish desktop app sections differ in structure");
}

/// Commands are the same in both; only their trailing `# comments` are translated.
#[test]
fn both_languages_show_the_same_commands() {
    fn commands(text: &str) -> Vec<Vec<String>> {
        code_blocks(text)
            .into_iter()
            .map(|(_, body)| body.iter().map(|l| l.split(" #").next().unwrap().trim().to_string()).filter(|l| !l.is_empty()).collect())
            .collect()
    }
    let [(en, ep, et), (es, sp, st)] = docs();
    let (e, s) = (section_text(&windows(&en, ep), 4, et), section_text(&windows(&es, sp), 4, st));
    assert_eq!(commands(&e), commands(&s), "code blocks differ between languages");
}

// ---------- the rest of the section stays ----------

#[test]
fn the_rest_of_the_windows_section_is_still_there() {
    for (md, platforms, _) in docs() {
        let w = windows(&md, platforms);
        for lit in ["cargo build --release", r"copy target\release\hilo.exe $HOME\.local\bin\", "core.autocrlf", "~/.hilo/hilo.sock", "hilo run -- CMD"] {
            assert!(w.contains(lit), "`### Windows` lost `{lit}`");
        }
        assert!(code_blocks(&w).iter().any(|(info, _)| info == "powershell"), "`### Windows` lost its powershell block");
    }
    let [(en, ..), (es, ..)] = docs();
    assert!(en.contains("- No desktop notifications; they are in the log."));
    assert!(es.contains("- Sin notificaciones de escritorio; quedan en el log."));
}
