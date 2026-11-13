//! Noise_XXpsk3_25519_ChaChaPoly_SHA256, by hand on top of two audited primitives.
//!   -> e
//!   <- e, ee, s, es
//!   -> s, se, psk
#![forbid(unsafe_code)]
use crate::crypto::{hmac, sha256};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::ChaCha20Poly1305;
use x25519_dalek::{PublicKey, StaticSecret};

pub const NAME: &[u8] = b"Noise_XXpsk3_25519_ChaChaPoly_SHA256";

#[derive(Debug)]
pub struct Error;
type R<T> = Result<T, Error>;

#[derive(Clone)]
pub struct Cipher {
    k: Option<[u8; 32]>,
    n: u64,
}

impl Cipher {
    fn new(k: Option<[u8; 32]>) -> Self {
        Cipher { k, n: 0 }
    }
    fn nonce(&self) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&self.n.to_le_bytes());
        n
    }
    /// Fails (instead of ever reusing a nonce) once 2^64-1 messages have gone out.
    pub fn encrypt(&mut self, ad: &[u8], pt: &[u8]) -> R<Vec<u8>> {
        let Some(k) = self.k else { return Ok(pt.to_vec()) };
        if self.n == u64::MAX {
            return Err(Error);
        }
        let ct = ChaCha20Poly1305::new(&k.into()).encrypt(&self.nonce().into(), Payload { msg: pt, aad: ad }).map_err(|_| Error)?;
        self.n += 1;
        Ok(ct)
    }
    pub fn decrypt(&mut self, ad: &[u8], ct: &[u8]) -> R<Vec<u8>> {
        let Some(k) = self.k else { return Ok(ct.to_vec()) };
        if self.n == u64::MAX {
            return Err(Error);
        }
        let pt = ChaCha20Poly1305::new(&k.into())
            .decrypt(&self.nonce().into(), Payload { msg: ct, aad: ad })
            .map_err(|_| Error)?;
        self.n += 1;
        Ok(pt)
    }
}

/// Noise HKDF: up to three 32-byte outputs.
fn kdf(ck: &[u8; 32], ikm: &[u8]) -> [[u8; 32]; 3] {
    let t = hmac(ck, &[ikm]);
    let o1 = hmac(&t, &[&[1]]);
    let o2 = hmac(&t, &[&o1, &[2]]);
    let o3 = hmac(&t, &[&o2, &[3]]);
    [o1, o2, o3]
}

#[derive(Clone)]
struct Symmetric {
    ck: [u8; 32],
    h: [u8; 32],
    c: Cipher,
}

impl Symmetric {
    fn mix_hash(&mut self, d: &[u8]) {
        self.h = sha256(&[&self.h[..], d].concat());
    }
    fn mix_key(&mut self, ikm: &[u8]) {
        let [ck, k, _] = kdf(&self.ck, ikm);
        self.ck = ck;
        self.c = Cipher::new(Some(k));
    }
    fn mix_key_and_hash(&mut self, ikm: &[u8]) {
        let [ck, th, k] = kdf(&self.ck, ikm);
        self.ck = ck;
        self.mix_hash(&th);
        self.c = Cipher::new(Some(k));
    }
    fn encrypt_and_hash(&mut self, pt: &[u8]) -> R<Vec<u8>> {
        let ct = self.c.encrypt(&self.h, pt)?;
        self.mix_hash(&ct);
        Ok(ct)
    }
    fn decrypt_and_hash(&mut self, ct: &[u8]) -> R<Vec<u8>> {
        let pt = self.c.decrypt(&self.h, ct)?;
        self.mix_hash(ct);
        Ok(pt)
    }
}

pub struct Handshake {
    sym: Symmetric,
    s: StaticSecret,
    e: StaticSecret,
    re: Option<PublicKey>,
    pub rs: Option<[u8; 32]>,
}

fn dh(a: &StaticSecret, b: &PublicKey) -> R<[u8; 32]> {
    let s = a.diffie_hellman(b);
    if !s.was_contributory() {
        return Err(Error); // low-order point
    }
    Ok(s.to_bytes())
}

