//! Anchor GC: `gc_blobs`, `compact_log`, `Core::gc`, the `Tick` counter and the GC `start` runs.
//! Child module of `anchor` (sees its private items). Wire it from src/anchor.rs with:
//!     #[cfg(test)]
//!     mod gc_tests;
//! Every test points HILO_HOME at its own temp dir under `config::TEST_HOME`, like
//! `anchor::tests::scoped_sandbox_sees_only_its_folder`.

use super::*;
use crate::config::{self, Peer};
use crate::crypto::hex;
use crate::lease;
use crate::sync::{hash_of, ref_hash, ref_payload, Data, Hash, Key, Op, Reject, Sequencer, NONE, REF};
use crate::wire::{Msg, Rd, W};
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

const NOTES: &str = "drive:notes";
/// The machine that writes the history; its ops are stamped with this origin.
const WRITER: u64 = 1;

/// A fresh HILO_HOME for one test. Dropping it restores the variable and removes the directory,
/// also when the test panics.
struct Home {
    dir: PathBuf,
    old: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Home {
    fn new() -> Home {
        let lock = config::TEST_HOME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("hilo-anchor-gc-test-{}-{}", std::process::id(), hex(&crate::crypto::random::<4>())));
        for d in ["anchor/blobs", "tmp", "secrets", "vault", "index"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let old = std::env::var_os("HILO_HOME");
        std::env::set_var("HILO_HOME", &dir);
        // every machine id the tests connect with is a paired machine with key [id; 32]
        let peers: Vec<Peer> = (1..=60u8).map(|i| Peer { key: [i; 32], name: name_of(i as u64), ip: "-".into(), tag: String::new() }).collect();
        config::save_peers(&peers).unwrap();
        Home { dir, old, _lock: lock }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        match self.old.take() {
            Some(o) => std::env::set_var("HILO_HOME", o),
            None => std::env::remove_var("HILO_HOME"),
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn name_of(id: u64) -> String {
    if id == WRITER { "w".into() } else { format!("m{id}") }
}

fn new_core() -> Core {
    let (seq, origins, log) = open_log().unwrap();
    Core { seq, origins, log, clients: HashMap::new(), leases: lease::Table::default(), sessions: HashMap::new(), ticks: 0 }
}

fn connect(core: &mut Core, id: u64) -> Receiver<Msg> {
    let (tx, rx) = channel();
    core.handle(In::Connect { id, tx, key: [id as u8; 32], ip: "-".into(), stream: None });
    rx
}

fn hello(core: &mut Core, id: u64, last_seq: u64) {
    core.handle(In::Msg(id, Msg::Hello { last_seq, name: name_of(id) }));
}

/// A new machine connects, says Hello and leaves: the ops of its catch-up, in the order sent.
fn catch_up(core: &mut Core, id: u64, last_seq: u64) -> Vec<Op> {
    let rx = connect(core, id);
    hello(core, id, last_seq);
    let msgs: Vec<Msg> = rx.try_iter().collect();
    assert!(msgs.contains(&Msg::CaughtUp), "machine {id} got no CaughtUp after Hello {{ last_seq: {last_seq} }}: {msgs:?}");
    core.handle(In::Gone(id));
    msgs.into_iter().filter_map(|m| if let Msg::Op(o) = m { Some(o) } else { None }).collect()
}

fn h(d: &[u8]) -> Hash {
    hash_of(Some(d))
}

fn key(wt: &str, path: &str) -> Key {
    (wt.to_string(), path.to_string())
}

/// A whole-file write on top of `base`.
fn write(wt: &str, path: &str, base: Hash, body: &[u8]) -> Op {
    Op { seq: 0, wt: wt.into(), path: path.into(), base, new: h(body), data: Data::Full(body.to_vec()), origin: String::new(), t_event: 0, t_seq: 0 }
}

/// A delete of the version `base`: a tombstone.
fn delete(wt: &str, path: &str, base: Hash) -> Op {
    Op { seq: 0, wt: wt.into(), path: path.into(), base, new: NONE, data: Data::Absent, origin: String::new(), t_event: 0, t_seq: 0 }
}

fn send(core: &mut Core, from: u64, op: Op) {
    core.handle(In::Msg(from, Msg::Op(op)));
}

/// More than DELTA_MIN bytes, not compressible to nothing.
fn big() -> Vec<u8> {
    (0..DELTA_MIN * 3).map(|i| (i * 7 % 251) as u8).collect()
}

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const MID: u64 = 3; // a/1's last version: a catch-up from here skips a/1 and gets the rest

/// What the history leaves behind.
struct Scenario {
    wrx: Receiver<Msg>,
    /// Current content of every key (None = tombstone), by key.
    content: HashMap<Key, Option<Vec<u8>>>,
    /// Blobs no key points at any more.
    orphans: Vec<Hash>,
}

/// The acceptance scenario: several versions of a file, a deleted key, keys sharing one blob
/// (one of them deleted later), an old version kept alive by another key, a vault value, a branch
/// tip and a file above DELTA_MIN. 14 ops, 10 keys, 2 orphan blobs.
fn scenario(core: &mut Core) -> Scenario {
    let wrx = connect(core, WRITER);
    hello(core, WRITER, 0);
    let refp = ref_payload(SHA, false, None);
    let mut refop = write("R:main", REF, NONE, &refp);
    refop.new = ref_hash(SHA);
    let ops = vec![
        write(NOTES, "a/1", NONE, b"v1"),                 // 1
        write(NOTES, "a/1", h(b"v1"), b"v2"),             // 2
        write(NOTES, "a/1", h(b"v2"), b"v3"),             // 3 = MID
        write(NOTES, "a/gone", NONE, b"soon gone"),       // 4
        delete(NOTES, "a/gone", h(b"soon gone")),         // 5
        write(NOTES, "a/twin1", NONE, b"shared"),         // 6
        write(NOTES, "a/twin2", NONE, b"shared"),         // 7
        write("vault:R", "a/.env\nK", NONE, b"sealed"),   // 8
        write(NOTES, "a/big.bin", NONE, &big()),          // 9
        write("drive:other", "a/copy", NONE, b"v1"),      // 10: a/1's first version lives on here
        write(NOTES, "a/twin3", NONE, b"shared"),         // 11
        delete(NOTES, "a/twin3", h(b"shared")),           // 12: its blob is still twin1's and twin2's
        refop,                                            // 13
        write(NOTES, "a/empty", NONE, b""),               // 14: an empty file is not a tombstone
    ];
    let n = ops.len() as u64;
    for op in ops {
        send(core, WRITER, op);
    }
    let content: HashMap<Key, Option<Vec<u8>>> = [
        (key(NOTES, "a/1"), Some(b"v3".to_vec())),
        (key(NOTES, "a/gone"), None),
        (key(NOTES, "a/twin1"), Some(b"shared".to_vec())),
        (key(NOTES, "a/twin2"), Some(b"shared".to_vec())),
        (key("vault:R", "a/.env\nK"), Some(b"sealed".to_vec())),
        (key(NOTES, "a/big.bin"), Some(big())),
        (key("drive:other", "a/copy"), Some(b"v1".to_vec())),
        (key(NOTES, "a/twin3"), None),
        (key("R:main", REF), Some(refp)),
        (key(NOTES, "a/empty"), Some(vec![])),
    ]
    .into_iter()
    .collect();
    // the harness itself: every op of the history was accepted
    assert_eq!(core.seq.seq, n, "scenario: not every op was accepted");
    assert_eq!(core.seq.state.len(), content.len(), "scenario: unexpected keys");
    assert_eq!(core.seq.state[&key(NOTES, "a/1")].0, MID);
    assert_eq!(records().len() as u64, n, "scenario: one log record per accepted op before compaction");
    wrx.try_iter().count();
    Scenario { wrx, content, orphans: vec![h(b"v2"), h(b"soon gone")] }
}

fn blobs_dir() -> PathBuf {
    config::path("anchor/blobs")
}

/// Every entry of anchor/blobs, by name.
fn blob_names() -> BTreeSet<String> {
    std::fs::read_dir(blobs_dir()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
}

/// The names GC must leave: the current version of every key.
fn live_names(seq: &Sequencer) -> BTreeSet<String> {
    seq.state.values().filter(|v| v.1 != NONE).map(|v| hex(&v.1)).collect()
}

/// Leaves three `.tmp` files in anchor/blobs (a crashed write of a dead blob, one of a live blob
/// and a loose one); returns their names and total size.
fn plant_tmps(seq: &Sequencer) -> (Vec<String>, u64) {
    let live = live_names(seq).into_iter().next().unwrap();
    let files = [(format!("{}.tmp", hex(&h(b"never finished"))), &b"half a blob"[..]), (format!("{live}.tmp"), &b"half of a live one"[..]), ("suelto.tmp".to_string(), &b"x"[..])];
    for (name, body) in &files {
        std::fs::write(blobs_dir().join(name), body).unwrap();
    }
    (files.iter().map(|f| f.0.clone()).collect(), files.iter().map(|f| f.1.len() as u64).sum())
}

type Rec = (u64, String, String, Hash, String);

/// anchor/log2, record by record; panics if it doesn't parse to the last byte.
fn records() -> Vec<Rec> {
    let bytes = std::fs::read(config::path("anchor/log2")).unwrap();
    let mut r = Rd(&bytes);
    let mut v = vec![];
    while !r.0.is_empty() {
        let rec = (|| Some((r.u64()?, r.str()?, r.str()?, r.hash()?, r.str()?)))();
        v.push(rec.unwrap_or_else(|| panic!("anchor/log2 has a record that doesn't parse after {} good ones", v.len())));
    }
    v
}

/// The records a compacted log holds for this state: one per key, by seq.
fn compacted(seq: &Sequencer, origins: &HashMap<Key, String>) -> Vec<Rec> {
    seq.since(0).into_iter().map(|(k, s, hash)| (s, k.0.clone(), k.1.clone(), hash, origins.get(&k).cloned().unwrap_or_default())).collect()
}

/// `open_log()` now gives exactly this state.
fn assert_log_replays(seq: &Sequencer, origins: &HashMap<Key, String>, when: &str) {
    let (s, o, _) = open_log().unwrap();
    assert_eq!(s.seq, seq.seq, "{when}: open_log gives another seq");
    assert_eq!(s.state, seq.state, "{when}: open_log gives another state");
    assert_eq!(&o, origins, "{when}: open_log gives other origins");
}

/// Catch-up from 0, from MID and from a later point, as three new machines see it.
fn catch_ups(core: &mut Core, first_id: u64) -> Vec<Vec<Op>> {
    [0, MID, 9].iter().enumerate().map(|(i, last)| catch_up(core, first_id + i as u64, *last)).collect()
}

/// What catch-up has to look like for the scenario, GC or not.
fn assert_catch_up_shape(core: &Core, sc: &Scenario, ups: &[Vec<Op>]) {
    let seqs = |v: &[Op]| v.iter().map(|o| o.seq).collect::<Vec<_>>();
    assert_eq!(seqs(&ups[0]), vec![3, 5, 6, 7, 8, 9, 10, 12, 13, 14], "catch-up from 0: one op per key, by seq");
    assert_eq!(seqs(&ups[1]), vec![5, 6, 7, 8, 9, 10, 12, 13, 14], "catch-up from {MID}");
    assert_eq!(seqs(&ups[2]), vec![10, 12, 13, 14], "catch-up from 9");
    for o in &ups[0] {
        let k = key(&o.wt, &o.path);
        assert_eq!(o.new, core.seq.state[&k].1, "{k:?}");
        assert_eq!(o.origin, "w", "{k:?}");
        match &sc.content[&k] {
            None => assert_eq!((o.new, &o.data), (NONE, &Data::Absent), "the delete of {k:?} travels as a tombstone"),
            // branch tips and vault values travel whole in catch-up: their blobs are read
            Some(d) if k.1 == REF || k.0.starts_with("vault:") => assert_eq!(o.data, Data::Full(d.clone()), "{k:?}"),
            Some(_) => assert_eq!(o.data, Data::Omitted, "{k:?}"),
        }
    }
}

/// After GC the anchor works as before: a plain op, a delta on the current version, NeedFull
/// for every key, a conflict, and content whose blob was collected coming back.
fn assert_still_works(core: &mut Core, sc: &Scenario, reader_id: u64) {
    let wrx = &sc.wrx;
    let rrx = connect(core, reader_id);
    hello(core, reader_id, core.seq.seq);
    rrx.try_iter().count();
    wrx.try_iter().count();
    let ops = |rx: &Receiver<Msg>| rx.try_iter().filter(|m| matches!(m, Msg::Op(_) | Msg::Reject(_))).collect::<Vec<Msg>>();
    let need = |core: &mut Core, wt: &str, path: &str| {
        core.handle(In::Msg(WRITER, Msg::NeedFull { wt: wt.into(), path: path.into() }));
        ops(wrx)
    };
    let full = |core: &Core, wt: &str, path: &str, body: &[u8]| {
        let (seq, hash) = core.seq.state[&key(wt, path)];
        vec![Msg::Op(Op { seq, wt: wt.into(), path: path.into(), base: NONE, new: hash, data: Data::Full(body.to_vec()), origin: "w".into(), t_event: 0, t_seq: 0 })]
    };

    // NeedFull: the current content of every key the scenario left
    for (k, body) in &sc.content {
        let got = need(core, &k.0, &k.1);
        match body {
            Some(d) => assert_eq!(got, full(core, &k.0, &k.1, d), "NeedFull of {k:?} after GC"),
            None => match &got[..] {
                [Msg::Op(o)] => assert_eq!((o.new, &o.data), (NONE, &Data::Absent), "NeedFull of the deleted {k:?} after GC"),
                _ => panic!("NeedFull of the deleted {k:?} after GC: {got:?}"),
            },
        }
    }

    // a plain op
    let next = core.seq.seq + 1;
    send(core, WRITER, write(NOTES, "a/new", NONE, b"fresh"));
    assert_eq!(core.seq.state.get(&key(NOTES, "a/new")), Some(&(next, h(b"fresh"))), "a plain op after GC was not accepted");
    match &ops(wrx)[..] {
        [Msg::Op(o)] => assert_eq!((o.seq, o.path.as_str(), o.new, &o.data), (next, "a/new", h(b"fresh"), &Data::Omitted), "the author's copy of a plain op after GC"),
        got => panic!("the author of a plain op after GC got {got:?}"),
    }
    match &ops(&rrx)[..] {
        [Msg::Op(o)] => assert_eq!((o.seq, o.path.as_str(), &o.data), (next, "a/new", &Data::Full(b"fresh".to_vec())), "another machine's copy of a plain op after GC"),
        got => panic!("another machine got {got:?} for a plain op after GC"),
    }

    // a delta on the current version of a file above DELTA_MIN
    let (old, mut new) = (big(), big());
    new.extend_from_slice(b"one more line");
    let mut d = write(NOTES, "a/big.bin", h(&old), &new);
    d.data = Data::delta(&old, &new);
    assert!(old.len() > DELTA_MIN && matches!(&d.data, Data::Delta(p) if p.len() < new.len() / 2));
    send(core, WRITER, d);
    assert_eq!(core.seq.current(NOTES, "a/big.bin"), h(&new), "a delta on the current version after GC was not accepted");
    assert_eq!(read_blob(&h(&new)).unwrap(), Some(new.clone()), "the delta's result is not stored");
    assert!(matches!(&ops(wrx)[..], [Msg::Op(o)] if o.new == h(&new)), "the author of the delta got no op back");
    match &ops(&rrx)[..] {
        [Msg::Op(o)] => {
            let body = match &o.data {
                Data::Full(b) => Some(b.clone()),
                delta => delta.apply_delta(&old),
            };
            assert_eq!(body, Some(new.clone()), "another machine can't rebuild the file from the op of the delta");
        }
        got => panic!("another machine got {got:?} for the delta after GC"),
    }
    assert_eq!(need(core, NOTES, "a/big.bin"), full(core, NOTES, "a/big.bin", &new), "NeedFull after the delta");

    // conflicts: a stale base gets the version that stands, with its content
    let before = core.seq.seq;
    send(core, WRITER, write(NOTES, "a/1", h(b"v1"), b"loser"));
    let reject = |path: &str, rejected: &[u8], current: Hash, data: Data| vec![Msg::Reject(Reject { wt: NOTES.into(), path: path.into(), rejected: h(rejected), current, data, origin: "w".into() })];
    assert_eq!(ops(wrx), reject("a/1", b"loser", h(b"v3"), Data::Full(b"v3".to_vec())), "conflict on a/1 after GC");
    send(core, WRITER, write(NOTES, "a/gone", h(b"soon gone"), b"back from a stale base"));
    assert_eq!(ops(wrx), reject("a/gone", b"back from a stale base", NONE, Data::Absent), "conflict on a deleted key after GC");
    assert_eq!(core.seq.seq, before, "a rejected op was ordered");
    assert!(ops(&rrx).is_empty(), "a conflict reached another machine");

    // content whose blob was collected is stored again when it comes back
    send(core, WRITER, write(NOTES, "a/again", NONE, b"v2"));
    assert_eq!(core.seq.current(NOTES, "a/again"), h(b"v2"));
    assert_eq!(read_blob(&h(b"v2")).unwrap(), Some(b"v2".to_vec()), "a collected blob was not written again");
    ops(wrx);
    assert_eq!(need(core, NOTES, "a/again"), full(core, NOTES, "a/again", b"v2"));
    // and a deleted key is written again on top of its tombstone
    send(core, WRITER, write(NOTES, "a/gone", NONE, b"soon gone"));
    assert_eq!(core.seq.current(NOTES, "a/gone"), h(b"soon gone"), "a deleted key could not be written again after GC");
    ops(wrx);
    core.handle(In::Gone(reader_id));

    // all of it reached the log the anchor writes to now
    assert_log_replays(&core.seq, &core.origins, "after the ops that followed GC");
    assert_eq!(records().len(), sc.content.len() + 4, "the ops after GC are appended to the compacted log, one record each");
}

// ---------- the acceptance scenario ----------

#[test]
fn compact_then_gc_keeps_current_versions_and_serves_the_same() {
    let _home = Home::new();
    let mut core = new_core();
    let sc = scenario(&mut core);
    let (tmps, tmp_bytes) = plant_tmps(&core.seq);
    let before = catch_ups(&mut core, 10);
    assert_catch_up_shape(&core, &sc, &before);
    let (seq0, state0, origins0) = (core.seq.seq, core.seq.state.clone(), core.origins.clone());

    core.log = compact_log(&core.seq, &core.origins).expect("compact_log");
    let freed: io::Result<(usize, u64)> = gc_blobs(&core.seq);
    let freed = freed.expect("gc_blobs");

    // blobs: only the current version of each key is left
    assert_eq!(blob_names(), live_names(&core.seq), "anchor/blobs after GC is not exactly the current versions");
    for hash in &sc.orphans {
        assert!(!blob_path(hash).exists(), "an old version survived");
    }
    for t in &tmps {
        assert!(!blobs_dir().join(t).exists(), "{t} survived");
    }
    assert!(blob_path(&h(b"shared")).exists(), "the blob two keys share is gone");
    assert!(blob_path(&h(b"v1")).exists(), "an old version of a/1 that is another key's current version is gone");
    for (k, body) in &sc.content {
        let hash = core.seq.state[k].1;
        assert_eq!(read_blob(&hash).unwrap(), body.clone(), "content of {k:?} after GC");
    }
    let orphan_bytes = (b"v2".len() + b"soon gone".len()) as u64;
    assert_eq!(freed, (sc.orphans.len() + tmps.len(), orphan_bytes + tmp_bytes), "gc_blobs returns (files deleted, bytes freed)");

    // log: one record per key, and it replays to the same state
    assert_eq!((core.seq.seq, &core.seq.state, &core.origins), (seq0, &state0, &origins0), "GC changed the anchor's state");
    assert_eq!(records(), compacted(&core.seq, &core.origins), "anchor/log2 after compaction");
    assert_eq!(records().len(), sc.content.len());
    assert!(!config::path("anchor/log2.tmp").exists(), "log2.tmp left behind");
    assert_log_replays(&core.seq, &core.origins, "after compaction");

    // machines that connect now get what they would have got before
    let after = catch_ups(&mut core, 20);
    assert_eq!(after, before, "catch-up changed with GC");
    {
        // also from an anchor restarted on the compacted log
        let mut restarted = new_core();
        assert_eq!(catch_ups(&mut restarted, 30), before, "catch-up changed after GC and a restart");
    }

    assert_still_works(&mut core, &sc, 40);
}

// ---------- gc_blobs ----------

#[test]
fn gc_blobs_touches_only_blob_names_and_tmp_files() {
    let _home = Home::new();
    let dir = blobs_dir();
    let live = h(b"live");
    let mut seq = Sequencer { seq: 2, ..Default::default() };
    seq.state.insert(key(NOTES, "keep"), (1, live));
    seq.state.insert(key(NOTES, "dead"), (2, NONE));
    let orphan = hex(&h(b"orphan"));
    let put = |name: &str, body: &[u8]| std::fs::write(dir.join(name), body).unwrap();
    put(&hex(&live), b"live");
    put(&orphan, b"orphan!"); // 7 bytes
    put(&format!("{}.tmp", hex(&live)), b"partial"); // 7 bytes: a .tmp goes even with a live prefix
    put("copia.bak.tmp", b"x"); // 1 byte
    let kept_files = ["README".to_string(), "notas.txt".to_string(), "tmp".to_string(), "notas.tmp.bak".to_string(), orphan[..63].to_string(), format!("{orphan}0"), format!("{}z", &orphan[..63]), format!("{orphan}.bak")];
    for name in &kept_files {
        put(name, b"not a blob");
    }
    let kept_dirs = [hex(&h(b"a directory with a blob's name")), "carpeta.tmp".to_string()];
    for name in &kept_dirs {
        std::fs::create_dir(dir.join(name)).unwrap();
    }

    assert_eq!(gc_blobs(&seq).expect("gc_blobs"), (3, 15), "files deleted and their bytes");

    let mut want: BTreeSet<String> = kept_files.iter().chain(&kept_dirs).cloned().collect();
    want.insert(hex(&live));
    assert_eq!(blob_names(), want, "what is left in anchor/blobs");
    assert_eq!(std::fs::read(blob_path(&live)).unwrap(), b"live");
    for name in &kept_dirs {
        assert!(dir.join(name).is_dir(), "{name} is no longer a directory");
    }
    // nothing left to free
    assert_eq!(gc_blobs(&seq).expect("gc_blobs again"), (0, 0));
    assert_eq!(blob_names(), want);
}

#[test]
fn gc_blobs_errs_when_blobs_dir_cannot_be_listed() {
    let _home = Home::new();
    std::fs::remove_dir_all(blobs_dir()).unwrap();
    assert!(gc_blobs(&Sequencer::default()).is_err());
}

// ---------- compact_log ----------

#[test]
fn compact_log_writes_one_record_per_key_by_seq() {
    let _home = Home::new();
    let log2 = config::path("anchor/log2");
    let tmp = config::path("anchor/log2.tmp");
    // a state made by hand: seqs with gaps, a tombstone, a key with no origin on record
    let mut seq = Sequencer { seq: 40, ..Default::default() };
    seq.state.insert(key("drive:b", "x/late"), (40, h(b"late")));
    seq.state.insert(key(NOTES, "a/first"), (2, h(b"first")));
    seq.state.insert(key(NOTES, "a/dead"), (7, NONE));
    seq.state.insert(key("vault:R", "a/.env\nK"), (19, h(b"sealed")));
    let mut origins: HashMap<Key, String> = HashMap::new();
    origins.insert(key("drive:b", "x/late"), "laptop".into());
    origins.insert(key(NOTES, "a/first"), "desk".into());
    origins.insert(key(NOTES, "a/dead"), "desk".into());
    // what was there before does not matter: an older log, and a longer log2.tmp from a crash
    std::fs::write(&log2, b"an older log that the new one replaces").unwrap();
    std::fs::write(&tmp, vec![0xaa; 4096]).unwrap();
    std::fs::write(blobs_dir().join("untouched"), b"compaction leaves blobs alone").unwrap();

    let f: io::Result<File> = compact_log(&seq, &origins);
    let mut f = f.expect("compact_log");

    let want: Vec<Rec> = vec![
        (2, NOTES.into(), "a/first".into(), h(b"first"), "desk".into()),
        (7, NOTES.into(), "a/dead".into(), NONE, "desk".into()),
        (19, "vault:R".into(), "a/.env\nK".into(), h(b"sealed"), String::new()),
        (40, "drive:b".into(), "x/late".into(), h(b"late"), "laptop".into()),
    ];
    assert_eq!(records(), want);
    // byte for byte the format accept() writes
    let mut bytes = vec![];
    for (s, wt, path, hash, origin) in &want {
        let mut w = W::default();
        w.u64(*s).str(wt).str(path).hash(hash).str(origin);
        bytes.extend_from_slice(&w.0);
    }
    assert_eq!(std::fs::read(&log2).unwrap(), bytes, "anchor/log2 is not the records accept() would have written");
    assert!(!tmp.exists(), "log2.tmp left behind");
    assert_eq!(blob_names(), BTreeSet::from(["untouched".to_string()]), "compact_log touched anchor/blobs");
    let (s, o, _) = open_log().unwrap();
    assert_eq!((s.seq, &s.state), (40, &seq.state), "open_log after compaction");
    assert_eq!(o.get(&key("vault:R", "a/.env\nK")).map(String::as_str), Some(""), "a key with no origin is recorded with an empty one");

    // the handle it returns appends to the new log
    let mut w = W::default();
    w.u64(41).str(NOTES).str("a/next").hash(&h(b"next")).str("desk");
    f.write_all(&w.0).unwrap();
    drop(f);
    let mut all = want.clone();
    all.push((41, NOTES.into(), "a/next".into(), h(b"next"), "desk".into()));
    assert_eq!(records(), all, "a record written through the returned file is not at the end of anchor/log2");
    let (s, _, _) = open_log().unwrap();
    assert_eq!((s.seq, s.current(NOTES, "a/next")), (41, h(b"next")));
}

#[test]
fn compact_log_of_empty_state_gives_empty_log() {
    let _home = Home::new();
    let log2 = config::path("anchor/log2");
    // a new anchor: no log yet
    assert!(!log2.exists());
    let mut f = compact_log(&Sequencer::default(), &HashMap::new()).expect("compact_log with no log2 yet");
    assert_eq!(std::fs::read(&log2).unwrap(), b"", "an empty state compacts to an empty anchor/log2");
    assert!(!config::path("anchor/log2.tmp").exists());
    let mut w = W::default();
    w.u64(1).str(NOTES).str("a/1").hash(&h(b"v1")).str("w");
    f.write_all(&w.0).unwrap();
    drop(f);
    assert_eq!(records(), vec![(1, NOTES.to_string(), "a/1".to_string(), h(b"v1"), "w".to_string())]);
    // and an existing log is emptied too
    drop(compact_log(&Sequencer::default(), &HashMap::new()).expect("compact_log"));
    assert_eq!(std::fs::read(&log2).unwrap(), b"");
}

#[test]
fn compact_log_error_leaves_log_untouched() {
    let _home = Home::new();
    let mut core = new_core();
    scenario(&mut core);
    let log2 = config::path("anchor/log2");
    let before = std::fs::read(&log2).unwrap();
    // log2.tmp can't be created: a directory holds its name
    let tmp = config::path("anchor/log2.tmp");
    std::fs::create_dir(&tmp).unwrap();
    std::fs::write(tmp.join("in the way"), b"x").unwrap();

    assert!(compact_log(&core.seq, &core.origins).is_err(), "compact_log could not write log2.tmp and still returned Ok");
    assert_eq!(std::fs::read(&log2).unwrap(), before, "a failed compaction changed anchor/log2");
    assert_log_replays(&core.seq, &core.origins, "after a failed compaction");
}

// ---------- Core::gc ----------

#[test]
fn core_gc_compacts_collects_and_keeps_serving() {
    let _home = Home::new();
    let mut core = new_core();
    let sc = scenario(&mut core);
    let (tmps, _) = plant_tmps(&core.seq);
    core.handle(In::Msg(WRITER, Msg::Claim { wt: NOTES.into(), path: "a/1".into(), session: "s1".into() }));
    let before = catch_ups(&mut core, 10);
    assert_catch_up_shape(&core, &sc, &before);
    let (seq0, state0, origins0) = (core.seq.seq, core.seq.state.clone(), core.origins.clone());
    let clients0: BTreeSet<u64> = core.clients.keys().copied().collect();

    core.gc();

    assert_eq!((core.seq.seq, &core.seq.state, &core.origins), (seq0, &state0, &origins0), "gc changed the anchor's state");
    assert_eq!(core.clients.keys().copied().collect::<BTreeSet<u64>>(), clients0, "gc changed who is connected");
    assert!(core.leases.holder(NOTES, "a/1").is_some_and(|e| e.session == "s1"), "gc dropped a lease");
    assert!(core.sessions.contains_key("s1"), "gc dropped a lease session");
    assert_eq!(blob_names(), live_names(&core.seq), "anchor/blobs after gc is not exactly the current versions");
    for t in &tmps {
        assert!(!blobs_dir().join(t).exists(), "{t} survived");
    }
    assert_eq!(records(), compacted(&core.seq, &core.origins), "anchor/log2 after gc");
    assert!(!config::path("anchor/log2.tmp").exists(), "log2.tmp left behind");
    assert_log_replays(&core.seq, &core.origins, "after gc");
    assert_eq!(catch_ups(&mut core, 20), before, "catch-up changed with gc");

    // a second run finds nothing to do
    let log = std::fs::read(config::path("anchor/log2")).unwrap();
    core.gc();
    assert_eq!(std::fs::read(config::path("anchor/log2")).unwrap(), log, "a second gc changed the log");
    assert_eq!(blob_names(), live_names(&core.seq), "a second gc changed the blobs");

    // the ops that follow go to the compacted log (assert_still_works replays it)
    assert_still_works(&mut core, &sc, 40);
    core.gc();
    assert_eq!(blob_names(), live_names(&core.seq));
    assert_eq!(records(), compacted(&core.seq, &core.origins));
    assert_log_replays(&core.seq, &core.origins, "after a gc that followed more ops");
}

#[test]
fn core_gc_on_a_new_anchor_does_nothing_and_ops_persist() {
    let _home = Home::new();
    let mut core = new_core();
    core.gc();
    assert_eq!(core.seq.seq, 0);
    assert!(blob_names().is_empty());
    let _wrx = connect(&mut core, WRITER);
    hello(&mut core, WRITER, 0);
    send(&mut core, WRITER, write(NOTES, "a/1", NONE, b"v1"));
    send(&mut core, WRITER, write(NOTES, "a/1", h(b"v1"), b"v2"));
    assert_eq!(core.seq.current(NOTES, "a/1"), h(b"v2"));
    assert_eq!(records().len(), 2, "ops after the first gc of a new anchor are not in anchor/log2");
    assert_log_replays(&core.seq, &core.origins, "after the first gc of a new anchor");
}

#[test]
fn core_gc_keeps_the_old_log_when_compaction_fails() {
    let _home = Home::new();
    let mut core = new_core();
    let sc = scenario(&mut core);
    let (tmps, _) = plant_tmps(&core.seq);
    let log2 = config::path("anchor/log2");
    let before = std::fs::read(&log2).unwrap();
    let tmp = config::path("anchor/log2.tmp");
    std::fs::create_dir(&tmp).unwrap();
    std::fs::write(tmp.join("in the way"), b"x").unwrap();

    core.gc(); // must not panic or exit

    assert_eq!(std::fs::read(&log2).unwrap(), before, "a failed compaction changed anchor/log2");
    // blobs are collected all the same: that only depends on the state
    assert_eq!(blob_names(), live_names(&core.seq), "blobs were not collected after a failed compaction");
    for t in &tmps {
        assert!(!blobs_dir().join(t).exists(), "{t} survived");
    }
    // the anchor goes on with the log it had
    send(&mut core, WRITER, write(NOTES, "a/after", NONE, b"after"));
    assert_eq!(core.seq.current(NOTES, "a/after"), h(b"after"));
    assert_eq!(records().len(), count_records(&before) + 1, "the op after a failed compaction is not appended to the old log");
    assert_log_replays(&core.seq, &core.origins, "after an op that followed a failed compaction");

    // once log2.tmp can be written again, the next gc compacts
    std::fs::remove_dir_all(&tmp).unwrap();
    core.gc();
    assert_eq!(records(), compacted(&core.seq, &core.origins));
    assert_eq!(records().len(), sc.content.len() + 1);
    send(&mut core, WRITER, write(NOTES, "a/later", NONE, b"later"));
    assert_log_replays(&core.seq, &core.origins, "after the gc that recovered");
}

/// Records in these log bytes.
fn count_records(bytes: &[u8]) -> usize {
    let mut r = Rd(bytes);
    let mut n = 0;
    while (|| Some((r.u64()?, r.str()?, r.str()?, r.hash()?, r.str()?)))().is_some() {
        n += 1;
    }
    n
}

#[test]
fn core_gc_survives_a_missing_blobs_dir() {
    let _home = Home::new();
    let mut core = new_core();
    let sc = scenario(&mut core);
    let state0 = core.seq.state.clone();
    std::fs::remove_dir_all(blobs_dir()).unwrap();

    core.gc(); // gc_blobs fails: logged, nothing else

    assert_eq!(core.seq.state, state0);
    assert_eq!(records(), compacted(&core.seq, &core.origins), "the log is compacted even when blobs can't be listed");
    assert_eq!(records().len(), sc.content.len());
    // a delete needs no blob: the anchor still takes it and logs it
    send(&mut core, WRITER, delete(NOTES, "a/1", h(b"v3")));
    assert_eq!(core.seq.current(NOTES, "a/1"), NONE);
    assert_log_replays(&core.seq, &core.origins, "after a gc that could not list the blobs");
}

// ---------- In::Tick ----------

#[test]
fn tick_counts_and_runs_gc_every_gc_ticks() {
    let _home = Home::new();
    let day: u32 = GC_TICKS;
    assert_eq!(day, 2880, "24 h of 30 s ticks");
    let mut core = new_core();
    let sc = scenario(&mut core);
    plant_tmps(&core.seq);
    let log2 = config::path("anchor/log2");
    let (log, blobs) = (std::fs::read(&log2).unwrap(), blob_names());
    assert_eq!(core.ticks, 0);

    core.handle(In::Tick);
    assert_eq!(core.ticks, 1, "a tick adds one");
    assert_eq!(std::fs::read(&log2).unwrap(), log, "the first tick compacted the log");
    assert_eq!(blob_names(), blobs, "the first tick collected blobs");

    core.ticks = GC_TICKS - 1;
    core.handle(In::Tick);
    assert_eq!(core.ticks, 0, "the counter starts over at GC_TICKS");
    assert_eq!(blob_names(), live_names(&core.seq), "tick number GC_TICKS did not collect the blobs");
    assert_eq!(records(), compacted(&core.seq, &core.origins), "tick number GC_TICKS did not compact the log");
    assert_eq!(records().len(), sc.content.len());

    // a whole day from zero: nothing until the last tick
    send(&mut core, WRITER, write(NOTES, "a/1", h(b"v3"), b"v4")); // v3 becomes an orphan
    let orphan = blob_path(&h(b"v3"));
    for _ in 0..GC_TICKS - 1 {
        core.handle(In::Tick);
    }
    assert_eq!(core.ticks, GC_TICKS - 1);
    assert!(orphan.exists(), "gc ran before GC_TICKS ticks");
    assert_eq!(records().len(), sc.content.len() + 1, "the log was compacted before GC_TICKS ticks");
    core.handle(In::Tick);
    assert_eq!(core.ticks, 0);
    assert!(!orphan.exists(), "gc did not run on tick number GC_TICKS");
    assert_eq!(records(), compacted(&core.seq, &core.origins));
    assert_log_replays(&core.seq, &core.origins, "after a day of ticks");
}

// ---------- start ----------

/// The local daemon's own connection (id 0) says Hello: the ops of its catch-up.
fn local_catch_up(tx: &std::sync::mpsc::Sender<In>) -> Vec<Op> {
    let (ctx, crx) = channel();
    assert!(tx.send(In::Connect { id: 0, tx: ctx, key: [0; 32], ip: "-".into(), stream: None }).is_ok(), "the anchor's thread is gone");
    assert!(tx.send(In::Msg(0, Msg::Hello { last_seq: 0, name: "local".into() })).is_ok(), "the anchor's thread is gone");
    let mut ops = vec![];
    loop {
        match crx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(Msg::CaughtUp) => return ops,
            Ok(Msg::Op(o)) => ops.push(o),
            Ok(_) => {}
            Err(e) => panic!("the anchor started by start() did not answer Hello: {e}"),
        }
    }
}

#[test]
fn start_runs_gc_before_the_first_event() {
    let _home = Home::new();
    let (before, live, want) = {
        let mut core = new_core();
        scenario(&mut core);
        plant_tmps(&core.seq);
        (catch_up(&mut core, 10, 0), live_names(&core.seq), compacted(&core.seq, &core.origins))
    };
    assert_ne!(blob_names(), live);

    let tx = start("127.0.0.1:0", [7; 32]).expect("start");
    // by the time the first event is answered, GC is done
    assert_eq!(local_catch_up(&tx), before, "catch-up from the anchor start() runs");
    assert_eq!(blob_names(), live, "start did not collect the blobs before the first event");
    assert_eq!(records(), want, "start did not compact the log before the first event");
    assert!(!config::path("anchor/log2.tmp").exists());
}

#[test]
fn start_is_ok_when_gc_fails() {
    let _home = Home::new();
    let (before, live) = {
        let mut core = new_core();
        scenario(&mut core);
        plant_tmps(&core.seq);
        (catch_up(&mut core, 10, 0), live_names(&core.seq))
    };
    let log2 = config::path("anchor/log2");
    let log = std::fs::read(&log2).unwrap();
    let tmp = config::path("anchor/log2.tmp");
    std::fs::create_dir(&tmp).unwrap();
    std::fs::write(tmp.join("in the way"), b"x").unwrap();

    let tx = start("127.0.0.1:0", [7; 32]).expect("start with a compaction that fails");
    assert_eq!(local_catch_up(&tx), before, "the anchor does not serve after a failed compaction at start");
    assert_eq!(std::fs::read(&log2).unwrap(), log, "a failed compaction at start changed anchor/log2");
    assert_eq!(blob_names(), live, "blobs are collected at start even when compaction fails");
}

// ---------- the comments ----------

#[test]
fn source_comments_describe_gc() {
    let src = include_str!("../anchor.rs");
    assert!(!src.contains("blobs are never collected"), "the old ponytail comment is still in src/anchor.rs");
    assert!(
        src.lines().any(|l| l.contains("//") && l.contains("ponytail:") && l.contains("millions of blobs") && l.contains("incremental GC")),
        "no comment line in src/anchor.rs with `ponytail:`, `millions of blobs` and `incremental GC`"
    );
}
