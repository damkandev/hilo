//! SHA-256, HMAC-SHA256 and HKDF by hand (FIPS 180-4, RFC 2104/4231, RFC 5869).
//! AEAD and X25519 come from audited crates; see noise.rs.

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

// ponytail: ARMv8 SHA2 instructions when present, portable code otherwise; x86 SHA-NI not wired (untestable here).
#[derive(Clone)]
pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
            ],
            buf: [0; 64],
            buf_len: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        if self.buf_len > 0 {
            let n = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + n].copy_from_slice(&data[..n]);
            self.buf_len += n;
            data = &data[n..];
            if self.buf_len < 64 {
                return;
            }
            let b = self.buf;
            self.block(&b);
            self.buf_len = 0;
        }
        while data.len() >= 64 {
            self.block(data[..64].try_into().unwrap());
            data = &data[64..];
        }
        self.buf[..data.len()].copy_from_slice(data);
        self.buf_len = data.len();
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 32];
        for (i, w) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn block(&mut self, b: &[u8; 64]) {
        #[cfg(target_arch = "aarch64")]
        if std::arch::is_aarch64_feature_detected!("sha2") {
            return unsafe { block_arm(&mut self.h, b) };
        }
        block_soft(&mut self.h, b)
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "sha2")]
unsafe fn block_arm(h: &mut [u32; 8], b: &[u8; 64]) {
    use std::arch::aarch64::*;
    let (abcd0, efgh0) = (vld1q_u32(h.as_ptr()), vld1q_u32(h.as_ptr().add(4)));
    let (mut abcd, mut efgh) = (abcd0, efgh0);
    let mut m = [0, 16, 32, 48].map(|o| vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(b.as_ptr().add(o)))));
    for i in 0..16 {
        let wk = vaddq_u32(m[i % 4], vld1q_u32(K.as_ptr().add(i * 4)));
        if i < 12 {
            m[i % 4] = vsha256su1q_u32(vsha256su0q_u32(m[i % 4], m[(i + 1) % 4]), m[(i + 2) % 4], m[(i + 3) % 4]);
        }
        let prev = abcd;
        abcd = vsha256hq_u32(abcd, efgh, wk);
        efgh = vsha256h2q_u32(efgh, prev, wk);
    }
    vst1q_u32(h.as_mut_ptr(), vaddq_u32(abcd, abcd0));
    vst1q_u32(h.as_mut_ptr().add(4), vaddq_u32(efgh, efgh0));
}

fn block_soft(state: &mut [u32; 8], b: &[u8; 64]) {
    {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b_, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b_) ^ (a & c) ^ (b_ & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b_;
            b_ = a;
            a = t1.wrapping_add(t2);
        }
        for (s, v) in state.iter_mut().zip([a, b_, c, d, e, f, g, h]) {
            *s = s.wrapping_add(v);
        }
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut s = Sha256::new();
    s.update(data);
    s.finish()
}

pub fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(&k.map(|b| b ^ 0x36));
    for p in parts {
        inner.update(p);
    }
    let mut outer = Sha256::new();
    outer.update(&k.map(|b| b ^ 0x5c));
    outer.update(&inner.finish());
    outer.finish()
}

/// RFC 5869 HKDF-SHA256.
pub fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let prk = hmac(salt, &[ikm]);
    let mut out = Vec::with_capacity(len);
    let mut t: Vec<u8> = Vec::new();
    let mut i = 1u8;
    while out.len() < len {
        t = hmac(&prk, &[&t, info, &[i]]).to_vec();
        out.extend_from_slice(&t);
        i += 1;
    }
    out.truncate(len);
    out
}

/// Constant-time equality.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn random<const N: usize>() -> [u8; N] {
    use std::io::Read;
    let mut b = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("/dev/urandom");
    b
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_fips_vectors() {
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // one million 'a', fed in odd-sized chunks to exercise buffering
        let mut s = Sha256::new();
        for _ in 0..(1_000_000 / 1000) {
            s.update(&[b'a'; 7][..]);
            s.update(&[b'a'; 993][..]);
        }
        assert_eq!(hex(&s.finish()), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    #[test]
    fn hardware_matches_portable() {
        let mut x = 0x1234_5678u64;
        for len in [0usize, 1, 55, 56, 63, 64, 65, 1000, 4097] {
            let data: Vec<u8> = (0..len).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect();
            let mut soft = Sha256::new();
            soft.update(&data);
            let fast = soft.clone().finish();
            // recompute through the portable path only
            let mut h = Sha256::new().h;
            let mut padded = data.clone();
            padded.push(0x80);
            while padded.len() % 64 != 56 { padded.push(0); }
            padded.extend_from_slice(&((len as u64) * 8).to_be_bytes());
            for c in padded.chunks(64) { block_soft(&mut h, c.try_into().unwrap()); }
            let slow: Vec<u8> = h.iter().flat_map(|w| w.to_be_bytes()).collect();
            assert_eq!(fast.to_vec(), slow, "len {len}");
        }
    }

    #[test]
    fn hmac_rfc4231() {
        assert_eq!(
            hex(&hmac(&[0x0b; 20], &[b"Hi There"])),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac(b"Jefe", &[b"what do ya want ", b"for nothing?"])),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // case 6: key longer than block size
        assert_eq!(
            hex(&hmac(&[0xaa; 131], &[b"Test Using Larger Than Block-Size Key - Hash Key First"])),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn hkdf_rfc5869() {
        let salt: Vec<u8> = (0..=0x0c).collect();
        let info: Vec<u8> = (0xf0..=0xf9).collect();
        assert_eq!(
            hex(&hkdf(&salt, &[0x0b; 22], &info, 42)),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }
}
