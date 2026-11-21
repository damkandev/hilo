//! Pure sync logic: the anchor's sequencer and each machine's replica state.
//! No I/O here, so the deterministic simulation below can hammer it.
//! Keys are (worktree, path) for files, (worktree, "\0ref") for the branch, ("vault:<repo>", "<file>\n<KEY>") for envs.
#![forbid(unsafe_code)]
use crate::crypto::sha256;
use crate::fsops::MAX_FILE;
use crate::wire::{Rd, W};
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};

pub type Hash = [u8; 32];
/// Hash of "file absent".
pub const NONE: Hash = [0; 32];
pub type Key = (String, String);

pub fn hash_of(data: Option<&[u8]>) -> Hash {
    data.map_or(NONE, sha256)
}

/// What travels with an op.
#[derive(Clone, Debug, PartialEq)]
pub enum Data {
    Absent,
    Full(Vec<u8>),
    /// Hash only: the receiver may already have it (catch-up, or the op's own author).
    Omitted,
    /// `new` rebuilt from `base`: a run of `0 off len` (copy base[off..off+len]) and `1 len bytes` (insert).
    Delta(Vec<u8>),
}

impl Data {
    pub fn from_opt(o: Option<Vec<u8>>) -> Data {
        o.map_or(Data::Absent, Data::Full)
    }
    /// The content, if this is resolved (Full or Absent).
    pub fn content(&self) -> Option<Option<&[u8]>> {
        match self {
            Data::Absent => Some(None),
            Data::Full(d) => Some(Some(d)),
            _ => None,
        }
    }
    /// rsync-style delta, but the sender holds both sides, so no checksum round trip and no strong
    /// hash: base blocks are indexed by a rolling checksum, `new` is scanned byte by byte, and each
    /// hit is checked against the real bytes and stretched both ways. Moved, inserted and repeated
    /// chunks become copies, so big binaries with scattered edits travel small too.
    pub fn delta(base: &[u8], new: &[u8]) -> Data {
        debug_assert!(base.len().max(new.len()) <= u32::MAX as usize, "offsets and lengths travel as u32 (callers cap at MAX_FILE)");
        // ~sqrt(len) like rsync: fine blocks for code, few index entries for a 50 MB file
        let b = ((base.len() as f64).sqrt() as usize).clamp(64, 1 << 14);
        let mut w = W::default();
        let mut lit = 0; // start of the bytes not yet covered
        if base.len() >= b && new.len() >= b {
            let mut idx: HashMap<u32, usize, BuildHasherDefault<Mix>> = HashMap::with_capacity_and_hasher(base.len() / b, Default::default());
            for off in (0..=base.len() - b).step_by(b) {
                idx.entry(Roll::new(&base[off..off + b]).sum()).or_insert(off);
            }
            let (mut i, mut r) = (0, Roll::new(&new[..b]));
            // ponytail: one cheap HashMap probe per byte of `new`; a bitset in front if huge binaries get slow
            while i + b <= new.len() {
                if let Some(&off) = idx.get(&r.sum()).filter(|&&o| base[o..o + b] == new[i..i + b]) {
                    let (mut s, mut o) = (i, off);
                    while s > lit && o > 0 && new[s - 1] == base[o - 1] {
                        (s, o) = (s - 1, o - 1);
                    }
                    let mut e = i + b;
                    while e < new.len() && e - s + o < base.len() && new[e] == base[e - s + o] {
                        e += 1;
                    }
                    if s > lit {
                        w.u8(1).bytes(&new[lit..s]);
                    }
                    w.u8(0).u32(o as u32).u32((e - s) as u32);
                    (i, lit) = (e, e);
                    if i + b <= new.len() {
                        r = Roll::new(&new[i..i + b]);
                    }
                    continue;
                }
                if i + b < new.len() {
                    r.roll(new[i], new[i + b]);
                }
                i += 1;
            }
        }
        if lit < new.len() {
            w.u8(1).bytes(&new[lit..]);
        }
        Data::Delta(w.0)
    }
    /// Rebuilds a delta on top of `base`. None if it doesn't fit or would outgrow MAX_FILE
    /// (a few bytes of copies could otherwise ask for gigabytes).
    pub fn apply_delta(&self, base: &[u8]) -> Option<Vec<u8>> {
        let Data::Delta(p) = self else { return None };
        let (mut r, mut out) = (Rd(p), Vec::new());
        while !r.0.is_empty() {
            let piece = match r.u8()? {
                0 => {
                    let (off, n) = (r.u32()? as usize, r.u32()? as usize);
                    base.get(off..off.checked_add(n)?)?
                }
                1 => r.bytes()?,
                _ => return None,
            };
            if (out.len() + piece.len()) as u64 > MAX_FILE {
                return None;
            }
            out.extend_from_slice(piece);
        }
        Some(out)
    }
}

