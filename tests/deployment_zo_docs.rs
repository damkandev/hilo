//! docs/deployment.md and docs/deployment.es.md: the `## Zo Computer` section (peer and anchor)
//! and the Zo Computer row in the background-service table. Reads the docs as plain markdown.

use std::path::Path;

fn read(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())).replace("\r\n", "\n")
}

fn en() -> String {
    read("deployment.md")
}

fn es() -> String {
    read("deployment.es.md")
}

#[derive(Debug, PartialEq, Clone)]
enum Block {
    Heading(usize, String),
    Table(Vec<Vec<String>>),
    Code(String, String), // (info string, body)
}

fn cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// Headings, tables and fenced code blocks, in order. Lines inside fences are never headings or tables.
fn blocks(md: &str) -> Vec<Block> {
    let mut out = vec![];
    let mut lines = md.lines().peekable();
    while let Some(l) = lines.next() {
        let t = l.trim_start();
        if let Some(info) = t.strip_prefix("```") {
            let mut body = vec![];
            for l in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    break;
                }
                body.push(l);
            }
            out.push(Block::Code(info.trim().to_string(), body.join("\n")));
        } else if t.starts_with('#') {
            let level = t.chars().take_while(|&c| c == '#').count();
            if t[level..].starts_with(' ') {
                out.push(Block::Heading(level, t[level..].trim().to_string()));
            }
        } else if t.starts_with('|') {
            let mut rows = vec![cells(t)];
            while let Some(n) = lines.peek() {
                if !n.trim_start().starts_with('|') {
                    break;
                }
                rows.push(cells(lines.next().unwrap()));
            }
            out.push(Block::Table(rows));
        }
    }
    out
}

/// Blocks after the heading `title` (at `level`) up to the next heading of the same or higher level.
fn section(bs: &[Block], level: usize, title: &str) -> Vec<Block> {
    let hits: Vec<usize> = bs.iter().enumerate().filter(|(_, b)| **b == Block::Heading(level, title.into())).map(|(i, _)| i).collect();
    assert_eq!(hits.len(), 1, "expected exactly one `{} {title}` heading, found {}", "#".repeat(level), hits.len());
    bs[hits[0] + 1..].iter().take_while(|b| !matches!(b, Block::Heading(l, _) if *l <= level)).cloned().collect()
}

/// Raw markdown text of a section (heading line excluded), fence-aware.
fn section_text(md: &str, level: usize, title: &str) -> String {
    let want = format!("{} {title}", "#".repeat(level));
    let mut fenced = false;
    let mut inside = false;
    let mut out = vec![];
    for l in md.lines() {
        let t = l.trim_start();
        if t.starts_with("```") {
            fenced = !fenced;
        }
        let heading_level = if !fenced && !t.starts_with("```") && t.starts_with('#') {
            let n = t.chars().take_while(|&c| c == '#').count();
            t[n..].starts_with(' ').then_some(n)
        } else {
            None
        };
        if inside {
            if matches!(heading_level, Some(n) if n <= level) {
                break;
            }
            out.push(l);
        } else if heading_level == Some(level) && t.trim_end() == want {
            inside = true;
        }
    }
    assert!(inside, "no `{want}` heading");
    out.join("\n")
}

fn tables(bs: &[Block]) -> Vec<&Vec<Vec<String>>> {
    bs.iter().filter_map(|b| if let Block::Table(t) = b { Some(t) } else { None }).collect()
}

fn codes(bs: &[Block]) -> Vec<&str> {
    bs.iter().filter_map(|b| if let Block::Code(_, s) = b { Some(s.as_str()) } else { None }).collect()
}

fn h2s(bs: &[Block]) -> Vec<String> {
    bs.iter().filter_map(|b| if let Block::Heading(2, s) = b { Some(s.clone()) } else { None }).collect()
}

fn subsections(bs: &[Block]) -> Vec<String> {
    bs.iter().filter_map(|b| if let Block::Heading(3, s) = b { Some(s.clone()) } else { None }).collect()
}

fn table_text(t: &[Vec<String>]) -> String {
    t.iter().map(|r| r.join(" | ")).collect::<Vec<_>>().join("\n")
}

fn is_separator(row: &[String]) -> bool {
    !row.is_empty() && row.iter().all(|c| c == "---")
}

// ---------- position and structure ----------

