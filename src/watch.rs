//! File-change notifications. macOS: FSEvents (file-level, latency 0, no defer).
//! Linux: inotify, one watch per non-ignored directory. Either way, events only say *where*
//! to look; the scanner decides what changed. `None` from the channel = "rescan everything".
use std::path::PathBuf;
use std::sync::mpsc::Sender;

pub type Events = Sender<Option<Vec<PathBuf>>>;

#[cfg(target_os = "macos")]
mod imp {
    use super::Events;
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::path::{Path, PathBuf};

    #[repr(C)]
    struct Context {
        version: isize,
        info: *mut c_void,
        retain: *const c_void,
        release: *const c_void,
        copy_description: *const c_void,
    }

    type Callback = extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_void, *const u32, *const u64);

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn FSEventStreamCreate(alloc: *const c_void, cb: Callback, ctx: *const Context, paths: *const c_void, since: u64, latency: f64, flags: u32) -> *mut c_void;
        fn FSEventStreamSetDispatchQueue(s: *mut c_void, q: *mut c_void);
        fn FSEventStreamStart(s: *mut c_void) -> bool;
        fn FSEventStreamStop(s: *mut c_void);
        fn FSEventStreamInvalidate(s: *mut c_void);
        fn FSEventStreamRelease(s: *mut c_void);
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFTypeArrayCallBacks: c_void;
        fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, enc: u32) -> *const c_void;
        fn CFArrayCreate(alloc: *const c_void, vals: *const *const c_void, n: isize, cb: *const c_void) -> *const c_void;
        fn CFRelease(p: *const c_void);
    }
    extern "C" {
        fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
        fn dispatch_release(q: *mut c_void);
    }

    const UTF8: u32 = 0x0800_0100;
    const NO_DEFER: u32 = 0x2;
    const FILE_EVENTS: u32 = 0x10;
    const SINCE_NOW: u64 = u64::MAX;
    const MUST_RESCAN: u32 = 0x1 | 0x2 | 0x4; // MustScanSubDirs, UserDropped, KernelDropped

    extern "C" fn callback(_: *mut c_void, info: *mut c_void, n: usize, paths: *mut c_void, flags: *const u32, _: *const u64) {
        let tx = unsafe { &*(info as *const Events) };
        let paths = unsafe { std::slice::from_raw_parts(paths as *const *const c_char, n) };
        let flags = unsafe { std::slice::from_raw_parts(flags, n) };
        if flags.iter().any(|f| f & MUST_RESCAN != 0) {
            let _ = tx.send(None);
            return;
        }
        let v = paths.iter().map(|p| PathBuf::from(unsafe { CStr::from_ptr(*p) }.to_string_lossy().into_owned())).collect();
        let _ = tx.send(Some(v));
    }

    pub struct Watcher {
        stream: *mut c_void,
        queue: *mut c_void,
        info: *mut Events,
    }
    unsafe impl Send for Watcher {}

    impl Watcher {
        pub fn new(roots: &[PathBuf], tx: Events) -> Option<Watcher> {
            unsafe {
                let cstrs: Vec<CString> = roots.iter().filter_map(|r| CString::new(r.to_string_lossy().as_bytes()).ok()).collect();
                let strs: Vec<*const c_void> = cstrs.iter().map(|c| CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8)).collect();
                let arr = CFArrayCreate(std::ptr::null(), strs.as_ptr(), strs.len() as isize, &kCFTypeArrayCallBacks);
                strs.iter().for_each(|s| CFRelease(*s));
                let info = Box::into_raw(Box::new(tx));
                let ctx = Context { version: 0, info: info.cast(), retain: std::ptr::null(), release: std::ptr::null(), copy_description: std::ptr::null() };
                let stream = FSEventStreamCreate(std::ptr::null(), callback, &ctx, arr, SINCE_NOW, 0.0, NO_DEFER | FILE_EVENTS);
                CFRelease(arr);
                if stream.is_null() {
                    drop(Box::from_raw(info));
                    return None;
                }
                let queue = dispatch_queue_create(c"dev.hilo.fsevents".as_ptr(), std::ptr::null());
                FSEventStreamSetDispatchQueue(stream, queue);
                if !FSEventStreamStart(stream) {
                    FSEventStreamInvalidate(stream);
                    FSEventStreamRelease(stream);
                    dispatch_release(queue);
                    drop(Box::from_raw(info));
                    return None;
                }
                Some(Watcher { stream, queue, info })
            }
        }
        /// FSEvents watches whole trees: nothing to add, nothing to re-check.
        pub fn add_dirs(&mut self, _: &[&Path]) -> Vec<PathBuf> {
            vec![]
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            unsafe {
                FSEventStreamStop(self.stream);
                FSEventStreamInvalidate(self.stream);
                FSEventStreamRelease(self.stream);
                dispatch_release(self.queue);
                drop(Box::from_raw(self.info));
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Events;
    use std::collections::HashMap;
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    extern "C" {
        fn inotify_init1(flags: c_int) -> c_int;
        fn inotify_add_watch(fd: c_int, path: *const c_char, mask: u32) -> c_int;
        fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize;
    }
    const IN_CLOEXEC: c_int = 0x80000;
    const MASK: u32 = 0x2 | 0x4 | 0x8 | 0x40 | 0x80 | 0x100 | 0x200 | 0x400 | 0x800 | 0x0100_0000; // modify..move_self, onlydir
    const IN_Q_OVERFLOW: u32 = 0x4000;
    const IN_IGNORED: u32 = 0x8000;

    pub struct Watcher {
        fd: c_int,
        dirs: Arc<Mutex<HashMap<c_int, PathBuf>>>,
    }

    impl Watcher {
        pub fn new(roots: &[PathBuf], tx: Events) -> Option<Watcher> {
            let fd = unsafe { inotify_init1(IN_CLOEXEC) };
            if fd < 0 {
                return None;
            }
            let w = Watcher { fd, dirs: Arc::new(Mutex::new(HashMap::new())) };
            for r in roots {
                w.add(r);
            }
            let dirs = w.dirs.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = unsafe { read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                    if n <= 0 {
                        return;
                    }
                    let mut out = vec![];
                    let mut i = 0usize;
                    let mut rescan = false;
                    while i + 16 <= n as usize {
                        let wd = c_int::from_ne_bytes(buf[i..i + 4].try_into().unwrap());
                        let mask = u32::from_ne_bytes(buf[i + 4..i + 8].try_into().unwrap());
                        let len = u32::from_ne_bytes(buf[i + 12..i + 16].try_into().unwrap()) as usize;
                        let name = &buf[i + 16..(i + 16 + len).min(n as usize)];
                        let name = String::from_utf8_lossy(name.split(|b| *b == 0).next().unwrap_or(&[])).into_owned();
                        i += 16 + len;
                        if mask & IN_Q_OVERFLOW != 0 {
                            rescan = true;
                            continue;
                        }
                        let mut d = dirs.lock().unwrap();
                        if mask & IN_IGNORED != 0 {
                            d.remove(&wd);
                            continue;
                        }
                        if let Some(dir) = d.get(&wd) {
                            out.push(if name.is_empty() { dir.clone() } else { dir.join(name) });
                        }
                    }
                    if tx.send(if rescan { None } else { Some(out) }).is_err() {
                        return;
                    }
                }
            });
            Some(w)
        }

        /// True if this directory wasn't watched before.
        fn add(&self, dir: &Path) -> bool {
            let Ok(c) = CString::new(dir.to_string_lossy().as_bytes()) else { return false };
            let wd = unsafe { inotify_add_watch(self.fd, c.as_ptr(), MASK) };
            wd >= 0 && self.dirs.lock().unwrap().insert(wd, dir.to_path_buf()).is_none()
        }

        /// Directories the scanner found (and didn't ignore). Returns the newly watched ones:
        /// anything written there before the watch existed was missed, so the caller looks again.
        pub fn add_dirs(&mut self, dirs: &[&Path]) -> Vec<PathBuf> {
            dirs.iter().filter(|d| self.add(d)).map(|d| d.to_path_buf()).collect()
        }
    }
}

pub use imp::Watcher;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sees_a_write() {
        let d = std::env::temp_dir().join(format!("hilo-watch-{}", crate::crypto::hex(&crate::crypto::random::<4>())));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        let d = std::fs::canonicalize(&d).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut w = Watcher::new(&[d.clone()], tx).expect("watcher");
        let _ = w.add_dirs(&[&d.join("sub")]);
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(d.join("sub/f.txt"), "x").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while !found && std::time::Instant::now() < deadline {
            if let Ok(Some(paths)) = rx.recv_timeout(Duration::from_millis(200)) {
                found = paths.iter().any(|p| p.ends_with("sub/f.txt"));
            }
        }
        drop(w);
        std::fs::remove_dir_all(&d).unwrap();
        assert!(found);
    }
}
