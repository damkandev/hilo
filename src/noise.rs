//! Noise_XXpsk3_25519_ChaChaPoly_SHA256 through `snow`.
//!   -> e
//!   <- e, ee, s, es
//!   -> s, se, psk
#![forbid(unsafe_code)]
use snow::resolvers::{CryptoResolver, DefaultResolver};
use snow::params::{CipherChoice, DHChoice, HashChoice};
use snow::types::{Cipher as SnowCipher, Dh, Hash, Random};
use snow::{HandshakeState, StatelessTransportState};
use std::sync::Arc;
use x25519_dalek::{PublicKey, StaticSecret};

pub const NAME: &str = "Noise_XXpsk3_25519_ChaChaPoly_SHA256";
const MAX: usize = 65535;

#[derive(Debug)]
pub struct Error;
type R<T> = Result<T, Error>;

impl From<snow::Error> for Error {
    fn from(_: snow::Error) -> Self {
        Error
    }
}

/// snow's own 25519 / ChaChaPoly / SHA256. The only randomness snow asks for is the ephemeral key,
/// which hilo draws from the system RNG (`crypto::random`) and hands over here, so a responder can replay its side exactly.
struct Resolver([u8; 32]);
struct Ephemeral([u8; 32]);
impl Random for Ephemeral {
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), snow::Error> {
        dest.copy_from_slice(self.0.get(..dest.len()).ok_or(snow::Error::Rng)?);
        Ok(())
    }
}
impl CryptoResolver for Resolver {
    fn resolve_rng(&self) -> Option<Box<dyn Random>> {
        Some(Box::new(Ephemeral(self.0)))
    }
    fn resolve_dh(&self, c: &DHChoice) -> Option<Box<dyn Dh>> {
        DefaultResolver.resolve_dh(c)
    }
    fn resolve_hash(&self, c: &HashChoice) -> Option<Box<dyn Hash>> {
        DefaultResolver.resolve_hash(c)
    }
    fn resolve_cipher(&self, c: &CipherChoice) -> Option<Box<dyn SnowCipher>> {
        DefaultResolver.resolve_cipher(c)
    }
}

/// One direction of a transport session.
#[derive(Clone)]
pub struct Cipher {
    t: Arc<StatelessTransportState>,
    n: u64,
}

impl Cipher {
    /// snow refuses nonce 2^64-1, so this fails (instead of ever reusing a nonce) once the space runs out.
    pub fn encrypt(&mut self, pt: &[u8]) -> R<Vec<u8>> {
        let mut out = vec![0u8; pt.len() + 16];
        let n = self.t.write_message(self.n, pt, &mut out)?;
        out.truncate(n);
        self.n += 1;
        Ok(out)
    }
    pub fn decrypt(&mut self, ct: &[u8]) -> R<Vec<u8>> {
        let mut out = vec![0u8; ct.len()];
        let n = self.t.read_message(self.n, ct, &mut out)?;
        out.truncate(n);
        self.n += 1;
        Ok(out)
    }
}

pub struct Handshake {
    hs: HandshakeState,
    s: [u8; 32],
    e: [u8; 32],
    prologue: Vec<u8>,
    m1: Vec<u8>, // responder: what it read and wrote, to replay per candidate psk
    p2: Vec<u8>,
}

fn build(s: &[u8; 32], prologue: &[u8], initiator: bool, e: [u8; 32]) -> R<HandshakeState> {
    let b = snow::Builder::with_resolver(NAME.parse()?, Box::new(Resolver(e))).local_private_key(s)?.prologue(prologue)?;
    Ok(if initiator { b.build_initiator()? } else { b.build_responder()? })
}

impl Handshake {
    /// `e` is only injected by tests; real handshakes draw a fresh ephemeral.
    pub fn new(s: [u8; 32], prologue: &[u8], initiator: bool, e: Option<[u8; 32]>) -> R<Self> {
        let e = e.unwrap_or_else(crate::crypto::random);
        let hs = build(&s, prologue, initiator, e)?;
        Ok(Handshake { hs, s, e, prologue: prologue.to_vec(), m1: vec![], p2: vec![] })
    }

    /// Refuses low-order points (snow doesn't): their DH output is known to everyone.
    fn contributory(&self, key: &[u8]) -> R<()> {
        let k = <[u8; 32]>::try_from(key.get(..32).ok_or(Error)?).map_err(|_| Error)?;
        StaticSecret::from(self.s).diffie_hellman(&PublicKey::from(k)).was_contributory().then_some(()).ok_or(Error)
    }