#[test]
fn zo_section_follows_background_service_in_english() {
    let h = h2s(&blocks(&en()));
    assert_eq!(h, ["Topology", "The anchor's address", "Background service", "Zo Computer", "Files", "Platforms", "Updating"], "{h:?}");
}

#[test]
fn zo_section_follows_background_service_in_spanish() {
    let h = h2s(&blocks(&es()));
    assert_eq!(h, ["Topología", "La dirección del ancla", "Servicio en segundo plano", "Zo Computer", "Archivos", "Plataformas", "Actualizar"], "{h:?}");
}

#[test]
fn zo_subsections_are_peer_then_anchor() {
    assert_eq!(subsections(&section(&blocks(&en()), 2, "Zo Computer")), ["Peer", "Anchor"]);
    assert_eq!(subsections(&section(&blocks(&es()), 2, "Zo Computer")), ["Peer", "Ancla"]);
}

/// Same headings levels, tables (rows and columns) and code blocks, in the same order.
#[test]
fn zo_section_has_same_structure_in_both_languages() {
    fn skeleton(bs: &[Block]) -> Vec<String> {
        bs.iter()
            .map(|b| match b {
                Block::Heading(l, _) => format!("h{l}"),
                Block::Table(t) => format!("table {}x{}", t.len(), t.iter().map(|r| r.len()).max().unwrap_or(0)),
                Block::Code(info, _) => format!("code {info}"),
            })
            .collect()
    }
    let e = skeleton(&section(&blocks(&en()), 2, "Zo Computer"));
    let s = skeleton(&section(&blocks(&es()), 2, "Zo Computer"));
    assert_eq!(e, s, "English and Spanish Zo sections differ in structure");
}

#[test]
fn each_zo_subsection_has_a_table_in_the_doc_format() {
    for (md, subs) in [(en(), ["Peer", "Anchor"]), (es(), ["Peer", "Ancla"])] {
        let zo = section(&blocks(&md), 2, "Zo Computer");
        for sub in subs {
            let s = section(&zo, 3, sub);
            let ts = tables(&s);
            assert!(!ts.is_empty(), "### {sub}: no table");
            for t in ts {
                assert!(t.len() >= 3, "### {sub}: table needs header, separator and rows:\n{}", table_text(t));
                assert!(is_separator(&t[1]), "### {sub}: separator must be `| --- | --- |`:\n{}", table_text(t));
                assert!(t.iter().all(|r| r.len() == t[0].len()), "### {sub}: ragged table:\n{}", table_text(t));
            }
        }
    }
}

// ---------- peer ----------

fn peer(md: &str) -> (Vec<Block>, String) {
    let zo = section(&blocks(md), 2, "Zo Computer");
    (section(&zo, 3, "Peer"), section_text(&section_text(md, 2, "Zo Computer"), 3, "Peer"))
}

#[test]
fn peer_table_lists_the_zo_service_fields() {
    for md in [en(), es()] {
        let (bs, _) = peer(&md);
        let all: String = tables(&bs).iter().map(|t| table_text(t)).collect::<Vec<_>>().join("\n");
        for field in ["label", "mode", "entrypoint", "workdir"] {
            assert!(all.contains(field), "peer table lacks field `{field}`:\n{all}");
        }
        for lit in ["`hilo`", "`process`", "/home/workspace"] {
            assert!(all.contains(lit), "peer table lacks {lit}:\n{all}");
        }
        assert!(all.contains("hilo daemon"), "peer entrypoint must be `<path to hilo> daemon`:\n{all}");
        for var in ["HILO_HOME", "HILO_KEYCHAIN", "HILO_NOTIFY"] {
            assert!(all.contains(var), "peer table lacks env var {var}:\n{all}");
        }
    }
}