fn take<'a>(m: &mut &'a [u8], n: usize) -> R<&'a [u8]> {
    if m.len() < n {
        return Err(Error);
    }
    let (a, b) = m.split_at(n);
    *m = b;
    Ok(a)
}

fn pubkey(b: &[u8]) -> PublicKey {
    PublicKey::from(<[u8; 32]>::try_from(b).unwrap())
}

impl Handshake {
    /// `e` is only injected by tests; real handshakes draw a fresh ephemeral.
    pub fn new(s: [u8; 32], prologue: &[u8], e: Option<[u8; 32]>) -> Self {
        let h: [u8; 32] = sha256(NAME); // name > 32 bytes
        let mut sym = Symmetric { ck: h, h, c: Cipher::new(None) };
        sym.mix_hash(prologue);
        Handshake {
            sym,
            s: StaticSecret::from(s),
            e: StaticSecret::from(e.unwrap_or_else(crate::crypto::random)),
            re: None,
            rs: None,
        }
    }

    fn e_pub(&self) -> [u8; 32] {
        PublicKey::from(&self.e).to_bytes()
    }

    // ---- initiator ----
    pub fn write1(&mut self, payload: &[u8]) -> R<Vec<u8>> {
        let e = self.e_pub();
        self.sym.mix_hash(&e);
        self.sym.mix_key(&e); // psk modifier
        Ok([&e[..], &self.sym.encrypt_and_hash(payload)?].concat())
    }
    pub fn read2(&mut self, mut m: &[u8]) -> R<Vec<u8>> {
        let re = take(&mut m, 32)?;
        self.sym.mix_hash(re);
        self.sym.mix_key(re);
        let re = pubkey(re);
        self.sym.mix_key(&dh(&self.e, &re)?); // ee
        let rs = self.sym.decrypt_and_hash(take(&mut m, 48)?)?;
        let rs = pubkey(&rs);
        self.sym.mix_key(&dh(&self.e, &rs)?); // es
        self.re = Some(re);
        self.rs = Some(rs.to_bytes());
        self.sym.decrypt_and_hash(m)
    }
    pub fn write3(&mut self, payload: &[u8], psk: &[u8; 32]) -> R<Vec<u8>> {
        let s = PublicKey::from(&self.s).to_bytes();
        let cs = self.sym.encrypt_and_hash(&s)?;
        self.sym.mix_key(&dh(&self.s, self.re.as_ref().ok_or(Error)?)?); // se
        self.sym.mix_key_and_hash(psk);
        Ok([cs, self.sym.encrypt_and_hash(payload)?].concat())
    }

    // ---- responder ----
    pub fn read1(&mut self, mut m: &[u8]) -> R<Vec<u8>> {
        let re = take(&mut m, 32)?;
        self.sym.mix_hash(re);
        self.sym.mix_key(re);
        self.re = Some(pubkey(re));
        self.sym.decrypt_and_hash(m)
    }
    pub fn write2(&mut self, payload: &[u8]) -> R<Vec<u8>> {
        let re = self.re.ok_or(Error)?;
        let e = self.e_pub();
        self.sym.mix_hash(&e);
        self.sym.mix_key(&e);
        self.sym.mix_key(&dh(&self.e, &re)?); // ee
        let s = PublicKey::from(&self.s).to_bytes();
        let cs = self.sym.encrypt_and_hash(&s)?;
        self.sym.mix_key(&dh(&self.s, &re)?); // es
        Ok([&e[..], &cs, &self.sym.encrypt_and_hash(payload)?].concat())
    }
    /// Tries each candidate psk; returns which one matched. Unknown psk => Err.
    pub fn read3(&mut self, mut m: &[u8], psks: &[[u8; 32]]) -> R<(usize, Vec<u8>)> {
        let rs = self.sym.decrypt_and_hash(take(&mut m, 48)?)?;
        let rs = pubkey(&rs);
        self.sym.mix_key(&dh(&self.e, &rs)?); // se
        self.rs = Some(rs.to_bytes());
        for (i, psk) in psks.iter().enumerate() {
            let mut trial = self.sym.clone();
            trial.mix_key_and_hash(psk);
            if let Ok(p) = trial.decrypt_and_hash(m) {
                self.sym = trial;
                return Ok((i, p));
            }
        }
        Err(Error)
    }