/// Hasher for the rolling sums, which are checksums already: one multiply spreads them over the
/// table (SipHash, once per byte of `new`, was most of a delta's time).
#[derive(Default)]
struct Mix(u64);
impl Hasher for Mix {
    fn write(&mut self, b: &[u8]) {
        b.iter().for_each(|&x| self.write_u32(x as u32));
    }
    fn write_u32(&mut self, n: u32) {
        self.0 = (self.0 ^ n as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 32)
    }
}

/// rsync's weak checksum: two 16-bit sums that slide one byte in O(1).
struct Roll {
    a: u32,
    b: u32,
    n: u32,
}
impl Roll {
    fn new(d: &[u8]) -> Roll {
        let (mut a, mut b) = (0u32, 0u32);
        for &x in d {
            a = a.wrapping_add(x as u32);
            b = b.wrapping_add(a);
        }
        Roll { a, b, n: d.len() as u32 }
    }
    fn roll(&mut self, out: u8, inn: u8) {
        self.a = self.a.wrapping_sub(out as u32).wrapping_add(inn as u32);
        self.b = self.b.wrapping_sub(self.n.wrapping_mul(out as u32)).wrapping_add(self.a);
    }
    fn sum(&self) -> u32 {
        (self.a & 0xffff) | (self.b << 16)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Op {
    pub seq: u64, // 0 until the anchor orders it
    pub wt: String,
    pub path: String,
    pub base: Hash,
    pub new: Hash,
    pub data: Data,
    pub origin: String, // machine name, stamped by the anchor
    pub t_event: u64,   // ms, anchor clock
    pub t_seq: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reject {
    pub wt: String,
    pub path: String,
    pub rejected: Hash,
    pub current: Hash,
    pub data: Data,     // the winning version, always resolved
    pub origin: String, // who wrote the winner
}

/// Branch tips travel as ops on this path: payload = sha, forced flag, optional bundle.
pub const REF: &str = "\0ref";

/// A repo inside a linked folder travels as an op on ("repo:<id>", META), so machines that lack it can
/// clone it into their own copy of that folder. Payload: folder label, dir name, origin, branch, bundle.
pub const META: &str = "\0meta";

pub fn meta_hash(label: &str, name: &str, origin: &str) -> Hash {
    sha256(format!("meta\0{label}\0{name}\0{origin}").as_bytes())
}

pub fn meta_payload(label: &str, name: &str, origin: &str, branch: &str, bundle: Option<&[u8]>) -> Vec<u8> {
    let mut w = crate::wire::W::default();
    w.str(label).str(name).str(origin).str(branch).opt(bundle);
    w.0
}

pub struct Meta {
    pub label: String,
    pub name: String,
    pub origin: String,
    pub branch: String,
    pub bundle: Option<Vec<u8>>,
}

/// A folder or repo name that is safe as one path component.
pub fn safe_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 255 && !n.starts_with('.') && !n.contains('/') && !n.contains('\0') && !crate::fsops::is_dotgit(n)
}

pub fn parse_meta(b: &[u8]) -> Option<Meta> {
    let mut r = crate::wire::Rd(b);
    let m = Meta { label: r.str()?, name: r.str()?, origin: r.str()?, branch: r.str()?, bundle: r.opt()? };
    (r.0.is_empty() && safe_name(&m.label) && safe_name(&m.name) && crate::git::valid_branch(&m.branch)).then_some(m)
}

pub fn ref_hash(sha: &str) -> Hash {
    sha256(format!("ref\0{sha}").as_bytes())
}

pub fn ref_payload(sha: &str, forced: bool, bundle: Option<&[u8]>) -> Vec<u8> {
    let mut w = crate::wire::W::default();
    w.str(sha).u8(forced as u8).opt(bundle);
    w.0
}

pub fn parse_ref(b: &[u8]) -> Option<(String, bool, Option<Vec<u8>>)> {
    let mut r = crate::wire::Rd(b);
    let v = (r.str()?, r.u8()? == 1, r.opt()?);
    (r.0.is_empty() && crate::git::valid_sha(&v.0)).then_some(v)
}

/// What the replica asks its caller to do with (wt, path).
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// Keep this version (conflict branch / vault history) before anything overwrites it.
    Save(Data),
    /// Make this version current.
    Write(Data),
}

/// The anchor's ordering rule: first op on a given base wins, others are rejected.
#[derive(Default)]
pub struct Sequencer {
    pub seq: u64,
    pub state: HashMap<Key, (u64, Hash)>,
}

impl Sequencer {
    pub fn current(&self, wt: &str, path: &str) -> Hash {
        self.state.get(&(wt.to_string(), path.to_string())).map_or(NONE, |s| s.1)
    }

