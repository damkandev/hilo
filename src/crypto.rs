//! SHA-256 and HKDF from RustCrypto (`sha2`, `hkdf`), plus small helpers.
#![cfg_attr(unix, forbid(unsafe_code))]
#![cfg_attr(windows, deny(unsafe_code))] // one audited call: BCryptGenRandom in `random`

use sha2::{Digest, Sha256};

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// RFC 5869 HKDF-SHA256.
pub fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    hkdf::Hkdf::<Sha256>::new(Some(salt), ikm).expand(info, &mut out).expect("hkdf length");
    out
}

/// Constant-time equality.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(unix)]
pub fn random<const N: usize>() -> [u8; N] {
    use std::io::Read;
    let mut b = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("/dev/urandom");
    b
}

/// Windows has no /dev/urandom: the system RNG through bcrypt (Vista+), the same source std uses.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn random<const N: usize>() -> [u8; N] {
    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(alg: *mut std::ffi::c_void, buf: *mut u8, len: u32, flags: u32) -> i32;
    }
    const USE_SYSTEM_PREFERRED_RNG: u32 = 2;
    let mut b = [0u8; N];
    let st = unsafe { BCryptGenRandom(std::ptr::null_mut(), b.as_mut_ptr(), N as u32, USE_SYSTEM_PREFERRED_RNG) };
    assert!(st == 0, "BCryptGenRandom: {st:#x}");
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
        assert_eq!(hex(&s.finalize()), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
        use hmac::Mac;
        let mut m = hmac::Hmac::<Sha256>::new_from_slice(key).unwrap();
        parts.iter().for_each(|p| m.update(p));
        m.finalize().into_bytes().into()
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
