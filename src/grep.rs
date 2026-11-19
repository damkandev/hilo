//! Literal search over files hilo already tracks (`hilo grep`, the MCP `search` tool).
#![forbid(unsafe_code)]
use std::path::PathBuf;

pub const MAX_RESULTS: usize = 200;
const MAX_SIZE: u64 = 4 << 20;
const MAX_LINE: usize = 300;

/// NUL in the first 8 KB, like git.
pub fn is_binary(d: &[u8]) -> bool {
    d[..d.len().min(8192)].contains(&0)
}

/// `shown:line: text` for each line containing `pattern`, at most `max`.
/// `files` are (path on disk, name to show). Binary files and files over 4 MB are skipped.
pub fn search(files: &[(PathBuf, String)], pattern: &str, ignore_case: bool, max: usize) -> Vec<String> {
    let mut out = vec![];
    if pattern.is_empty() {
        return out;
    }
    // ponytail: ASCII case folding only (é ≠ É); add Unicode folding if agents search non-English text.
    let pat = if ignore_case { pattern.to_ascii_lowercase() } else { pattern.to_string() };
    for (p, shown) in files {
        let Ok(m) = std::fs::symlink_metadata(p) else { continue };
        if !m.is_file() || m.len() > MAX_SIZE {
            continue;
        }
        let Ok(data) = std::fs::read(p) else { continue };
        if is_binary(&data) {
            continue;
        }
        for (i, line) in String::from_utf8_lossy(&data).lines().enumerate() {
            let at = if ignore_case { line.to_ascii_lowercase().find(&pat) } else { line.find(&pat) };
            if let Some(at) = at {
                out.push(format!("{shown}:{}: {}", i + 1, clip(line, at)));
                if out.len() >= max {
                    return out;
                }
            }
        }
    }
    out
}

/// The text `hilo grep` and MCP `search` answer with.
pub fn answer(files: &[(PathBuf, String)], pattern: &str, ignore_case: bool) -> String {
    let mut hits = search(files, pattern, ignore_case, MAX_RESULTS);
    if hits.len() == MAX_RESULTS {
        hits.push(format!("[stopped at {MAX_RESULTS} results; narrow the folder or the pattern]"));
    }
    if hits.is_empty() { "no matches".into() } else { hits.join("\n") }
}

/// At most MAX_LINE bytes of `line` around the match at byte `at`.
fn clip(line: &str, at: usize) -> String {
    let line = line.trim_end();
    if line.len() <= MAX_LINE {
        return line.to_string();
    }
    let floor = |mut i: usize| {
        while !line.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let start = floor(at.saturating_sub(MAX_LINE / 3));
    let end = floor((start + MAX_LINE).min(line.len()));
    format!("{}{}{}", if start > 0 { "…" } else { "" }, &line[start..end], if end < line.len() { "…" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grep() {
        let d = std::env::temp_dir().join(format!("hilo-grep-{}", crate::crypto::hex(&crate::crypto::random::<4>())));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.txt"), "one Needle\ntwo\nneedle three\n").unwrap();
        std::fs::write(d.join("b.bin"), b"needle\0binary").unwrap();
        std::fs::write(d.join("long.txt"), format!("{}needle{}", "x".repeat(1000), "é".repeat(500))).unwrap();
        let files: Vec<(PathBuf, String)> = ["a.txt", "b.bin", "long.txt", "missing"].iter().map(|n| (d.join(n), n.to_string())).collect();
        assert_eq!(search(&files[..2], "needle", false, 10), ["a.txt:3: needle three"]);
        assert_eq!(search(&files[..2], "NEEDLE", true, 10), ["a.txt:1: one Needle", "a.txt:3: needle three"]);
        assert_eq!(search(&files, "needle", true, 1).len(), 1);
        let long = search(&files[2..], "needle", false, 10);
        assert!(long[0].contains("needle") && long[0].len() < 320 + "long.txt:1: ".len(), "{long:?}");
        assert!(search(&files, "", false, 10).is_empty() && search(&files, "nope", false, 10).is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
