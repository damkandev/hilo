//! A tiny terminal kit for the setup wizard: prompts with arrow keys, spinners, gradients.
//! No crates: raw mode through `stty` (the console API on Windows), everything else is ANSI escapes.
use std::io::{IsTerminal, Read, Write};
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal() && std::env::var("TERM").map_or(true, |t| t != "dumb")
}

fn color_on() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

// ---------- colors ----------

pub type Rgb = (u8, u8, u8);
pub const BLUE: Rgb = (37, 99, 235);
pub const CYAN: Rgb = (34, 211, 238);
pub const VIOLET: Rgb = (167, 139, 250);
pub const GREEN: Rgb = (74, 222, 128);
pub const RED: Rgb = (248, 113, 113);
pub const YELLOW: Rgb = (250, 204, 21);

pub fn fg(c: Rgb, s: &str) -> String {
    if color_on() { format!("\x1b[38;2;{};{};{}m{s}\x1b[39m", c.0, c.1, c.2) } else { s.to_string() }
}
pub fn bold(s: &str) -> String {
    if color_on() { format!("\x1b[1m{s}\x1b[22m") } else { s.to_string() }
}
pub fn dim(s: &str) -> String {
    if color_on() { format!("\x1b[2m{s}\x1b[22m") } else { s.to_string() }
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (l(a.0, b.0), l(a.1, b.1), l(a.2, b.2))
}

/// Three-stop gradient across the characters of `s`.
pub fn gradient(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len().max(2) - 1;
    chars
        .iter()
        .enumerate()
        .map(|(i, ch)| {
            let t = i as f32 / n as f32;
            let c = if t < 0.5 { mix(BLUE, CYAN, t * 2.0) } else { mix(CYAN, VIOLET, (t - 0.5) * 2.0) };
            fg(c, &ch.to_string())
        })
        .collect()
}

pub fn ok(s: &str) -> String {
    format!("{} {s}", fg(GREEN, "✔"))
}
pub fn fail(s: &str) -> String {
    format!("{} {s}", fg(RED, "✖"))
}