    pub fn submit(&mut self, op: &mut Op) -> Result<u64, Hash> {
        let cur = self.current(&op.wt, &op.path);
        if op.base != cur {
            return Err(cur);
        }
        self.seq += 1;
        op.seq = self.seq;
        self.state.insert((op.wt.clone(), op.path.clone()), (self.seq, op.new));
        Ok(self.seq)
    }

    /// Latest version of every key changed after `last`, in seq order.
    pub fn since(&self, last: u64) -> Vec<(Key, u64, Hash)> {
        let mut v: Vec<_> =
            self.state.iter().filter(|(_, s)| s.0 > last).map(|(k, s)| (k.clone(), s.0, s.1)).collect();
        v.sort_by_key(|x| x.1);
        v
    }
}

#[derive(Default)]
pub struct Replica {
    pub last_seq: u64,
    pub confirmed: HashMap<Key, Hash>,
    pending: HashMap<Key, VecDeque<Op>>,
}

impl Replica {
    pub fn expected(&self, k: &Key) -> Hash {
        match self.pending.get(k).and_then(|q| q.back()) {
            Some(op) => op.new,
            None => self.confirmed.get(k).copied().unwrap_or(NONE),
        }
    }

    /// Will `applied` need the op's content to write it?
    pub fn needs_content(&self, op: &Op, disk: Hash) -> bool {
        let k = (op.wt.clone(), op.path.clone());
        self.pending.get(&k).is_none_or(|q| q.is_empty()) && disk != op.new
    }

    /// A scan saw `data` (hashing to `new`). Returns the op to send if it is new.
    /// The op stays pending until the anchor applies or rejects it.
    pub fn local(&mut self, wt: &str, path: &str, new: Hash, data: Data) -> Option<Op> {
        let k: Key = (wt.into(), path.into());
        let base = self.expected(&k);
        if new == base {
            return None;
        }
        let op = Op { seq: 0, wt: k.0.clone(), path: k.1.clone(), base, new, data, origin: String::new(), t_event: 0, t_seq: 0 };
        self.pending.entry(k).or_default().push_back(op.clone());
        Some(op)
    }

    /// The anchor ordered `op`. `disk` is what is current here right now.
    /// If `needs_content` was true, `op.data` must be resolved.
    pub fn applied(&mut self, op: &Op, disk_hash: Hash, disk: Data) -> Vec<Effect> {
        self.last_seq = self.last_seq.max(op.seq);
        let k: Key = (op.wt.clone(), op.path.clone());
        let old = self.confirmed.insert(k.clone(), op.new).unwrap_or(NONE);
        if let Some(q) = self.pending.get_mut(&k).filter(|q| !q.is_empty()) {
            if q[0].new == op.new {
                q.pop_front(); // ours
            }
            // Otherwise a remote op won: ours will come back rejected, carrying the winner.
            return vec![];
        }
        if disk_hash == op.new {
            return vec![];
        }
        let mut fx = vec![];
        if disk_hash != old {
            fx.push(Effect::Save(disk)); // changed here but not scanned yet
        }
        fx.push(Effect::Write(op.data.clone()));
        fx
    }