/// Same lines `zo_instructions` prints: `label: hilo`, `mode: process`, `workdir: /home/workspace`.
#[test]
fn peer_fields_match_what_zo_instructions_prints() {
    for md in [en(), es()] {
        let (bs, _) = peer(&md);
        let rows: Vec<&Vec<String>> = tables(&bs).into_iter().flat_map(|t| t.iter().skip(2)).collect();
        let value = |field: &str| -> String {
            rows.iter()
                .find(|r| r[0].trim_matches('`') == field)
                .unwrap_or_else(|| panic!("no `{field}` row in peer table"))
                .iter()
                .skip(1)
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(value("label").contains("`hilo`"));
        assert!(value("mode").contains("`process`"));
        assert!(value("workdir").contains("/home/workspace"));
        let ep = value("entrypoint");
        assert!(ep.contains("hilo daemon"), "entrypoint: {ep}");
    }
}

#[test]
fn peer_explains_join_setup_detection_log_and_restart() {
    for (md, restart) in [(en(), "restart"), (es(), "reinici")] {
        let (_, text) = peer(&md);
        for lit in ["hilo join", "setup", "~/.hilo/daemon.log", "/home/workspace"] {
            assert!(text.contains(lit), "peer text lacks {lit}:\n{text}");
        }
        assert!(text.to_lowercase().contains(restart), "peer must warn the process dies on a Zo restart:\n{text}");
    }
}

// ---------- anchor ----------

#[test]
fn anchor_registers_process_and_tcp_services() {
    for (md, sub) in [(en(), "Anchor"), (es(), "Ancla")] {
        let zo = section(&blocks(&md), 2, "Zo Computer");
        let a = section(&zo, 3, sub);
        let all: String = tables(&a).iter().map(|t| table_text(t)).collect::<Vec<_>>().join("\n");
        assert!(all.contains("tcp"), "### {sub} table lacks tcp:\n{all}");
        assert!(all.contains("7710"), "### {sub} table lacks 7710:\n{all}");
        let text = section_text(&section_text(&md, 2, "Zo Computer"), 3, sub);
        assert!(text.contains("process"), "### {sub} must say the daemon is a `process` service too:\n{text}");
    }
}

#[test]
fn anchor_has_the_init_command_in_a_code_block() {
    for (md, sub, cmd) in [
        (en(), "Anchor", "hilo init --listen <zo-host>:<public-port> --public --bind 0.0.0.0:7710"),
        (es(), "Ancla", "hilo init --listen <host-zo>:<puerto-público> --public --bind 0.0.0.0:7710"),
    ] {
        let zo = section(&blocks(&md), 2, "Zo Computer");
        let a = section(&zo, 3, sub);
        let cs = codes(&a);
        assert!(cs.iter().any(|c| c.lines().any(|l| l.trim() == cmd)), "### {sub}: no code block with `{cmd}`: {cs:?}");
    }
}

#[test]
fn anchor_explains_listen_takes_an_ip_and_why_bind() {
    for (md, sub) in [(en(), "Anchor"), (es(), "Ancla")] {
        let text = section_text(&section_text(&md, 2, "Zo Computer"), 3, sub);
        assert!(text.contains("getent hosts"), "### {sub}: no `getent hosts <host>` hint:\n{text}");
        assert!(text.contains("--listen"), "### {sub}: no --listen note:\n{text}");
        assert!(text.contains("IP"), "### {sub}: must say --listen takes an IP:\n{text}");
        // the note about --bind sits outside the command itself
        let prose: String = text.lines().filter(|l| !l.contains("hilo init")).collect::<Vec<_>>().join("\n");
        assert!(prose.contains("--bind") && prose.contains("7710"), "### {sub}: must explain why --bind (Zo's public port is not 7710):\n{text}");
    }
}

// ---------- background service table ----------

#[test]
fn background_service_table_has_a_zo_computer_row() {
    for (md, title) in [(en(), "Background service"), (es(), "Servicio en segundo plano")] {
        let bs = blocks(&md);
        let s = section(&bs, 2, title);
        let t = tables(&s).into_iter().next().unwrap_or_else(|| panic!("{title}: no table"));
        assert!(is_separator(&t[1]));
        let row = t.iter().find(|r| r[0] == "Zo Computer").unwrap_or_else(|| panic!("{title}: no `Zo Computer` row:\n{}", table_text(t)));
        assert_eq!(row.len(), t[0].len(), "{title}: Zo row has the wrong column count");
        let r = row.join(" | ");
        for lit in ["process", "~/.hilo/daemon.log", "systemd", "#zo-computer"] {
            assert!(r.contains(lit), "{title}: Zo row lacks {lit}: {r}");
        }
        // existing rows untouched
        for os in ["macOS", "Linux", "Windows"] {
            assert!(t.iter().any(|r| r[0] == os), "{title}: lost the {os} row");
        }
    }
}

// ---------- links ----------

#[test]
fn language_links_still_work() {
    let e = en();
    let s = es();
    assert!(e.contains(r#"<a href="deployment.es.md">Español</a>"#));
    assert!(s.contains(r#"<a href="deployment.md">English</a>"#));
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
    assert!(dir.join("deployment.md").exists() && dir.join("deployment.es.md").exists());
}