    fn write(&mut self, payload: &[u8]) -> R<Vec<u8>> {
        let mut out = vec![0u8; MAX];
        let n = self.hs.write_message(payload, &mut out)?;
        out.truncate(n);
        Ok(out)
    }

    fn read(&mut self, m: &[u8]) -> R<Vec<u8>> {
        let mut out = vec![0u8; m.len()];
        let n = self.hs.read_message(m, &mut out)?;
        out.truncate(n);
        if let Some(rs) = self.hs.get_remote_static() {
            self.contributory(rs)?;
        }
        Ok(out)
    }

    // ---- initiator ----
    pub fn write1(&mut self, payload: &[u8]) -> R<Vec<u8>> {
        self.write(payload)
    }
    pub fn read2(&mut self, m: &[u8]) -> R<Vec<u8>> {
        self.contributory(m)?; // re
        self.read(m)
    }
    pub fn write3(&mut self, payload: &[u8], psk: &[u8; 32]) -> R<Vec<u8>> {
        self.hs.set_psk(3, psk)?;
        self.write(payload)
    }

    // ---- responder ----
    pub fn read1(&mut self, m: &[u8]) -> R<Vec<u8>> {
        self.contributory(m)?; // re
        self.m1 = m.to_vec();
        self.read(m)
    }
    pub fn write2(&mut self, payload: &[u8]) -> R<Vec<u8>> {
        self.p2 = payload.to_vec();
        self.write(payload)
    }
    /// Tries each candidate psk; returns which one matched. Unknown psk => Err.
    /// A failed read leaves snow's cipher key changed, so every try after the first
    /// rebuilds the responder from the same ephemeral and replays messages 1 and 2.
    pub fn read3(&mut self, m: &[u8], psks: &[[u8; 32]]) -> R<(usize, Vec<u8>)> {
        for (i, psk) in psks.iter().enumerate() {
            if i > 0 {
                self.hs = build(&self.s, &self.prologue, false, self.e)?;
                self.hs.read_message(&self.m1, &mut vec![0u8; self.m1.len()])?;
                self.hs.write_message(&self.p2, &mut vec![0u8; MAX])?;
            }
            self.hs.set_psk(3, psk)?;
            if let Ok(p) = self.read(m) {
                return Ok((i, p));
            }
        }
        Err(Error)
    }

    /// (send, recv, remote static key) once the handshake is done.
    pub fn split(self) -> R<(Cipher, Cipher, [u8; 32])> {
        let rs = self.hs.get_remote_static().ok_or(Error)?.try_into().map_err(|_| Error)?;
        let t = Arc::new(self.hs.into_stateless_transport_mode()?);
        Ok((Cipher { t: t.clone(), n: 0 }, Cipher { t, n: 0 }, rs))
    }
    #[cfg(test)]
    pub fn hash(&self) -> [u8; 32] {
        self.hs.get_handshake_hash().try_into().unwrap()
    }
}