    pub fn rejected(&mut self, r: &Reject, disk_hash: Hash, disk: Data) -> Vec<Effect> {
        let k: Key = (r.wt.clone(), r.path.clone());
        self.confirmed.insert(k.clone(), r.current);
        let mut fx = vec![];
        let mut saved = None;
        let q = self.pending.entry(k).or_default();
        if let Some(op) = q.pop_front_if(|op| op.new == r.rejected) {
            if op.new != r.current {
                fx.push(Effect::Save(op.data));
            }
            saved = Some(op.new);
        }
        if q.is_empty() && disk_hash != r.current {
            if Some(disk_hash) != saved {
                fx.push(Effect::Save(disk));
            }
            fx.push(Effect::Write(r.data.clone()));
        }
        fx
    }

    /// The version the rejected write was made on, if it is still the oldest one waiting. A chained
    /// write behind it, or one whose queue a merge already replaced, gets None.
    pub fn base_of(&self, r: &Reject) -> Option<Hash> {
        let q = self.pending.get(&(r.wt.clone(), r.path.clone()))?;
        q.front().filter(|op| op.new == r.rejected).map(|op| op.base)
    }

    /// `op` from another machine lands on a file changed here but not scanned yet: the version that
    /// change was made on. None if a write of this machine is waiting (it comes back rejected).
    pub fn unscanned_base(&self, op: &Op, disk: Hash) -> Option<Hash> {
        let k: Key = (op.wt.clone(), op.path.clone());
        let old = self.confirmed.get(&k).copied().unwrap_or(NONE);
        (self.pending.get(&k).is_none_or(|q| q.is_empty()) && disk != op.new && disk != old).then_some(old)
    }

    /// The disk now holds both edits merged (`new`) on top of the winner (`current`): everything
    /// waiting for this key is replaced by one write on `current`, so the rejections still coming for
    /// it change nothing. Returns that write, or None when the merge is the winner itself.
    pub fn rebased(&mut self, k: &Key, current: Hash, new: Hash, data: Data) -> Option<Op> {
        self.confirmed.insert(k.clone(), current);
        let q = self.pending.entry(k.clone()).or_default();
        q.clear();
        if new == current {
            return None;
        }
        let op = Op { seq: 0, wt: k.0.clone(), path: k.1.clone(), base: current, new, data, origin: String::new(), t_event: 0, t_seq: 0 };
        q.push_back(op.clone());
        Some(op)
    }

    /// Forget what waits to be sent for `k` (the anchor would never take it); the file stays as it is.
    pub fn drop_pending(&mut self, k: &Key) -> usize {
        self.pending.remove(k).map_or(0, |q| q.len())
    }

    /// Keys with something waiting to be sent (no copies of the data).
    pub fn pending_keys(&self) -> impl Iterator<Item = &Key> {
        self.pending.iter().filter(|(_, q)| !q.is_empty()).map(|(k, _)| k)
    }

    /// Catch-up finished: whatever is still pending never reached the anchor. Resend it.
    pub fn caught_up(&self) -> Vec<Op> {
        self.pending.values().flatten().cloned().collect()
    }
}

#[cfg(test)]
mod sim {
    //! Deterministic simulation: 3 machines, random writes, scans, deliveries and drops.
    //! Invariants: no version is ever overwritten before it was captured (sent or saved),
    //! every captured version ends accepted or on a conflict branch, and all machines converge,
    //! also when a rejected machine merges both edits and sends the result instead.
    use super::*;
    use std::collections::HashSet;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    enum Down {
        Applied(Op),
        Rejected(Reject),
        CaughtUp,
    }

    struct M {
        r: Replica,
        disk: HashMap<String, Vec<u8>>,
        captured: HashMap<String, bool>,
        up: VecDeque<Op>,
        down: VecDeque<Down>,
        online: bool,
        caught: bool,
    }

    const PATHS: [&str; 4] = ["a", "b", "c", "d"];

    #[test]
    fn no_lost_writes() {
        for seed in [1u64, 42, 0xdead_beef, 7777] {
            run(seed, 100_000);
        }
    }

