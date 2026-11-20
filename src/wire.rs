//! Hand-rolled binary codec. Frames are length-prefixed and encrypted (see net.rs).
#![forbid(unsafe_code)]
use crate::sync::{Data, Hash, Op, Reject};
use std::io::{self, Read, Write};

pub const MAX_FRAME: usize = 1 << 20; // before and after auth: messages above 60 KB travel in chunks
pub const MAX_MSG: usize = 64 << 20; // a reassembled message (50 MB file cap + overhead)

#[derive(Clone, Debug, PartialEq)]
pub struct Lease {
    pub wt: String,
    pub path: String,
    pub session: String,
    pub machine: String,
    pub age_ms: u64,
    pub ttl_ms: u64, // 0 = released
}

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Hello { last_seq: u64, name: String },
    Op(Op),
    Reject(Reject),
    CaughtUp,
    Ping,
    Claim { wt: String, path: String, session: String },
    Release { wt: String, path: String, session: String }, // empty wt = every lease of the session
    Lease(Lease),
    NeedFull { wt: String, path: String },
    Resync { wt: String },
    VaultKey { epoch: u32, key: Hash },
    Time { t0: u64, t1: u64 },
    Peers(Vec<String>),
    /// Anchor -> a sandbox with a scoped token, before catch-up: what it may sync. Only scoped
    /// clients get it, and they run a binary that knows it (`hilo up` installs the latest).
    Scope { unit: String, prefix: String, ro: bool },
}

#[derive(Default)]
pub struct W(pub Vec<u8>);
impl W {
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
        self
    }
    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
    pub fn hash(&mut self, v: &Hash) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
    pub fn opt(&mut self, v: Option<&[u8]>) -> &mut Self {
        match v {
            None => self.u8(0),
            Some(d) => self.u8(1).bytes(d),
        }
    }
    pub fn data(&mut self, d: &Data) -> &mut Self {
        match d {
            Data::Absent => self.u8(0),
            Data::Full(b) => self.u8(1).bytes(b),
            Data::Omitted => self.u8(2),
            Data::Delta(p) => self.u8(4).bytes(p),
        }
    }
}

/// Reader over untrusted bytes: every accessor returns None instead of panicking.
pub struct Rd<'a>(pub &'a [u8]);
impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    pub fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
    pub fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    pub fn str(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?.to_vec()).ok()
    }
    pub fn hash(&mut self) -> Option<Hash> {
        self.take(32)?.try_into().ok()
    }
    pub fn opt(&mut self) -> Option<Option<Vec<u8>>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.bytes()?.to_vec())),
            _ => None,
        }
    }
    pub fn data(&mut self) -> Option<Data> {
        Some(match self.u8()? {
            0 => Data::Absent,
            1 => Data::Full(self.bytes()?.to_vec()),
            2 => Data::Omitted,
            // 3 was the old prefix/suffix delta: a bad message now, so mixed hilo versions don't misread it
            4 => Data::Delta(self.bytes()?.to_vec()),
            _ => return None,
        })
    }
}

pub fn encode(m: &Msg) -> Vec<u8> {
    let mut w = W::default();
    match m {
        Msg::Hello { last_seq, name } => w.u8(1).u64(*last_seq).str(name),
        Msg::Op(o) => w
            .u8(2)
            .u64(o.seq)
            .str(&o.wt)
            .str(&o.path)
            .hash(&o.base)
            .hash(&o.new)
            .data(&o.data)
            .str(&o.origin)
            .u64(o.t_event)
            .u64(o.t_seq),
        Msg::Reject(r) => w.u8(3).str(&r.wt).str(&r.path).hash(&r.rejected).hash(&r.current).data(&r.data).str(&r.origin),
        Msg::CaughtUp => w.u8(4),
        Msg::Ping => w.u8(5),
        Msg::Claim { wt, path, session } => w.u8(6).str(wt).str(path).str(session),
        Msg::Release { wt, path, session } => w.u8(7).str(wt).str(path).str(session),
        Msg::Lease(l) => w.u8(8).str(&l.wt).str(&l.path).str(&l.session).str(&l.machine).u64(l.age_ms).u64(l.ttl_ms),
        Msg::NeedFull { wt, path } => w.u8(9).str(wt).str(path),
        Msg::Resync { wt } => w.u8(10).str(wt),
        Msg::VaultKey { epoch, key } => w.u8(11).u32(*epoch).hash(key),
        Msg::Time { t0, t1 } => w.u8(12).u64(*t0).u64(*t1),
        Msg::Peers(p) => {
            w.u8(13).u32(p.len() as u32);
            for n in p {
                w.str(n);
            }
            &mut w
        }
        Msg::Scope { unit, prefix, ro } => w.u8(14).str(unit).str(prefix).u8(*ro as u8),
    };
    w.0
}

