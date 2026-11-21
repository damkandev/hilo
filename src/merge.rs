//! Three-way line merge, for two agents that edited the same version of a text file.
//! Conservative on purpose: any doubt (overlapping or touching edits, binary, too big, a lockfile,
//! JSON that no longer parses) answers None, and the usual rule applies: first write wins, the
//! other version is kept. It never writes conflict markers.
#![forbid(unsafe_code)]
use crate::grep::is_binary;
use crate::json::Json;

/// Bigger files fall back to the usual rule (Myers is O(N·D)).
const MAX: usize = 1 << 20;
/// Edit distance at which a side stops counting as "an edit" (also bounds the trace memory, ~D²).
const MAX_D: isize = 1000;
/// Generated or machine-merged by their own tools: a line merge would produce nonsense.
const NEVER: [&str; 12] = [
    "Cargo.lock", "package-lock.json", "npm-shrinkwrap.json", "yarn.lock", "pnpm-lock.yaml", "bun.lock",
    "poetry.lock", "uv.lock", "Gemfile.lock", "composer.lock", "go.sum", "flake.lock",
];

/// `base` with both `ours` and `theirs` applied, or None if they can't both be kept safely.
pub fn merge(path: &str, base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if NEVER.contains(&name) || [".ipynb", ".min.js", ".min.css"].iter().any(|e| name.ends_with(e)) {
        return None;
    }
    let out = merge3(base, ours, theirs)?;
    if name.ends_with(".json") && std::str::from_utf8(&out).ok().and_then(Json::parse).is_none() {
        return None; // two clean edits can still leave a stray comma
    }
    Some(out)
}

fn merge3(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    if [base, ours, theirs].iter().any(|d| d.len() > MAX || is_binary(d)) {
        return None;
    }
    let (b, o, t) = (lines(base), lines(ours), lines(theirs));
    let (mo, mt) = (matches(&b, &o)?, matches(&b, &t)?);
    let mut out = Vec::with_capacity(ours.len().max(theirs.len()));
    let (mut pb, mut po, mut pt) = (0, 0, 0);
    // a base line both sides kept is a sync point; between two of them, at most one side may differ
    for i in 0..=b.len() {
        let Some((io, it)) = (if i == b.len() { Some((o.len(), t.len())) } else { mo[i].zip(mt[i]) }) else { continue };
        let (cb, co, ct) = (&b[pb..i], &o[po..io], &t[pt..it]);
        let take = if co == cb {
            ct
        } else if ct == cb || co == ct {
            co
        } else {
            return None; // both changed the same stretch (or lines touching it)
        };
        take.iter().for_each(|l| out.extend_from_slice(l));
        if i < b.len() {
            out.extend_from_slice(b[i]);
        }
        (pb, po, pt) = (i + 1, io + 1, it + 1);
    }
    Some(out)
}

/// Lines with their terminators, so CRLF and a missing final newline come back byte for byte.
fn lines(d: &[u8]) -> Vec<&[u8]> {
    d.split_inclusive(|&c| c == b'\n').collect()
}

/// For each line of `a`, where it sits in `b` in a longest common subsequence. None if too different.
fn matches(a: &[&[u8]], b: &[&[u8]]) -> Option<Vec<Option<usize>>> {
    let mut m = vec![None; a.len()];
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        m[pre] = Some(pre);
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf] {
        m[a.len() - 1 - suf] = Some(b.len() - 1 - suf);
        suf += 1;
    }
    for (x, y) in myers(&a[pre..a.len() - suf], &b[pre..b.len() - suf])? {
        m[pre + x] = Some(pre + y);
    }
    Some(m)
}

