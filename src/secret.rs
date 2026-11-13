//! 32-byte secrets: the machine key, vault keys, the local-layer key.
//! macOS: login Keychain, readable without a prompt only by the hilo binary that created the item.
//! Elsewhere (or HILO_KEYCHAIN=0, used by tests): ~/.hilo/secrets/<name>, 0600.
// ponytail: Linux keeps 0600 files; the PRD's dedicated `hilo` user needs repos writable by that user, left to deployment.
use crate::crypto::{hex, unhex};
use std::io;

fn use_keychain() -> bool {
    cfg!(target_os = "macos") && std::env::var("HILO_KEYCHAIN").as_deref() != Ok("0")
}

pub fn get(name: &str) -> io::Result<Option<[u8; 32]>> {
    let raw = if use_keychain() {
        kc::get(name)?
    } else {
        match std::fs::read_to_string(crate::config::path(&format!("secrets/{name}"))) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        }
    };
    Ok(raw.and_then(|s| unhex(s.trim())).and_then(|v| v.try_into().ok()))
}

pub fn set(name: &str, v: &[u8; 32]) -> io::Result<()> {
    if use_keychain() {
        kc::set(name, &hex(v))
    } else {
        crate::config::write_private(&format!("secrets/{name}"), hex(v).as_bytes())
    }
}

pub fn delete(name: &str) -> io::Result<()> {
    if use_keychain() {
        kc::delete(name)
    } else {
        match std::fs::remove_file(crate::config::path(&format!("secrets/{name}"))) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Runs `f`; if it takes more than 3 s (macOS is showing a Keychain dialog), says so in the log and
/// with a notification, so a background daemon is never silently stuck.
pub fn waiting_note<T>(f: impl FnOnce() -> T) -> T {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        if !d.load(Ordering::Relaxed) {
            crate::log!("waiting for Keychain permission: approve the macOS dialog (choose \"Always Allow\")");
            crate::notify("waiting for Keychain permission — choose \"Always Allow\"");
        }
    });
    let r = f();
    done.store(true, Ordering::Relaxed);
    r
}

pub fn get_or_create(name: &str) -> io::Result<[u8; 32]> {
    if let Some(k) = get(name)? {
        return Ok(k);
    }
    let k = crate::crypto::random();
    set(name, &k)?;
    Ok(k)
}

#[cfg(target_os = "macos")]
mod kc {
    use std::ffi::{c_char, c_void};
    use std::io;
    const SERVICE: &str = "dev.hilo";
    const NOT_FOUND: i32 = -25300;

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        fn SecKeychainAddGenericPassword(kc: *const c_void, sl: u32, s: *const c_char, al: u32, a: *const c_char, pl: u32, p: *const c_void, item: *mut *const c_void) -> i32;
        fn SecKeychainFindGenericPassword(kc: *const c_void, sl: u32, s: *const c_char, al: u32, a: *const c_char, pl: *mut u32, p: *mut *mut c_void, item: *mut *const c_void) -> i32;
        fn SecKeychainItemFreeContent(attrs: *mut c_void, data: *mut c_void) -> i32;
        fn SecKeychainItemDelete(item: *const c_void) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(p: *const c_void);
    }

    fn err(code: i32) -> io::Error {
        match code {
            -25293 | -25308 => io::Error::other("the Keychain refused (dialog denied or dismissed); restart hilo and choose \"Always Allow\""),
            _ => io::Error::other(format!("keychain error {code}")),
        }
    }

    fn find(name: &str, want_data: bool) -> io::Result<Option<(Option<String>, *const c_void)>> {
        let (mut len, mut data, mut item) = (0u32, std::ptr::null_mut(), std::ptr::null());
        let r = unsafe {
            SecKeychainFindGenericPassword(
                std::ptr::null(),
                SERVICE.len() as u32,
                SERVICE.as_ptr().cast(),
                name.len() as u32,
                name.as_ptr().cast(),
                if want_data { &mut len } else { std::ptr::null_mut() },
                if want_data { &mut data } else { std::ptr::null_mut() },
                &mut item,
            )
        };
        match r {
            0 => {
                let v = want_data.then(|| unsafe {
                    let s = String::from_utf8_lossy(std::slice::from_raw_parts(data as *const u8, len as usize)).into_owned();
                    SecKeychainItemFreeContent(std::ptr::null_mut(), data);
                    s
                });
                Ok(Some((v, item)))
            }
            NOT_FOUND => Ok(None),
            e => Err(err(e)),
        }
    }

    pub fn get(name: &str) -> io::Result<Option<String>> {
        Ok(find(name, true)?.map(|(v, item)| {
            unsafe { CFRelease(item) };
            v.unwrap_or_default()
        }))
    }

    pub fn delete(name: &str) -> io::Result<()> {
        if let Some((_, item)) = find(name, false)? {
            let r = unsafe { SecKeychainItemDelete(item) };
            unsafe { CFRelease(item) };
            if r != 0 {
                return Err(err(r));
            }
        }
        Ok(())
    }

    pub fn set(name: &str, v: &str) -> io::Result<()> {
        delete(name)?;
        let r = unsafe {
            SecKeychainAddGenericPassword(
                std::ptr::null(),
                SERVICE.len() as u32,
                SERVICE.as_ptr().cast(),
                name.len() as u32,
                name.as_ptr().cast(),
                v.len() as u32,
                v.as_ptr().cast(),
                std::ptr::null_mut(),
            )
        };
        if r == 0 { Ok(()) } else { Err(err(r)) }
    }
}

#[cfg(not(target_os = "macos"))]
mod kc {
    use std::io;
    pub fn get(_: &str) -> io::Result<Option<String>> { Ok(None) }
    pub fn set(_: &str, _: &str) -> io::Result<()> { Err(io::Error::other("no keychain")) }
    pub fn delete(_: &str) -> io::Result<()> { Ok(()) }
}

#[cfg(test)]
mod tests {
    /// Touches the real login Keychain: `cargo test -- --ignored keychain`.
    #[test]
    #[ignore]
    fn keychain_roundtrip() {
        let name = format!("test-{}", crate::crypto::hex(&crate::crypto::random::<4>()));
        assert_eq!(super::kc::get(&name).unwrap(), None);
        super::kc::set(&name, "abc").unwrap();
        super::kc::set(&name, "def").unwrap();
        assert_eq!(super::kc::get(&name).unwrap().as_deref(), Some("def"));
        super::kc::delete(&name).unwrap();
        assert_eq!(super::kc::get(&name).unwrap(), None);
    }
}