    /// (initiator->responder, responder->initiator) ciphers.
    pub fn split(&self) -> (Cipher, Cipher) {
        let [a, b, _] = kdf(&self.sym.ck, &[]);
        (Cipher::new(Some(a)), Cipher::new(Some(b)))
    }
    #[cfg(test)]
    pub fn hash(&self) -> [u8; 32] {
        self.sym.h
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
            Some(h32("893e28b9dc6ca8d611ab664754b8ceb7bac5117349a4439a6b0569da977c464a")),
        );
        let mut r = Handshake::new(
            h32("4a3acbfdb163dec651dfa3194dece676d437029c62a408b4c5ea9114246e4893"),
            &prologue,
            Some(h32("bbdb4cdbd309f1a1f2e1456967fe288cadd6f712d65dc7b7793d5e63da6b375b")),
        );
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

        let (mut i_tx, mut i_rx) = i.split();
        let (mut r_rx, mut r_tx) = r.split();
        let c3 = r_tx.encrypt(&[], &p(3)).unwrap();
        assert_eq!(hex(&c3), msgs[3].1);
        assert_eq!(i_rx.decrypt(&[], &c3).unwrap(), p(3));
        let c4 = i_tx.encrypt(&[], &p(4)).unwrap();
        assert_eq!(hex(&c4), msgs[4].1);
        assert_eq!(r_rx.decrypt(&[], &c4).unwrap(), p(4));
        assert_eq!(hex(&r_tx.encrypt(&[], &p(5)).unwrap()), msgs[5].1);
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
            let mut i = Handshake::new([1; 32], b"p", Some([3; 32]));
            let mut r = Handshake::new([2; 32], b"p", Some([4; 32]));
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
        let mut i = Handshake::new([1; 32], b"", None);
        let mut r = Handshake::new([2; 32], b"", None);
        r.read1(&i.write1(b"").unwrap()).unwrap();
        i.read2(&r.write2(b"").unwrap()).unwrap();
        r.read3(&i.write3(b"", &psk).unwrap(), &[psk]).unwrap();
        (i.split(), r.split())
    }

    #[test]
    fn transport_refuses_replay_reorder_and_tampering() {
        let ((mut tx, _), (mut rx, _)) = pair();
        let a = tx.encrypt(&[], b"first").unwrap();
        let b = tx.encrypt(&[], b"second").unwrap();
        // reordered: the second message can't be read first
        let mut rx2 = rx.clone();
        assert!(rx2.decrypt(&[], &b).is_err());
        assert_eq!(rx.decrypt(&[], &a).unwrap(), b"first");
        assert!(rx.clone().decrypt(&[], &a).is_err(), "replay accepted");
        let mut rng = Rng(42);
        for _ in 0..2000 {
            let bx = mutate(&mut rng, &b);
            if bx != b {
                assert!(rx.clone().decrypt(&[], &bx).is_err(), "tampered frame accepted");
            }
        }
        assert_eq!(rx.decrypt(&[], &b).unwrap(), b"second");
    }

    #[test]
    fn nonce_exhaustion_is_an_error_not_a_panic() {
        let ((mut tx, _), (mut rx, _)) = pair();
        tx.n = u64::MAX - 1;
        rx.n = u64::MAX - 1;
        let last = tx.encrypt(&[], b"last").unwrap();
        assert!(tx.encrypt(&[], b"one more").is_err());
        assert_eq!(rx.decrypt(&[], &last).unwrap(), b"last");
        assert!(rx.decrypt(&[], &last).is_err());
    }

    #[test]
    fn wrong_psk_fails() {
        let mut i = Handshake::new([1; 32], b"", None);
        let mut r = Handshake::new([2; 32], b"", None);
        r.read1(&i.write1(b"").unwrap()).unwrap();
        i.read2(&r.write2(b"").unwrap()).unwrap();
        let m3 = i.write3(b"", &[9; 32]).unwrap();
        assert!(r.read3(&m3, &[[0; 32], [8; 32]]).is_err());
    }
}