pub fn decode(b: &[u8]) -> Option<Msg> {
    let mut r = Rd(b);
    let m = match r.u8()? {
        1 => Msg::Hello { last_seq: r.u64()?, name: r.str()? },
        2 => Msg::Op(Op {
            seq: r.u64()?,
            wt: r.str()?,
            path: r.str()?,
            base: r.hash()?,
            new: r.hash()?,
            data: r.data()?,
            origin: r.str()?,
            t_event: r.u64()?,
            t_seq: r.u64()?,
        }),
        3 => Msg::Reject(Reject { wt: r.str()?, path: r.str()?, rejected: r.hash()?, current: r.hash()?, data: r.data()?, origin: r.str()? }),
        4 => Msg::CaughtUp,
        5 => Msg::Ping,
        6 => Msg::Claim { wt: r.str()?, path: r.str()?, session: r.str()? },
        7 => Msg::Release { wt: r.str()?, path: r.str()?, session: r.str()? },
        8 => Msg::Lease(Lease {
            wt: r.str()?,
            path: r.str()?,
            session: r.str()?,
            machine: r.str()?,
            age_ms: r.u64()?,
            ttl_ms: r.u64()?,
        }),
        9 => Msg::NeedFull { wt: r.str()?, path: r.str()? },
        10 => Msg::Resync { wt: r.str()? },
        11 => Msg::VaultKey { epoch: r.u32()?, key: r.hash()? },
        12 => Msg::Time { t0: r.u64()?, t1: r.u64()? },
        13 => {
            let n = r.u32()? as usize;
            let mut v = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                v.push(r.str()?);
            }
            Msg::Peers(v)
        }
        14 => Msg::Scope {
            unit: r.str()?,
            prefix: r.str()?,
            ro: match r.u8()? {
                0 => false,
                1 => true,
                _ => return None,
            },
        },
        _ => return None,
    };
    r.0.is_empty().then_some(m)
}

pub fn write_frame(w: &mut impl Write, b: &[u8]) -> io::Result<()> {
    let mut f = Vec::with_capacity(4 + b.len());
    f.extend_from_slice(&(b.len() as u32).to_be_bytes());
    f.extend_from_slice(b);
    w.write_all(&f)
}

pub fn read_frame(r: &mut impl Read, max: usize) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_be_bytes(len) as usize;
    if n > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut b = vec![0u8; n]; // capped above before allocating
    r.read_exact(&mut b)?;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_garbage() {
        let op = Op {
            seq: 9,
            wt: "r:main".into(),
            path: "src/a.ts".into(),
            base: [1; 32],
            new: [2; 32],
            data: Data::delta(b"hello world", b"hello brave world"),
            origin: "mac".into(),
            t_event: 5,
            t_seq: 6,
        };
        let lease = Lease { wt: "w".into(), path: "p".into(), session: "s".into(), machine: "m".into(), age_ms: 1, ttl_ms: 2 };
        for m in [
            Msg::Hello { last_seq: 3, name: "mac".into() },
            Msg::Op(op),
            Msg::CaughtUp,
            Msg::Ping,
            Msg::Lease(lease),
            Msg::Peers(vec!["a".into(), "b".into()]),
            Msg::Scope { unit: "drive:notes".into(), prefix: "a b".into(), ro: true },
            Msg::VaultKey { epoch: 2, key: [7; 32] },
        ] {
            let e = encode(&m);
            assert_eq!(decode(&e), Some(m));
            for cut in 0..e.len() {
                let _ = decode(&e[..cut]); // must not panic
            }
        }
        assert_eq!(decode(&[2, 0xff, 0xff]), None);
        assert_eq!(decode(&[13, 0xff, 0xff, 0xff, 0xff]), None);
    }

    /// Whatever arrives after the MAC check goes through `decode`: it must never panic or
    /// allocate wildly, and a decoded message must re-encode to the same bytes.
    #[test]
    fn decoder_fuzz() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let seeds: Vec<Vec<u8>> = [
            Msg::Hello { last_seq: 7, name: "mac".into() },
            Msg::Op(Op { seq: 1, wt: "r:main".into(), path: "a/b.ts".into(), base: [1; 32], new: [2; 32], data: Data::Full(b"x".to_vec()), origin: "m".into(), t_event: 1, t_seq: 2 }),
            Msg::Lease(Lease { wt: "w".into(), path: "p".into(), session: "s".into(), machine: "m".into(), age_ms: 1, ttl_ms: 2 }),
            Msg::Peers(vec!["a".into()]),
            Msg::Scope { unit: "repo:R".into(), prefix: String::new(), ro: false },
            Msg::Time { t0: 1, t1: 2 },
        ]
        .iter()
        .map(encode)
        .collect();
        for i in 0..200_000 {
            let mut b = seeds[i % seeds.len()].clone();
            match rnd() % 4 {
                0 => {
                    let p = (rnd() as usize) % b.len();
                    b[p] = rnd() as u8;
                }
                1 => b.truncate((rnd() as usize) % (b.len() + 1)),
                2 => b = (0..(rnd() % 64)).map(|_| rnd() as u8).collect(),
                _ => {
                    // huge length prefixes must be rejected before allocating
                    if b.len() > 5 {
                        b[1..5].copy_from_slice(&u32::MAX.to_be_bytes());
                    }
                }
            }
            if let Some(m) = decode(&b) {
                assert_eq!(encode(&m), b, "decode/encode mismatch");
            }
        }
    }
}