    fn run(seed: u64, steps: usize) {
        let mut rng = Rng(seed);
        let mut seq = Sequencer::default();
        let mut blobs: HashMap<Hash, Option<Vec<u8>>> = HashMap::from([(NONE, None)]);
        let mut accepted: HashSet<Hash> = HashSet::from([NONE]); // "absent" is always known
        let mut saved: HashSet<Hash> = HashSet::new();
        let mut produced: HashSet<Hash> = HashSet::new();
        let mut ms: Vec<M> = (0..3)
            .map(|_| M {
                r: Replica::default(),
                disk: HashMap::new(),
                captured: HashMap::new(),
                up: VecDeque::new(),
                down: VecDeque::new(),
                online: false,
                caught: false,
            })
            .collect();

        let connect = |m: &mut M, seq: &Sequencer, blobs: &HashMap<Hash, Option<Vec<u8>>>| {
            m.online = true;
            m.caught = false;
            for ((wt, path), s, h) in seq.since(m.r.last_seq) {
                let op = Op { seq: s, wt, path, base: NONE, new: h, data: Data::from_opt(blobs[&h].clone()), origin: String::new(), t_event: 0, t_seq: 0 };
                m.down.push_back(Down::Applied(op));
            }
            m.down.push_back(Down::CaughtUp);
        };
        for m in ms.iter_mut() {
            connect(m, &seq, &blobs);
        }

        let (merging, merges) = (std::cell::Cell::new(true), std::cell::Cell::new(0));
        let mut step = |rng: &mut Rng, ms: &mut Vec<M>, action: u64, i: usize| {
            match action {
                // an agent writes (or deletes) a file, unseen by hilo yet
                0..=24 => {
                    let p = PATHS[rng.below(4) as usize].to_string();
                    if rng.below(20) == 0 {
                        ms[i].disk.remove(&p);
                    } else {
                        ms[i].disk.insert(p.clone(), format!("v{}", rng.below(40)).into_bytes());
                    }
                    ms[i].captured.insert(p, false);
                }
                // scan
                25..=39 => {
                    let m = &mut ms[i];
                    for p in PATHS {
                        let data = m.disk.get(p).cloned();
                        let h = hash_of(data.as_deref());
                        if let Some(op) = m.r.local("w", p, h, Data::from_opt(data)) {
                            produced.insert(op.new);
                            if m.online && m.caught {
                                m.up.push_back(op);
                            }
                        }
                        m.captured.insert(p.into(), true);
                    }
                }
                // anchor receives one op from machine i
                40..=64 => {
                    if !ms[i].online {
                        return;
                    }
                    let Some(mut op) = ms[i].up.pop_front() else { return };
                    match seq.submit(&mut op) {
                        Ok(_) => {
                            blobs.insert(op.new, op.data.content().unwrap().map(<[u8]>::to_vec));
                            accepted.insert(op.new);
                            for m in ms.iter_mut().filter(|m| m.online) {
                                m.down.push_back(Down::Applied(op.clone()));
                            }
                        }
                        Err(cur) => ms[i].down.push_back(Down::Rejected(Reject {
                            wt: op.wt,
                            path: op.path,
                            rejected: op.new,
                            current: cur,
                            data: Data::from_opt(blobs[&cur].clone()),
                            origin: String::new(),
                        })),
                    }
                }
                // machine i receives one message
                65..=94 => {
                    let m = &mut ms[i];
                    let Some(msg) = m.down.pop_front() else { return };
                    let (path, fx) = match msg {
                        Down::Applied(op) => {
                            let d = m.disk.get(&op.path).cloned();
                            let dh = hash_of(d.as_deref());
                            // a remote write on a file changed here but not scanned yet: the daemon merges too
                            if merging.get() && rng.below(2) == 0 && d.is_some() && op.data.content().is_some_and(|c| c.is_some()) && m.r.unscanned_base(&op, dh).is_some_and(|b| b != NONE) {
                                merges.set(merges.get() + 1);
                                saved.insert(dh); // ours (never scanned) goes to the history before the write
                                m.r.last_seq = m.r.last_seq.max(op.seq);
                                let merged = format!("m{}", rng.below(1000)).into_bytes();
                                m.disk.insert(op.path.clone(), merged.clone());
                                m.captured.insert(op.path.clone(), true);
                                if let Some(o) = m.r.rebased(&(op.wt.clone(), op.path.clone()), op.new, hash_of(Some(&merged)), Data::Full(merged)) {
                                    produced.insert(o.new);
                                    if m.online && m.caught {
                                        m.up.push_back(o);
                                    }
                                }
                                return;
                            }
                            (op.path.clone(), m.r.applied(&op, dh, Data::from_opt(d)))
                        }
                        Down::Rejected(r) => {
                            let d = m.disk.get(&r.path).cloned();
                            let theirs = r.data.content().flatten().map(<[u8]>::to_vec);
                            // the daemon merges when both edits fit (a coin flip here), on a base it knows
                            let can = merging.get() && rng.below(2) == 0 && d.is_some() && m.r.base_of(&r).is_some_and(|b| b != NONE);
                            if let (true, Some(theirs)) = (can, theirs) {
                                merges.set(merges.get() + 1);
                                saved.insert(hash_of(d.as_deref())); // ours goes to the history before the write
                                for op in &m.r.pending[&(r.wt.clone(), r.path.clone())] {
                                    saved.insert(op.new); // in the history since their scan
                                }
                                let merged = if rng.below(4) == 0 { theirs } else { format!("m{}", rng.below(1000)).into_bytes() };
                                m.disk.insert(r.path.clone(), merged.clone());
                                m.captured.insert(r.path.clone(), true);
                                if let Some(op) = m.r.rebased(&(r.wt.clone(), r.path.clone()), r.current, hash_of(Some(&merged)), Data::Full(merged)) {
                                    produced.insert(op.new);
                                    if m.online && m.caught {
                                        m.up.push_back(op);
                                    }
                                }
                                return;
                            }
                            (r.path.clone(), m.r.rejected(&r, hash_of(d.as_deref()), Data::from_opt(d)))
                        }
                        Down::CaughtUp => {
                            m.caught = true;
                            let ops = m.r.caught_up();
                            m.up.extend(ops);
                            return;
                        }
                    };
                    for e in fx {
                        match e {
                            Effect::Save(d) => {
                                saved.insert(hash_of(d.content().unwrap()));
                                m.captured.insert(path.clone(), true);
                            }
                            Effect::Write(d) => {
                                let d = d.content().unwrap().map(<[u8]>::to_vec);
                                let cur = m.disk.get(&path).cloned();
                                let ch = hash_of(cur.as_deref());
                                // identical content to an accepted/saved version is not a loss
                                assert!(
                                    m.captured.get(&path).copied().unwrap_or(true)
                                        || cur == d
                                        || accepted.contains(&ch)
                                        || saved.contains(&ch),
                                    "seed {seed}: clobbered an uncaptured write on {path}"
                                );
                                match d {
                                    Some(d) => m.disk.insert(path.clone(), d),
                                    None => m.disk.remove(&path),
                                };
                                m.captured.insert(path.clone(), true);
                            }
                        }
                    }
                }
                // connection drops: some of what was in flight upward still lands
                95..=96 => {
                    if ms[i].online {
                        let keep = rng.below(ms[i].up.len() as u64 + 1) as usize;
                        ms[i].up.truncate(keep);
                        for _ in 0..keep {
                            // deliver them now, then go offline
                            let mut op = ms[i].up.pop_front().unwrap();
                            if seq.submit(&mut op).is_ok() {
                                blobs.insert(op.new, op.data.content().unwrap().map(<[u8]>::to_vec));
                                accepted.insert(op.new);
                                for (j, m) in ms.iter_mut().enumerate() {
                                    if m.online && j != i {
                                        m.down.push_back(Down::Applied(op.clone()));
                                    }
                                }
                            }
                        }
                        ms[i].online = false;
                        ms[i].up.clear();
                        ms[i].down.clear();
                    }
                }
                _ => {
                    if !ms[i].online {
                        connect(&mut ms[i], &seq, &blobs);
                    }
                }
            }
        };

        for _ in 0..steps {
            let action = rng.below(100);
            let i = rng.below(3) as usize;
            step(&mut rng, &mut ms, action, i);
        }
        // settle: everyone online, then scan + deliver until quiet (merges stop, as the daemon's cap does)
        merging.set(false);
        for i in 0..3 {
            if !ms[i].online {
                step(&mut rng, &mut ms, 99, i);
            }
        }
        for _ in 0..100 {
            for i in 0..3 {
                step(&mut rng, &mut ms, 30, i);
                while !ms[i].up.is_empty() {
                    step(&mut rng, &mut ms, 50, i);
                }
            }
            for i in 0..3 {
                while !ms[i].down.is_empty() {
                    step(&mut rng, &mut ms, 70, i);
                }
            }
        }
        for m in &ms {
            assert!(m.up.is_empty() && m.down.is_empty(), "seed {seed}: did not settle");
            for p in PATHS {
                let want = seq.current("w", p);
                assert_eq!(hash_of(m.disk.get(p).map(|v| &v[..])), want, "seed {seed}: diverged on {p}");
            }
        }
        for h in produced {
            assert!(accepted.contains(&h) || saved.contains(&h), "seed {seed}: lost a version");
        }
        assert!(merges.get() > 100, "seed {seed}: only {} merges", merges.get());
    }
}

#[cfg(test)]
mod delta_tests {
    use super::*;
    #[test]
    fn drop_pending() {
        let mut r = Replica::default();
        let k: Key = ("drive:notes".into(), "fuera.md".into());
        r.confirmed.insert(k.clone(), [1; 32]);
        assert!(r.local(&k.0, &k.1, [2; 32], Data::Omitted).is_some());
        assert!(r.local(&k.0, &k.1, [3; 32], Data::Omitted).is_some());
        assert!(r.local("drive:notes", "in.md", [4; 32], Data::Omitted).is_some());
        assert_eq!(r.drop_pending(&k), 2);
        assert_eq!(r.expected(&k), [1; 32]); // back to what the anchor last confirmed
        assert_eq!(r.caught_up().len(), 1); // the other key still goes out
        assert_eq!(r.drop_pending(&k), 0);
    }