pub fn public_key(secret: &[u8; 32]) -> [u8; 32] {
    PublicKey::from(&StaticSecret::from(*secret)).to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{hex, unhex};

    fn h32(s: &str) -> [u8; 32] {
        unhex(s).unwrap().try_into().unwrap()
    }

    /// Official cacophony vector for this exact protocol name.
    #[test]
    fn cacophony_xxpsk3() {
        let prologue = unhex("4a6f686e2047616c74").unwrap();
        let psk = h32("54686973206973206d7920417573747269616e20706572737065637469766521");
        let mut i = Handshake::new(
            h32("e61ef9919cde45dd5f82166404bd08e38bceb5dfdfded0a34c8df7ed542214d1"),
            &prologue,
            true,
            Some(h32("893e28b9dc6ca8d611ab664754b8ceb7bac5117349a4439a6b0569da977c464a")),
        )
        .unwrap();
        let mut r = Handshake::new(
            h32("4a3acbfdb163dec651dfa3194dece676d437029c62a408b4c5ea9114246e4893"),
            &prologue,
            false,
            Some(h32("bbdb4cdbd309f1a1f2e1456967fe288cadd6f712d65dc7b7793d5e63da6b375b")),
        )
        .unwrap();
        let msgs = [
            ("4c756477696720766f6e204d69736573", "ca35def5ae56cec33dc2036731ab14896bc4c75dbb07a61f879f8e3afa4c7944c9f5ff0e8079630cb7e270c20bbf480821b77a384a645c71a2fd9b3db1c16a5f"),
            ("4d757272617920526f746862617264", "95ebc60d2b1fa672c1f46a8aa265ef51bfe38e7ccb39ec5be34069f144808843b123def17f71e6ae8e57e0e1dec5949c5f7415c6f33517398747d821a06dc23ad430aa1fd7381d46195c378a819fd574425462cbb2d4ca339e738a0b7001dc91423fbf55a99af0c6f1df21012ceb2f"),
            ("462e20412e20486179656b", "52187316111b118d4c060364f7b975dc0809b2590779aff2d63113c564f11744493384db7bf32d5ae6686df6ab06d508d2e07caaf1d6afc010b978735fc78900e71ae1d314130d042e729a"),
            ("4361726c204d656e676572", "eaedc672d4c21e0e2955758756fb98f194c4e90d5deb5b6cf30b27"),
            ("4a65616e2d426170746973746520536179", "522d543c5fe799d09a3d9da7ff54d0dc03c8af1dc7751d2ff708339d2290943e98"),
            ("457567656e2042f6686d20766f6e2042617765726b", "7d4e2c3873eef6a213b04e72f9df60a91666072d3544c5d96c34a09e2329b5030bee796741"),
        ];
        let p = |k: usize| unhex(msgs[k].0).unwrap();
        let m1 = i.write1(&p(0)).unwrap();
        assert_eq!(hex(&m1), msgs[0].1);
        assert_eq!(r.read1(&m1).unwrap(), p(0));
        let m2 = r.write2(&p(1)).unwrap();
        assert_eq!(hex(&m2), msgs[1].1);
        assert_eq!(i.read2(&m2).unwrap(), p(1));
        let m3 = i.write3(&p(2), &psk).unwrap();
        assert_eq!(hex(&m3), msgs[2].1);
        let wrong = [7u8; 32];
        assert_eq!(r.read3(&m3, &[wrong, psk]).unwrap(), (1, p(2)));
        assert_eq!(hex(&i.hash()), "a477edf6a131bbdb54707f6ea30eab6cd935d9b560f0e5fd1f053a95a99669fb");
        assert_eq!(i.hash(), r.hash());

        let (mut i_tx, mut i_rx, _) = i.split().unwrap();
        let (mut r_tx, mut r_rx, _) = r.split().unwrap();
        let c3 = r_tx.encrypt(&p(3)).unwrap();
        assert_eq!(hex(&c3), msgs[3].1);
        assert_eq!(i_rx.decrypt(&c3).unwrap(), p(3));
        let c4 = i_tx.encrypt(&p(4)).unwrap();
        assert_eq!(hex(&c4), msgs[4].1);
        assert_eq!(r_rx.decrypt(&c4).unwrap(), p(4));
        assert_eq!(hex(&r_tx.encrypt(&p(5)).unwrap()), msgs[5].1);
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    /// Flip, cut, extend or swap bytes of a valid message.
    fn mutate(rng: &mut Rng, m: &[u8]) -> Vec<u8> {
        let mut v = m.to_vec();
        match rng.below(5) {
            0 if !v.is_empty() => {
                let i = rng.below(v.len());
                v[i] ^= 1 << rng.below(8);
            }
            1 => v.truncate(rng.below(v.len() + 1)),
            2 => v.extend((0..1 + rng.below(40)).map(|_| rng.next() as u8)),
            3 => v = (0..rng.below(200)).map(|_| rng.next() as u8).collect(),
            _ if v.len() > 1 => {
                let (a, b) = (rng.below(v.len()), rng.below(v.len()));
                v.swap(a, b);
            }
            _ => v.push(0),
        }
        v
    }

    /// Fuzz the handshake: any change to any message must be refused, and nothing may panic.
    #[test]
    fn handshake_refuses_every_tampered_message() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let psk = [5u8; 32];
        for round in 0..3000 {
            let mut i = Handshake::new([1; 32], b"p", true, Some([3; 32])).unwrap();
            let mut r = Handshake::new([2; 32], b"p", false, Some([4; 32])).unwrap();
            let m1 = i.write1(b"").unwrap();
            let which = round % 3;
            let m1x = if which == 0 { mutate(&mut rng, &m1) } else { m1.clone() };
            if r.read1(&m1x).is_err() {
                continue;
            }
            let m2 = match r.write2(b"") {
                Ok(m) => m,
                Err(_) => continue,
            };
            let m2x = if which == 1 { mutate(&mut rng, &m2) } else { m2.clone() };
            let ok2 = i.read2(&m2x).is_ok();
            if which == 1 {
                assert!(!ok2 || m2x == m2, "round {round}: tampered message 2 accepted");
                continue;
            }
            assert!(ok2 || which == 0, "round {round}");
            if which == 0 {
                // a tampered first message must not lead to a completed handshake
                let m3 = i.write3(b"", &psk).unwrap_or_default();
                assert!(m1x == m1 || r.read3(&m3, &[psk]).is_err(), "round {round}: tampered message 1 accepted");
                continue;
            }
            let m3 = i.write3(b"", &psk).unwrap();
            let m3x = mutate(&mut rng, &m3);
            assert!(m3x == m3 || r.read3(&m3x, &[psk]).is_err(), "round {round}: tampered message 3 accepted");
        }
    }

    fn pair() -> ((Cipher, Cipher), (Cipher, Cipher)) {
        let psk = [5u8; 32];
        let mut i = Handshake::new([1; 32], b"", true, None).unwrap();
        let mut r = Handshake::new([2; 32], b"", false, None).unwrap();
        r.read1(&i.write1(b"").unwrap()).unwrap();
        i.read2(&r.write2(b"").unwrap()).unwrap();
        r.read3(&i.write3(b"", &psk).unwrap(), &[psk]).unwrap();
        let ((a, b, _), (c, d, _)) = (i.split().unwrap(), r.split().unwrap());
        ((a, b), (d, c))
    }

    #[test]
    fn transport_refuses_replay_reorder_and_tampering() {
        let ((mut tx, _), (mut rx, _)) = pair();
        let a = tx.encrypt(b"first").unwrap();
        let b = tx.encrypt(b"second").unwrap();
        // reordered: the second message can't be read first
        let mut rx2 = rx.clone();
        assert!(rx2.decrypt(&b).is_err());
        assert_eq!(rx.decrypt(&a).unwrap(), b"first");
        assert!(rx.clone().decrypt(&a).is_err(), "replay accepted");
        let mut rng = Rng(42);
        for _ in 0..2000 {
            let bx = mutate(&mut rng, &b);
            if bx != b {
                assert!(rx.clone().decrypt(&bx).is_err(), "tampered frame accepted");
            }
        }
        assert_eq!(rx.decrypt(&b).unwrap(), b"second");
    }

    #[test]
    fn nonce_exhaustion_is_an_error_not_a_panic() {
        let ((mut tx, _), (mut rx, _)) = pair();
        tx.n = u64::MAX - 1;
        rx.n = u64::MAX - 1;
        let last = tx.encrypt(b"last").unwrap();
        assert!(tx.encrypt(b"one more").is_err());
        assert_eq!(rx.decrypt(&last).unwrap(), b"last");
        assert!(rx.decrypt(&last).is_err());
    }

    /// snow accepts low-order points; hilo must not (their DH output is public).
    #[test]
    fn low_order_points_are_refused() {
        let hs = Handshake::new([2; 32], b"", false, None).unwrap();
        let mut one = [0u8; 32];
        one[0] = 1;
        let order8 = h32("e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800");
        for p in [[0u8; 32], one, order8] {
            assert!(hs.contributory(&p).is_err());
        }
        assert!(hs.contributory(&public_key(&[9; 32])).is_ok());
        assert!(hs.contributory(&[1, 2, 3]).is_err());
    }

    #[test]
    fn wrong_psk_fails() {
        let mut i = Handshake::new([1; 32], b"", true, None).unwrap();
        let mut r = Handshake::new([2; 32], b"", false, None).unwrap();
        r.read1(&i.write1(b"").unwrap()).unwrap();
        i.read2(&r.write2(b"").unwrap()).unwrap();
        let m3 = i.write3(b"", &[9; 32]).unwrap();
        assert!(r.read3(&m3, &[[0; 32], [8; 32]]).is_err());
    }
}