/// Matched (x, y) pairs of a shortest edit script, in order (Myers 1986, greedy, with a trace).
fn myers(x: &[&[u8]], y: &[&[u8]]) -> Option<Vec<(usize, usize)>> {
    let (n, m) = (x.len() as isize, y.len() as isize);
    let off = n + m + 1;
    let mut v = vec![0isize; 2 * off as usize + 1];
    let at = |k: isize| (off + k) as usize;
    let mut trace: Vec<Vec<isize>> = vec![];
    let mut end = None;
    'search: for d in 0..=(n + m).min(MAX_D) {
        trace.push(v[at(-d)..=at(d)].to_vec()); // the frontier before step d, k in -d..=d
        for k in (-d..=d).step_by(2) {
            let mut px = if k == -d || (k != d && v[at(k - 1)] < v[at(k + 1)]) { v[at(k + 1)] } else { v[at(k - 1)] + 1 };
            let mut py = px - k;
            while px < n && py < m && x[px as usize] == y[py as usize] {
                (px, py) = (px + 1, py + 1);
            }
            v[at(k)] = px;
            if px >= n && py >= m {
                end = Some(d);
                break 'search;
            }
        }
    }
    let (mut cx, mut cy, mut pairs) = (n, m, vec![]);
    for d in (1..=end?).rev() {
        let t = &trace[d as usize];
        let get = |k: isize| t[(k + d) as usize];
        let k = cx - cy;
        let pk = if k == -d || (k != d && get(k - 1) < get(k + 1)) { k + 1 } else { k - 1 };
        let (px, py) = (get(pk), get(pk) - pk);
        let (sx, sy) = if pk == k + 1 { (px, py + 1) } else { (px + 1, py) };
        while cx > sx && cy > sy {
            (cx, cy) = (cx - 1, cy - 1);
            pairs.push((cx as usize, cy as usize));
        }
        (cx, cy) = (px, py);
    }
    while cx > 0 && cy > 0 {
        (cx, cy) = (cx - 1, cy - 1);
        pairs.push((cx as usize, cy as usize));
    }
    pairs.reverse();
    Some(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn m(b: &str, o: &str, t: &str) -> Option<String> {
        merge("f.txt", b.as_bytes(), o.as_bytes(), t.as_bytes()).map(|v| String::from_utf8(v).unwrap())
    }

    #[test]
    fn separate_edits_merge() {
        let base = "a\nb\nc\nd\ne\n";
        assert_eq!(m(base, "A\nb\nc\nd\ne\n", "a\nb\nc\nd\nE\n").as_deref(), Some("A\nb\nc\nd\nE\n"));
        // inserts at both ends, a delete in the middle
        assert_eq!(m(base, "top\na\nb\nc\nd\ne\n", "a\nb\nd\ne\nend\n").as_deref(), Some("top\na\nb\nd\ne\nend\n"));
        // the same edit on both sides is not a conflict
        assert_eq!(m(base, "a\nB\nc\nd\ne\n", "a\nB\nc\nd\ne\n").as_deref(), Some("a\nB\nc\nd\ne\n"));
        // one side unchanged: the other wins outright
        assert_eq!(m(base, base, "x\n").as_deref(), Some("x\n"));
    }

    #[test]
    fn overlapping_or_touching_edits_refuse() {
        let base = "a\nb\nc\nd\ne\n";
        assert_eq!(m(base, "a\nX\nc\nd\ne\n", "a\nY\nc\nd\ne\n"), None, "same line");
        assert_eq!(m(base, "a\nX\nc\nd\ne\n", "a\nb\nY\nd\ne\n"), None, "adjacent lines");
        assert_eq!(m(base, "a\nb\nnew1\nc\nd\ne\n", "a\nb\nnew2\nc\nd\ne\n"), None, "two inserts at the same spot");
        assert_eq!(m(base, "", "a\nb\nC\nd\ne\n"), None, "delete vs edit");
        assert_eq!(m("", "x\n", "y\n"), None, "both created it");
    }

    #[test]
    fn bytes_survive() {
        // CRLF and a missing final newline are kept as each side wrote them
        assert_eq!(m("a\r\nb\r\nc\r\nd", "A\r\nb\r\nc\r\nd", "a\r\nb\r\nc\r\nD").as_deref(), Some("A\r\nb\r\nc\r\nD"));
        assert_eq!(m("a\nb\nc\nd", "a\nb\nc\nd\ne\n", "Z\nb\nc\nd").as_deref(), Some("Z\nb\nc\nd\ne\n"));
        assert_eq!(m("a\nb\nc\nd\n", "a\nb\nc\nd\ne", "Z\nb\nc\nd\n").as_deref(), Some("Z\nb\nc\nd\ne"));
        // non-UTF-8 text is fine, binary is not
        assert!(merge3(b"\xff\n1\n2\n3\n", b"\xfe\n1\n2\n3\n", b"\xff\n1\n2\n4\n").is_some());
        assert_eq!(merge3(b"a\0\n1\n2\n", b"b\0\n1\n2\n", b"a\0\n1\n3\n"), None);
    }

    #[test]
    fn guards() {
        let b = "{\n\"a\": 1,\n\"b\": 2,\n\"c\": 3\n}\n";
        let t = b.replace("\"c\": 3", "\"c\": 4");
        assert!(merge("x.json", b.as_bytes(), b.replace("\"a\": 1,", "\"a\": 9,").as_bytes(), t.as_bytes()).is_some());
        // clean as lines, broken as JSON (one side dropped a comma): the usual rule decides
        let o = b.replace("\"a\": 1,", "\"a\": 1");
        assert!(merge3(b.as_bytes(), o.as_bytes(), t.as_bytes()).is_some());
        assert!(merge("x.json", b.as_bytes(), o.as_bytes(), t.as_bytes()).is_none());
        assert!(merge("Cargo.lock", b"a\nb\nc\n", b"A\nb\nc\n", b"a\nb\nC\n").is_none());
        assert!(merge("x/package-lock.json", b"a\nb\nc\n", b"A\nb\nc\n", b"a\nb\nC\n").is_none());
        let big = "l\n".repeat(MAX);
        assert!(merge3(big.as_bytes(), b"", b"").is_none());
        // too many changed lines on one side
        let base: String = (0..3000).map(|i| format!("{i}\n")).collect();
        let ours: String = (0..3000).map(|i| if i % 2 == 0 { format!("x{i}\n") } else { format!("{i}\n") }).collect();
        assert!(merge3(base.as_bytes(), ours.as_bytes(), base.as_bytes()).is_none());
    }

    #[test]
    fn myers_finds_a_longest_common_subsequence() {
        let ls = |s: &'static str| -> Vec<&[u8]> { s.as_bytes().split_inclusive(|&c| c == b'\n').collect() };
        for (a, b, n) in [("a\nb\nc\n", "a\nc\n", 2), ("", "x\n", 0), ("x\n", "", 0), ("a\nb\nc\nd\n", "b\nx\nd\na\n", 2), ("a\na\na\n", "a\na\n", 2)] {
            let (x, y) = (ls(a), ls(b));
            let p = myers(&x, &y).unwrap();
            assert_eq!(p.len(), n, "{a:?} {b:?}");
            assert!(p.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1) && p.iter().all(|&(i, j)| x[i] == y[j]));
        }
    }
}