    #[test]
    fn delta_roundtrip() {
        let long = "fn main() {}\n".repeat(400);
        let longer = long.replace("main() {}\nfn main", "main() { x }\nfn main");
        for (a, b) in [("hello world", "hello brave world"), ("aaaa", "aa"), ("", "x"), ("abc", "abc"), ("abcabc", "abc"), ("x", ""), (&long, &longer), (&longer, &long), (&long, "")] {
            let d = Data::delta(a.as_bytes(), b.as_bytes());
            assert_eq!(d.apply_delta(a.as_bytes()).unwrap(), b.as_bytes(), "{a} -> {b}");
        }
        let copy = |off: u32, n: u32| Data::Delta(W(vec![0]).u32(off).u32(n).0.clone());
        assert_eq!(copy(2, 4).apply_delta(b"short"), None, "copy past the end");
        assert_eq!(copy(u32::MAX, 2).apply_delta(b"short"), None, "offset overflow");
        let bomb = Data::Delta((0..20_000).flat_map(|_| W(vec![0]).u32(0).u32(4096).0.clone()).collect());
        assert_eq!(bomb.apply_delta(&[0; 4096]), None, "copies may not outgrow MAX_FILE");
        assert_eq!(Data::Delta(vec![7]).apply_delta(b"x"), None);
    }

    #[test]
    fn delta_small_for_scattered_edits() {
        // pseudo-random "binary": a prefix/suffix delta would resend nearly all of it
        let mut x = 1u64;
        let base: Vec<u8> = (0..1 << 20).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect();
        let mut new = base.clone();
        new[1000] ^= 1;
        new.splice(300_000..300_000, b"inserted".iter().copied()); // shifts everything after
        new.drain(700_000..700_100);
        new[1_000_000] ^= 1;
        new.extend_from_slice(&base[..5000]); // a moved chunk
        let d = Data::delta(&base, &new);
        let Data::Delta(p) = &d else { unreachable!() };
        assert!(p.len() < 8 * 1024, "delta is {} bytes", p.len());
        assert_eq!(d.apply_delta(&base).unwrap(), new);
    }
}