fn out(s: &str) {
    let mut o = std::io::stdout();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

pub fn hide_cursor() {
    if interactive() {
        out("\x1b[?25l");
    }
}
pub fn show_cursor() {
    if interactive() {
        out("\x1b[?25h");
    }
}

const CANCELLED: &str = "\r\x1b[2K\x1b[?25h\x1b[38;2;248;113;113m✖\x1b[39m cancelled\n";

#[cfg(unix)]
extern "C" {
    fn signal(sig: i32, handler: usize) -> usize;
    fn write(fd: i32, buf: *const std::ffi::c_void, n: usize) -> isize;
    fn _exit(code: i32) -> !;
}

#[cfg(unix)]
extern "C" fn on_interrupt(_: i32) {
    unsafe {
        write(1, CANCELLED.as_ptr().cast(), CANCELLED.len());
        _exit(130);
    }
}

/// Ctrl-C while a spinner runs: give the cursor back before leaving.
#[cfg(unix)]
pub fn guard_interrupt() {
    unsafe { signal(2, on_interrupt as *const () as usize) };
}

#[cfg(windows)]
mod con {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetStdHandle(which: u32) -> *mut c_void;
        pub fn GetConsoleMode(h: *mut c_void, mode: *mut u32) -> i32;
        pub fn SetConsoleMode(h: *mut c_void, mode: u32) -> i32;
        pub fn SetConsoleCtrlHandler(f: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }
    pub const STDIN: u32 = -10i32 as u32;
    pub const STDOUT: u32 = -11i32 as u32;
    pub const STDERR: u32 = -12i32 as u32;

    pub fn mode(which: u32) -> Option<u32> {
        let mut m = 0;
        (unsafe { GetConsoleMode(GetStdHandle(which), &mut m) } != 0).then_some(m)
    }
    pub fn set_mode(which: u32, m: u32) {
        unsafe { SetConsoleMode(GetStdHandle(which), m) };
    }
}

/// Lets the console run the ANSI escapes everything here prints (old conhost doesn't by default).
#[cfg(windows)]
pub fn enable_vt() {
    for h in [con::STDOUT, con::STDERR] {
        if let Some(m) = con::mode(h) {
            con::set_mode(h, m | 0x4); // ENABLE_VIRTUAL_TERMINAL_PROCESSING
        }
    }
}

/// Ctrl-C while a spinner runs: give the cursor back before leaving.
#[cfg(windows)]
pub fn guard_interrupt() {
    unsafe extern "system" fn on_ctrl(_: u32) -> i32 {
        let _ = std::io::stdout().write_all(CANCELLED.as_bytes());
        std::process::exit(130)
    }
    unsafe { con::SetConsoleCtrlHandler(Some(on_ctrl), 1) };
}

// ---------- raw keys ----------

#[cfg(unix)]
struct Raw(Option<String>);

#[cfg(unix)]
fn stty(args: &[&str]) -> Option<String> {
    let tty = std::fs::File::open("/dev/tty").ok()?;
    let o = Command::new("stty").args(args).stdin(tty).stderr(Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

#[cfg(unix)]
impl Raw {
    fn on() -> Raw {
        let saved = stty(&["-g"]);
        stty(&["-icanon", "-echo", "-isig", "min", "1"]);
        Raw(saved)
    }
}
#[cfg(unix)]
impl Drop for Raw {
    fn drop(&mut self) {
        if let Some(s) = &self.0 {
            stty(&[s]);
        }
    }
}

#[cfg(unix)]
fn tty() -> std::io::Result<std::fs::File> {
    std::fs::File::open("/dev/tty")
}

/// Windows: no line editing, echo or Ctrl-C processing (Ctrl-C arrives as byte 3, like `-isig`), and
/// arrows as the same ESC [ A..D sequences a Unix terminal sends.
#[cfg(windows)]
struct Raw(Option<u32>);

#[cfg(windows)]
impl Raw {
    fn on() -> Raw {
        let saved = con::mode(con::STDIN);
        if let Some(m) = saved {
            con::set_mode(con::STDIN, (m & !(0x1 | 0x2 | 0x4)) | 0x200); // -processed -line -echo, +VT input
        }
        Raw(saved)
    }
}
#[cfg(windows)]
impl Drop for Raw {
    fn drop(&mut self) {
        if let Some(m) = self.0 {
            con::set_mode(con::STDIN, m);
        }
    }
}

/// std reads a console stdin as UTF-16 and hands back UTF-8, which is what `key` expects.
#[cfg(windows)]
fn tty() -> std::io::Result<std::io::Stdin> {
    Ok(std::io::stdin())
}

#[derive(Debug, PartialEq)]
enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Back,
    Tab,
    Cancel,
    Char(char),
    Other,
}

fn key() -> Key {
    let mut tty = match tty() {
        Ok(t) => t,
        Err(_) => return Key::Cancel,
    };
    let mut b = [0u8; 1];
    if tty.read_exact(&mut b).is_err() {
        return Key::Cancel;
    }
    match b[0] {
        3 | 4 => Key::Cancel,
        b'\r' | b'\n' => Key::Enter,
        127 | 8 => Key::Back,
        b'\t' => Key::Tab,
        0x1b => {
            let mut s = [0u8; 2];
            if tty.read_exact(&mut s).is_err() {
                return Key::Cancel;
            }
            match s {
                [b'[', b'A'] => Key::Up,
                [b'[', b'B'] => Key::Down,
                [b'[', b'C'] => Key::Right,
                [b'[', b'D'] => Key::Left,
                _ => Key::Other,
            }
        }
        c if c < 0x20 => Key::Other,
        c => {
            // UTF-8: read the continuation bytes
            let n = if c >= 0xf0 { 3 } else if c >= 0xe0 { 2 } else if c >= 0xc0 { 1 } else { 0 };
            let mut buf = vec![c];
            let mut more = vec![0u8; n];
            if tty.read_exact(&mut more).is_err() {
                return Key::Cancel;
            }
            buf.extend(more);
            String::from_utf8(buf).ok().and_then(|s| s.chars().next()).map_or(Key::Other, Key::Char)
        }
    }
}

pub struct Cancelled;

/// Redraws a block of lines in place.
struct Block(usize);
impl Block {
    fn draw(&mut self, lines: &[String]) {
        let mut s = String::new();
        if self.0 > 0 {
            s += &format!("\x1b[{}A", self.0);
        }
        for l in lines {
            s += "\r\x1b[2K";
            s += l;
            s += "\n";
        }
        // clear leftovers from a taller previous frame
        for _ in lines.len()..self.0 {
            s += "\r\x1b[2K\n";
        }
        if self.0 > lines.len() {
            s += &format!("\x1b[{}A", self.0 - lines.len());
        }
        self.0 = lines.len();
        out(&s);
    }
}

fn question(q: &str) -> String {
    format!("{} {}", fg(CYAN, "?"), bold(q))
}

fn answered(q: &str, a: &str) -> String {
    format!("{} {} {} {}", fg(GREEN, "✔"), bold(q), dim("…"), fg(CYAN, a))
}

pub struct Choice {
    pub label: String,
    pub hint: String,
}

pub fn choice(label: &str, hint: &str) -> Choice {
    Choice { label: label.into(), hint: hint.into() }
}

/// Arrow-key list. Returns the index.
pub fn select(q: &str, choices: &[Choice], start: usize) -> Result<usize, Cancelled> {
    let _raw = Raw::on();
    hide_cursor();
    let mut i = start.min(choices.len().saturating_sub(1));
    let mut b = Block(0);
    loop {
        let mut lines = vec![format!("{} {}", question(q), dim("› ↑↓ to move, enter to pick"))];
        for (j, c) in choices.iter().enumerate() {
            let hint = if c.hint.is_empty() { String::new() } else { format!("  {}", dim(&c.hint)) };
            if j == i {
                lines.push(format!("{} {}{hint}", fg(CYAN, "❯"), fg(CYAN, &bold(&c.label))));
            } else {
                lines.push(format!("  {}{hint}", c.label));
            }
        }
        b.draw(&lines);
        match key() {
            Key::Up => i = (i + choices.len() - 1) % choices.len(),
            Key::Down | Key::Tab => i = (i + 1) % choices.len(),
            Key::Enter => {
                b.draw(&[answered(q, &choices[i].label)]);
                show_cursor();
                return Ok(i);
            }
            Key::Cancel => {
                b.draw(&[fail(&format!("{} {}", bold(q), dim("cancelled")))]);
                show_cursor();
                return Err(Cancelled);
            }
            _ => {}
        }
    }
}

/// Text input with a default shown as a placeholder.
pub fn input(q: &str, default: &str) -> Result<String, Cancelled> {
    let _raw = Raw::on();
    let mut s = String::new();
    let mut b = Block(0);
    loop {
        let shown = if s.is_empty() { dim(default) } else { s.clone() };
        b.draw(&[format!("{} {} {shown}", question(q), dim("›"))]);
        match key() {
            Key::Enter => {
                let v = if s.is_empty() { default.to_string() } else { s };
                b.draw(&[answered(q, &v)]);
                return Ok(v);
            }
            Key::Back => {
                s.pop();
            }
            Key::Tab if s.is_empty() => s = default.to_string(),
            Key::Char(c) => s.push(c),
            Key::Cancel => {
                b.draw(&[fail(&format!("{} {}", bold(q), dim("cancelled")))]);
                return Err(Cancelled);
            }
            _ => {}
        }
    }
}

/// No / Yes toggle.
pub fn confirm(q: &str, default: bool) -> Result<bool, Cancelled> {
    let _raw = Raw::on();
    hide_cursor();
    let mut v = default;
    let mut b = Block(0);
    loop {
        let (no, yes) = if v { (dim("No"), fg(CYAN, &bold("Yes"))) } else { (fg(CYAN, &bold("No")), dim("Yes")) };
        b.draw(&[format!("{} {} {no} {} {yes}", question(q), dim("›"), dim("/"))]);
        match key() {
            Key::Left | Key::Right | Key::Tab | Key::Up | Key::Down => v = !v,
            Key::Char('y' | 'Y') => v = true,
            Key::Char('n' | 'N') => v = false,
            Key::Enter => {
                b.draw(&[answered(q, if v { "Yes" } else { "No" })]);
                show_cursor();
                return Ok(v);
            }
            Key::Cancel => {
                b.draw(&[fail(&format!("{} {}", bold(q), dim("cancelled")))]);
                show_cursor();
                return Err(Cancelled);
            }
            _ => {}
        }
    }
}

// ---------- spinners ----------

/// What a running step can say about its progress.
#[derive(Clone, Default)]
pub struct Progress(Arc<Mutex<Option<String>>>);
impl Progress {
    pub fn set(&self, s: impl Into<String>) {
        *self.0.lock().unwrap() = Some(s.into());
    }
}

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Runs `f` behind a spinner. `f` returns the line to leave behind on success.
pub fn step<T: Send + 'static>(label: &str, f: impl FnOnce(&Progress) -> Result<(T, String), String> + Send + 'static) -> Result<T, String> {
    let p = Progress::default();
    let start = Instant::now();
    if !interactive() {
        println!("  … {label}");
        let r = f(&p);
        match &r {
            Ok((_, done)) => println!("{}", ok(done)),
            Err(e) => println!("{}", fail(e)),
        }
        return r.map(|x| x.0);
    }
    hide_cursor();
    let p2 = p.clone();
    let h = std::thread::spawn(move || f(&p2));
    let mut i = 0;
    while !h.is_finished() {
        let extra = p.0.lock().unwrap().clone().map(|s| format!(" {}", dim(&s))).unwrap_or_default();
        let t = i as f32 / 12.0;
        let c = mix(BLUE, CYAN, (t.sin() + 1.0) / 2.0);
        out(&format!("\r\x1b[2K{} {label}{extra}", fg(c, FRAMES[i % FRAMES.len()])));
        i += 1;
        std::thread::sleep(Duration::from_millis(70));
    }
    let r = h.join().unwrap_or_else(|_| Err("crashed".into()));
    let secs = start.elapsed().as_secs_f32();
    match &r {
        Ok((_, done)) => out(&format!("\r\x1b[2K{} {}\n", ok(done), dim(&format!("{secs:.1}s")))),
        Err(e) => out(&format!("\r\x1b[2K{}\n", fail(&format!("{label}: {e}")))),
    }
    show_cursor();
    r.map(|x| x.0)
}

// ---------- banner and boxes ----------

/// Two machines and the thread between them; a pulse runs along it, then the name appears.
pub fn banner(tagline: &str) {
    if !interactive() {
        println!("hilo — {tagline}\n");
        return;
    }
    hide_cursor();
    const W: usize = 34;
    let line = |drawn: usize, pulse: Option<usize>| -> String {
        let mut s = String::from("  ");
        s += &fg(BLUE, "●");
        for k in 0..W {
            if k >= drawn {
                s.push(' ');
                continue;
            }
            let t = k as f32 / W as f32;
            let base = if t < 0.5 { mix(BLUE, CYAN, t * 2.0) } else { mix(CYAN, VIOLET, (t - 0.5) * 2.0) };
            let c = match pulse {
                Some(p) if k.abs_diff(p) <= 1 => (255, 255, 255),
                Some(p) if k.abs_diff(p) <= 3 => mix(base, (255, 255, 255), 0.5),
                _ => base,
            };
            s += &fg(c, if pulse.is_some_and(|p| k == p) { "━" } else { "─" });
        }
        if drawn >= W {
            s += &fg(VIOLET, "●");
        }
        s
    };
    out("\n");
    for d in (0..=W).step_by(2) {
        out(&format!("\r\x1b[2K{}", line(d, None)));
        std::thread::sleep(Duration::from_millis(12));
    }
    for p in (0..W).step_by(2) {
        out(&format!("\r\x1b[2K{}", line(W, Some(p))));
        std::thread::sleep(Duration::from_millis(16));
    }
    out(&format!("\r\x1b[2K{}\n\n", line(W, None)));
    let title = format!("  {}  ", bold(&gradient("hilo")));
    out(&title);
    for ch in tagline.chars() {
        out(&dim(&ch.to_string()));
        std::thread::sleep(Duration::from_millis(9));
    }
    out("\n\n");
    show_cursor();
}

/// Rounded box, like a "you're done" card.
pub fn card(lines: &[String]) {
    let strip = |s: &str| {
        let mut n = 0;
        let mut esc = false;
        for c in s.chars() {
            match (esc, c) {
                (false, '\x1b') => esc = true,
                (true, 'm') => esc = false,
                (true, _) => {}
                _ => n += 1,
            }
        }
        n
    };
    let w = lines.iter().map(|l| strip(l)).max().unwrap_or(0) + 2;
    println!();
    println!("  {}", fg(CYAN, &format!("╭{}╮", "─".repeat(w))));
    for l in lines {
        println!("  {} {l}{} {}", fg(CYAN, "│"), " ".repeat(w - 2 - strip(l)), fg(CYAN, "│"));
    }
    println!("  {}", fg(CYAN, &format!("╰{}╯", "─".repeat(w))));
    println!();
}

/// `~/x` for paths under HOME.
pub fn tilde(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().into_owned();
    match crate::config::user_home().and_then(|h| h.to_str().map(str::to_string)) {
        Some(h) if !h.is_empty() && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}
