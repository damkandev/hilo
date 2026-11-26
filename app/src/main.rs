#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! hilo's desktop app. It runs the `hilo` CLI for everything (`status --json`, `setup`, `add`,
//! `log`, `restore`…), so the CLI stays the one place that knows how hilo works.
use gpui::{
    actions, div, linear_color_stop, linear_gradient, prelude::*, px, relative, rgb, rgba, size, Animation, AnimationExt, App, Application, Bounds,
    ClipboardItem, Context, FontWeight, Pixels, ScrollHandle, Menu, MenuItem, PathPromptOptions, SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use serde_json::Value;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

actions!(hilo, [Quit]);

#[cfg(target_os = "macos")]
extern "C" {
    fn getuid() -> u32;
}

/// The platforms the app runs on. What differs between them lives in the functions that take one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Os {
    Mac,
    Windows,
}
#[cfg(windows)]
const OS: Os = Os::Windows;
#[cfg(not(windows))]
const OS: Os = Os::Mac;

/// The shared folder the app sets up: ~/Desktop/hilo, a drive labelled "hilo" on every machine.
const LABEL: &str = "hilo";

/// The user's home, as the CLI finds it (src/config.rs): HOME, or USERPROFILE on Windows (where HOME is usually unset).
fn home_in(os: Os, env: impl Fn(&str) -> Option<String>) -> PathBuf {
    env("HOME").or_else(|| if os == Os::Windows { env("USERPROFILE") } else { None }).map(PathBuf::from).unwrap_or_default()
}

fn home() -> PathBuf {
    home_in(OS, |k| std::env::var(k).ok())
}

fn bin_name(os: Os) -> &'static str {
    match os {
        Os::Mac => "hilo",
        Os::Windows => "hilo.exe",
    }
}

fn reveal_label(os: Os) -> &'static str {
    match os {
        Os::Mac => "Show in Finder",
        Os::Windows => "Show in Explorer",
    }
}

/// The Mac window has a transparent title bar, with the traffic lights over the metal. On Windows
/// the system draws the bar, with its own title and buttons.
fn transparent_titlebar(os: Os) -> bool {
    os == Os::Mac
}

fn tilde(p: &str) -> String {
    let h = home().to_string_lossy().into_owned();
    match p.strip_prefix(&h) {
        Some(rest) if !h.is_empty() => format!("~{rest}"),
        _ => p.to_string(),
    }
}

/// The CLI shipped inside the bundle, else the one install.sh puts in ~/.local/bin, else PATH.
fn find_hilo_in(os: Os, exe_dir: Option<&Path>, home: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let name = bin_name(os);
    [exe_dir.map(|d| d.join(name)), Some(home.join(".local").join("bin").join(name))].into_iter().flatten().find(|p| exists(p)).unwrap_or_else(|| name.into())
}

fn find_hilo() -> PathBuf {
    let exe = std::env::current_exe().ok();
    find_hilo_in(OS, exe.as_deref().and_then(Path::parent), &home(), |p| p.is_file())
}

/// Runs the CLI and returns stdout, or its error line.
fn run(bin: &Path, args: &[&str]) -> Result<String, String> {
    let mut c = Command::new(bin);
    c.args(args).env("NO_COLOR", "1").stdin(Stdio::null());
    // Windows: no console window flashing up for each call (CREATE_NO_WINDOW)
    #[cfg(windows)]
    c.creation_flags(0x08000000);
    let o = c.output().map_err(|e| format!("{}: {e}", bin.display()))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    } else {
        let e = String::from_utf8_lossy(&o.stderr);
        Err(e.trim().trim_start_matches("hilo: ").to_string())
    }
}

fn ago(t: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    match (now - t).max(0) {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86400 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86400),
    }
}

fn bytes(n: i64) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1 << 20 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{:.1} MB", n as f64 / 1048576.0),
    }
}

/// A CLI command running in the background, its output shown as it arrives.
#[derive(Default)]
struct Job {
    title: String,
    lines: Vec<String>,
    running: bool,
    ok: Option<bool>,
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Status,
    Folders,
    Versions,
    Trash,
}

/// The setup assistant's steps, in order (they're listed down its left side).
#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum Step {
    Welcome,
    Choose,
    Connect,
    Working,
    Done,
}
const STEPS: [&str; 5] = ["Introduction", "Other Computer", "Connect", "Set Up", "Done"];

/// How this Mac meets the other machine.
#[derive(Clone, Copy, PartialEq)]
enum Way {
    /// hilo installs itself on an SSH host and pairs both
    Ssh,
    /// a join code from a machine that already runs hilo
    Code,
    /// this Mac is the anchor; the others join it
    Server,
    /// already the anchor: a new code for one more machine
    Invite,
}

struct Hilo {
    bin: PathBuf,
    folder: PathBuf,
    page: Page,
    status: Option<Value>,
    hosts: Vec<(String, String)>,
    job: Arc<Mutex<Job>>,
    notice: Option<String>,
    versions: Option<(String, Vec<Value>)>,
    trash: Vec<Value>,
    /// the selected row of the table on screen
    sel: Option<usize>,
    metal: Arc<gpui::Image>,
    /// each table's scroll position, kept across renders
    scrolls: std::cell::RefCell<std::collections::HashMap<String, ScrollHandle>>,
    /// the setup assistant, while it's open
    tour: Option<Step>,
    way: Way,
    /// the join code pasted in the assistant
    code: Option<String>,
    /// the SSH host being set up
    target: String,
    /// the assistant was opened to add one more machine (not the first setup)
    adding: bool,
}

fn load_hosts(bin: &Path) -> Vec<(String, String)> {
    run(bin, &["setup", "--hosts"]).unwrap_or_default().lines().filter_map(|l| l.split_once('\t')).map(|(h, n)| (h.to_string(), n.to_string())).collect()
}

/// A join code on the clipboard: the code alone, or a whole `hilo join CODE …` command.
fn find_code(s: &str) -> Option<String> {
    s.split_whitespace().find(|w| w.starts_with("HILO-") && w.contains('@')).map(String::from)
}

// Mac OS X 10.3/10.4, the brushed-metal apps (iTunes, Safari, the Finder): metal with its grain, an
// LCD in the toolbar, a blue-grey source list, striped tables, Aqua gel buttons, a barber-pole
// progress bar, sheets. Lucida Grande and Monaco, as then. Always light: Aqua had no dark mode.
const UI: &str = "Lucida Grande";
const MONO: &str = "Monaco";
const STRIPE: u32 = 0xedf3fe; // Apple's alternate-row blue (MP3 Player sample code)
const SELECT: u32 = 0x3875d7;
const SOURCE: u32 = 0xe7edf6;

/// The traffic-light colours, fill and rim, used as status lamps.
const GREEN: (u32, u32) = (0x28c840, 0x1ea333);
const YELLOW: (u32, u32) = (0xfebc2e, 0xd49b1e);
const RED: (u32, u32) = (0xff5f57, 0xd9453d);
const OFF: (u32, u32) = (0xc8c8c8, 0x9a9a9a);
/// the Installer's bullet for the step you're on
const BLUE: (u32, u32) = (0x5b9bf0, 0x2c62bf);
impl Hilo {
    fn new(cx: &mut Context<Self>) -> Self {
        let bin = find_hilo();
        let hosts = load_hosts(&bin);
        // poll the daemon; a running job's output shows up on the same tick
        cx.spawn(async move |this, cx| loop {
            let Ok(bin) = this.read_with(cx, |h, _| h.bin.clone()) else { return };
            let st = cx.background_executor().spawn(async move { run(&bin, &["status", "--json"]) }).await;
            let alive = this.update(cx, |h, cx| {
                let first = h.status.is_none();
                h.status = st.ok().and_then(|s| serde_json::from_str(&s).ok());
                // not set up yet: the assistant opens on its own
                if first && h.status.is_some() && h.str("role").is_empty() && h.tour.is_none() {
                    h.tour = Some(Step::Welcome);
                }
                cx.notify();
            });
            if alive.is_err() {
                return;
            }
            cx.background_executor().timer(Duration::from_millis(1000)).await;
        })
        .detach();
        // ponytail: on Windows the Desktop can be redirected to OneDrive and this doesn't resolve it; the fix is the Desktop Known Folder (FOLDERID_Desktop)
        Hilo { bin, folder: home().join("Desktop").join(LABEL), page: Page::Status, status: None, hosts, job: Default::default(), notice: None, versions: None, trash: vec![], sel: None, metal: Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, include_bytes!("../assets/metal.png").to_vec())), scrolls: Default::default(), tour: None, way: Way::Ssh, code: None, target: String::new(), adding: false }
    }

    fn str(&self, k: &str) -> String {
        self.status.as_ref().and_then(|s| s[k].as_str()).unwrap_or_default().to_string()
    }

    fn list(&self, k: &str) -> Vec<Value> {
        self.status.as_ref().and_then(|s| s[k].as_array().cloned()).unwrap_or_default()
    }

    /// Runs `hilo ARGS` in the background; its output streams into the job panel. `then` runs on success.
    fn start(&mut self, title: &str, args: Vec<String>, then: Option<fn(&mut Hilo, &mut Context<Hilo>)>, cx: &mut Context<Self>) {
        if self.job.lock().unwrap().running {
            return;
        }
        *self.job.lock().unwrap() = Job { title: title.into(), running: true, ..Default::default() };
        let (bin, job) = (self.bin.clone(), self.job.clone());
        let task = cx.background_executor().spawn(async move {
            use std::io::BufRead;
            let mut c = Command::new(&bin);
            c.args(&args).env("NO_COLOR", "1").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
            #[cfg(windows)]
            c.creation_flags(0x08000000);
            let ok = match c.spawn() {
                Ok(mut c) => {
                    for l in std::io::BufReader::new(c.stdout.take().unwrap()).lines().map_while(Result::ok) {
                        job.lock().unwrap().lines.push(l.trim().to_string());
                    }
                    let mut err = String::new();
                    let _ = std::io::Read::read_to_string(&mut c.stderr.take().unwrap(), &mut err);
                    job.lock().unwrap().lines.extend(err.lines().map(|l| l.trim_start_matches("hilo: ").to_string()));
                    c.wait().is_ok_and(|s| s.success())
                }
                Err(e) => {
                    job.lock().unwrap().lines.push(format!("{}: {e}", bin.display()));
                    false
                }
            };
            let mut j = job.lock().unwrap();
            (j.running, j.ok) = (false, Some(ok));
            ok
        });
        cx.spawn(async move |this, cx| {
            if task.await {
                if let Some(f) = then {
                    let _ = this.update(cx, f);
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn ensure_folder(&mut self) -> Option<String> {
        match std::fs::create_dir_all(&self.folder) {
            Ok(()) => Some(self.folder.to_string_lossy().into_owned()),
            Err(e) => {
                self.notice = Some(format!("Could not create {}: {e}", tilde(&self.folder.to_string_lossy())));
                None
            }
        }
    }

    /// The assistant's last step runs the command for the chosen way; on success it moves to Done.
    fn set_up(&mut self, cx: &mut Context<Self>) {
        let done = Some(Hilo::finish as fn(&mut Hilo, &mut Context<Hilo>));
        if self.way == Way::Invite {
            self.start("Making a join code", vec!["invite".into()], done, cx);
        } else if let Some(f) = self.ensure_folder() {
            let (title, args): (String, Vec<String>) = match self.way {
                Way::Ssh => {
                    // Try Again has no selection: the host from the last try
                    let Some(h) = self.sel.and_then(|i| self.hosts.get(i)).map(|h| h.0.clone()).or(Some(self.target.clone()).filter(|t| !t.is_empty())) else { return };
                    self.target = h.clone();
                    (format!("Setting up {h}"), ["setup", &h, "--folder", &f, "--as", LABEL].map(String::from).to_vec())
                }
                Way::Code => {
                    let Some(c) = self.code.clone() else { return };
                    ("Joining the other machine".into(), ["join", &c, "--folder", &f, "--as", LABEL].map(String::from).to_vec())
                }
                _ => ("Starting hilo on this Mac".into(), ["init", "--folder", &f, "--as", LABEL].map(String::from).to_vec()),
            };
            self.start(&title, args, done, cx);
        }
        if self.job.lock().unwrap().running {
            self.step(Step::Working, cx);
        }
        cx.notify();
    }

    fn finish(&mut self, cx: &mut Context<Self>) {
        if self.tour.is_some() {
            self.tour = Some(Step::Done);
            cx.notify();
        }
    }

    fn open_tour(&mut self, adding: bool, cx: &mut Context<Self>) {
        (self.adding, self.way, self.code, self.target) = (adding, Way::Ssh, None, String::new());
        self.step(if adding { Step::Choose } else { Step::Welcome }, cx);
    }

    fn step(&mut self, s: Step, cx: &mut Context<Self>) {
        if s < Step::Working {
            *self.job.lock().unwrap() = Job::default();
        }
        if s == Step::Connect && self.way == Way::Code && self.code.is_none() {
            self.code = cx.read_from_clipboard().and_then(|c| c.text()).and_then(|t| find_code(&t));
        }
        (self.tour, self.sel, self.notice) = (Some(s), None, None);
        cx.notify();
    }

    fn close_tour(&mut self, cx: &mut Context<Self>) {
        self.tour = None;
        *self.job.lock().unwrap() = Job::default();
        self.go(Page::Status, cx);
    }

    fn paste_code(&mut self, cx: &mut Context<Self>) {
        self.code = cx.read_from_clipboard().and_then(|c| c.text()).and_then(|t| find_code(&t));
        if self.code.is_none() {
            self.notice = Some("There's no join code on the clipboard. Copy it on the other machine first: it starts with HILO-.".into());
        }
        cx.notify();
    }

    fn job_code(&self) -> Option<String> {
        self.job.lock().unwrap().lines.iter().find_map(|l| find_code(l))
    }

    fn add_folder(&mut self, drive: bool, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Sync this folder".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(p) = paths.first() else { return };
            let p = p.to_string_lossy().into_owned();
            let args = if drive { vec!["add".into(), "--drive".into(), p.clone()] } else { vec!["add".into(), p.clone()] };
            let _ = this.update(cx, |h, cx| h.start(&format!("Linking {}", tilde(&p)), args, None, cx));
        })
        .detach();
    }

    fn pick_file(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Show versions".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(p) = paths.first() else { return };
            let p = p.to_string_lossy().into_owned();
            let _ = this.update(cx, |h, cx| h.load_versions(p, cx));
        })
        .detach();
    }

    fn load_versions(&mut self, path: String, cx: &mut Context<Self>) {
        match run(&self.bin, &["log", &path, "--json"]) {
            Ok(s) => {
                self.sel = None;
                self.versions = Some((path, serde_json::from_str::<Value>(&s).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default()));
                self.notice = None;
            }
            Err(e) => self.notice = Some(e),
        }
        cx.notify();
    }

    fn reload_versions(&mut self, cx: &mut Context<Self>) {
        if let Some((p, _)) = self.versions.clone() {
            self.load_versions(p, cx);
        }
    }

    /// Deleted files across every drive and repo that syncs here, newest first.
    fn load_trash(&mut self, cx: &mut Context<Self>) {
        let units = self.list("drives").into_iter().filter(|d| d["paused"].is_null()).chain(self.list("worktrees"));
        let mut all: Vec<Value> = units
            .filter_map(|u| run(&self.bin, &["trash", u["path"].as_str()?, "--json"]).ok())
            .filter_map(|s| serde_json::from_str::<Value>(&s).ok()?.as_array().cloned())
            .flatten()
            .collect();
        all.sort_by_key(|v| std::cmp::Reverse(v["time"].as_i64().unwrap_or(0)));
        self.trash = all;
        self.sel = None;
        cx.notify();
    }

    fn go(&mut self, p: Page, cx: &mut Context<Self>) {
        self.page = p;
        self.notice = None;
        self.sel = None;
        if p == Page::Trash {
            self.load_trash(cx);
        }
        cx.notify();
    }

    /// The folder "Open folder" opens: the Desktop drive when it syncs, else the first drive.
    fn main_folder(&self) -> Option<PathBuf> {
        let drives: Vec<String> = self.list("drives").iter().filter_map(|d| d["path"].as_str().map(String::from)).collect();
        let desk = self.folder.to_string_lossy();
        drives.iter().find(|d| **d == desk).or(drives.first()).map(PathBuf::from)
    }
}


// ---------- Aqua pieces ----------

fn grad(top: u32, bottom: u32) -> gpui::Background {
    linear_gradient(180., linear_color_stop(rgb(top), 0.), linear_color_stop(rgb(bottom), 1.))
}

fn shadow(y: f32, blur: f32, a: f32) -> gpui::BoxShadow {
    gpui::BoxShadow { color: gpui::hsla(0., 0., 0., a), offset: gpui::point(px(0.), px(y)), blur_radius: px(blur), spread_radius: px(0.) }
}

#[derive(Clone, Copy, PartialEq)]
enum Btn {
    /// the blue, pulsing default button: the one Return presses
    Default,
    Normal,
    Disabled,
}

type Click = Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App)>;

/// An Aqua push button: a gel pill, glossy on top and glowing at the bottom.
fn aqua_button(id: impl Into<SharedString>, text: impl Into<SharedString>, kind: Btn, on: Option<Click>) -> gpui::AnyElement {
    let (rim, top, bottom) = match kind {
        Btn::Default => (0x2a5ea8, (0xd3e8ff, 0x8cc1f6), (0x3b8fea, 0xa9d7ff)),
        _ => (0x8c8c8c, (0xffffff, 0xf0f0f0), (0xdadada, 0xfbfbfb)),
    };
    let mut b = div()
        .id(id.into())
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(22.))
        .min_w(px(84.))
        .px(px(18.))
        .rounded(px(11.))
        .border_1()
        .border_color(rgb(rim))
        .shadow(vec![shadow(1., 2., 0.25)])
        .child(div().absolute().top_0().left_0().right_0().h(px(10.)).rounded_t(px(11.)).bg(grad(top.0, top.1)))
        .child(div().absolute().bottom_0().left_0().right_0().h(px(10.)).rounded_b(px(11.)).bg(grad(bottom.0, bottom.1)));
    if kind == Btn::Default {
        // the default button breathes
        b = b.child(
            div().absolute().top_0().bottom_0().left_0().right_0().rounded(px(11.)).bg(rgb(0xffffff)).with_animation(
                "pulse",
                Animation::new(Duration::from_millis(1800)).repeat().with_easing(gpui::bounce(gpui::ease_in_out)),
                |d, t| d.opacity(t * 0.3),
            ),
        );
    }
    b = b.child(div().relative().text_size(px(13.)).text_color(rgb(if kind == Btn::Disabled { 0x9a9a9a } else { 0x000000 })).child(text.into()));
    match on {
        Some(f) if kind != Btn::Disabled => b.active(|s| s.opacity(0.75)).on_click(f).into_any_element(),
        _ => b.into_any_element(),
    }
}

/// A textured button, the kind that sat on brushed metal toolbars.
fn metal_button(id: impl Into<SharedString>, text: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .flex()
        .items_center()
        .h(px(22.))
        .px(px(12.))
        .rounded(px(5.))
        .border_1()
        .border_color(rgba(0x0000008c))
        .bg(grad(0xfdfdfd, 0xcfcfcf))
        .shadow(vec![gpui::BoxShadow { color: gpui::hsla(0., 0., 1., 0.6), offset: gpui::point(px(0.), px(1.)), blur_radius: px(0.), spread_radius: px(0.) }])
        .text_size(px(11.))
        .cursor_default()
        .active(|s| s.bg(grad(0xbdbdbd, 0xe6e6e6)))
        .child(text.into())
}

/// A gel lamp in a traffic-light colour, with its highlight.
fn lamp((fill, rim): (u32, u32), d: f32) -> gpui::Div {
    div().flex_none().relative().size(px(d)).rounded_full().bg(rgb(fill)).border_1().border_color(rgb(rim)).child(
        div().absolute().top(px(d * 0.12)).left(px(d * 0.25)).w(px(d * 0.45)).h(px(d * 0.3)).rounded_full().bg(gpui::hsla(0., 0., 1., 0.55)),
    )
}

/// The indeterminate Aqua progress bar: blue gel, white stripes marching to the right.
fn barber() -> gpui::Div {
    div()
        .relative()
        .h(px(14.))
        .w_full()
        .rounded(px(7.))
        .border_1()
        .border_color(rgb(0x4a78b8))
        .bg(grad(0x9fcdff, 0x2f7fe8))
        .overflow_hidden()
        .child(
            div().absolute().top_0().bottom_0().left(px(6.)).right(px(6.)).overflow_hidden().child(
                div().absolute().top_0().bottom_0().w(relative(1.2)).bg(gpui::pattern_slash(gpui::hsla(0., 0., 1., 0.35), 6., 6.)).with_animation(
                    "barber",
                    Animation::new(Duration::from_millis(900)).repeat(),
                    |d, t| d.left(px(-24. + 24. * t)),
                ),
            ),
        )
        .child(div().absolute().top_0().left_0().right_0().h(px(6.)).rounded_t(px(7.)).bg(linear_gradient(180., linear_color_stop(gpui::hsla(0., 0., 1., 0.6), 0.), linear_color_stop(gpui::hsla(0., 0., 1., 0.15), 1.))))
}

const ROW: f32 = 19.;
const ARROW: f32 = 15.;

/// The Aqua scroller: a pale trough, a blue gel thumb lit down the middle, both arrows at the bottom.
/// The thumb follows the wheel; a click or drag on the trough moves it there.
fn scroller(id: &str, h: &ScrollHandle, cx: &mut Context<Hilo>) -> gpui::Div {
    let (view, max, off) = (f32::from(h.bounds().size.height), f32::from(h.max_offset().height), -f32::from(h.offset().y));
    let track = (view - 2. * ARROW).max(0.);
    let mut trough = div()
        .id(SharedString::from(format!("{id}-trough")))
        .relative()
        .flex_1()
        .bg(linear_gradient(90., linear_color_stop(rgb(0xd4d4d4), 0.), linear_color_stop(rgb(0xfafafa), 1.)));
    if max > 0.5 && track > 0. {
        let thumb = (track * view / (view + max)).clamp(24., track);
        let top = (track - thumb) * (off / max).clamp(0., 1.);
        let half = |left: bool| {
            let (a, b) = if left { (0x2f6fce, 0xb9dcff) } else { (0xb9dcff, 0x4a8fe6) };
            let d = div().h_full().w(px(5.5)).bg(linear_gradient(90., linear_color_stop(rgb(a), 0.), linear_color_stop(rgb(b), 1.)));
            if left { d.rounded_l(px(5.5)) } else { d.rounded_r(px(5.5)) }
        };
        trough = trough.child(div().absolute().left(px(1.)).top(px(top)).h(px(thumb)).w(px(11.)).flex().rounded(px(5.5)).border_1().border_color(rgb(0x2c5fa8)).child(half(true)).child(half(false)));
        // ponytail: a drag only follows while the pointer stays over the trough; a window-wide drag needs gpui's on_drag
        let seek = {
            let h = h.clone();
            move |y: Pixels| {
                let at = f32::from(y - h.bounds().top()) - thumb / 2.;
                h.set_offset(gpui::point(px(0.), px(-(at / (track - thumb).max(1.)).clamp(0., 1.) * max)));
            }
        };
        let seek2 = seek.clone();
        trough = trough
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |_, e: &gpui::MouseDownEvent, _, cx| {
                seek(e.position.y);
                cx.notify();
            }))
            .on_mouse_move(cx.listener(move |_, e: &gpui::MouseMoveEvent, _, cx| {
                if e.pressed_button == Some(gpui::MouseButton::Left) {
                    seek2(e.position.y);
                    cx.notify();
                }
            }));
    }
    let arrow = |n: &str, up: bool| {
        let h = h.clone();
        div()
            .id(SharedString::from(format!("{id}-{n}")))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(px(ARROW))
            .border_t_1()
            .border_color(rgb(0xa5a5a5))
            .bg(linear_gradient(90., linear_color_stop(rgb(0xe6e6e6), 0.), linear_color_stop(rgb(0xffffff), 1.)))
            .active(|s| s.bg(rgb(0xc8d9f2)))
            .text_size(px(7.))
            .text_color(rgb(0x3a3a3a))
            .child(if up { "▲" } else { "▼" })
            .on_click(cx.listener(move |_, _, _, cx| {
                let y = f32::from(h.offset().y) + if up { ROW } else { -ROW };
                h.set_offset(gpui::point(px(0.), px(y.clamp(-f32::from(h.max_offset().height), 0.))));
                cx.notify();
            }))
    };
    div().flex().flex_col().flex_none().w(px(15.)).border_l_1().border_color(rgb(0xa5a5a5)).child(trough).child(arrow("up", true)).child(arrow("down", false))
}

/// Small bold caption over a table, on the metal.
/// An Aqua radio button: blue gel with a dark dot when on.
fn radio(on: bool) -> gpui::Div {
    let d = div().flex().flex_none().items_center().justify_center().size(px(14.)).rounded_full().border_1();
    if on {
        d.border_color(rgb(0x2f5fa8)).bg(grad(0x9cc6f7, 0x2f6fd0)).shadow(vec![shadow(1., 1., 0.25)]).child(div().size(px(5.)).rounded_full().bg(rgb(0x0f2346)))
    } else {
        d.border_color(rgb(0x8a8a8a)).bg(grad(0xffffff, 0xd4d4d4)).shadow(vec![shadow(1., 1., 0.15)])
    }
}

fn para(s: impl Into<SharedString>) -> gpui::Div {
    div().text_size(px(12.)).line_height(px(17.)).child(s.into())
}

/// Text to read or copy, in an inset white field.
fn field(s: impl Into<SharedString>, size: f32) -> gpui::Div {
    div().px(px(8.)).py(px(5.)).bg(rgb(0xffffff)).border_1().border_color(rgb(0x8e8e8e)).font_family(MONO).text_size(px(size)).child(s.into())
}

/// A machine and its folder, in the assistant's welcome.
fn place(who: &'static str, path: impl Into<SharedString>) -> gpui::Div {
    div().flex().flex_col().gap(px(2.)).px(px(12.)).py(px(8.)).bg(rgb(SOURCE)).border_1().border_color(rgb(0xa9b4c4)).rounded(px(4.)).child(div().text_size(px(12.)).font_weight(FontWeight::BOLD).child(who)).child(div().text_size(px(11.)).child(path.into()))
}

/// A command's output, the newest lines that fit.
fn log_box(lines: &[String], h: f32) -> gpui::Div {
    let fit = ((h - 12.) / 14.) as usize;
    div()
        .flex()
        .flex_col()
        .flex_none()
        .h(px(h))
        .p(px(6.))
        .bg(rgb(0xffffff))
        .border_1()
        .border_color(rgb(0x9b9b9b))
        .overflow_hidden()
        .font_family(MONO)
        .text_size(px(10.))
        .line_height(px(14.))
        .children(lines.iter().rev().take(fit).rev().map(|l| div().whitespace_nowrap().child(l.clone())))
}

fn copy_button(id: &'static str, text: &'static str, s: String) -> gpui::AnyElement {
    aqua_button(id, text, Btn::Normal, Some(Box::new(move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| cx.write_to_clipboard(ClipboardItem::new_string(s.clone())))))
}

fn caption(s: impl Into<SharedString>) -> gpui::Div {
    div().text_size(px(11.)).font_weight(FontWeight::BOLD).text_color(rgb(0x3a3a3a)).child(s.into())
}

fn note(s: impl Into<SharedString>) -> gpui::Div {
    div().text_size(px(11.)).text_color(rgb(0x3a3a3a)).child(s.into())
}

fn cell(s: impl Into<SharedString>) -> gpui::AnyElement {
    div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(s.into()).into_any_element()
}

fn lit(l: (u32, u32), s: impl Into<SharedString>) -> gpui::AnyElement {
    div().flex().items_center().gap(px(6.)).min_w_0().child(lamp(l, 9.)).child(cell(s)).into_any_element()
}

/// A column: its title and fixed width (None: takes the rest).
type Col = (&'static str, Option<f32>);

impl Hilo {
    /// A list view: Aqua column headers, rows striped white and blue, the selection in Aqua blue.
    /// Empty stripes fill the rest, as in the Finder. Rows are selectable when `pick`.
    fn table(&self, id: &str, cols: &[Col], rows: Vec<Vec<gpui::AnyElement>>, pick: bool, cx: &mut Context<Self>) -> gpui::Div {
        let width = |d: gpui::Div, w: Option<f32>| match w {
            Some(w) => d.w(px(w)).flex_none(),
            None => d.flex_1().min_w_0(),
        };
        let mut head = div().flex().h(px(17.)).bg(grad(0xffffff, 0xe1e1e1)).border_b_1().border_color(rgb(0x9a9a9a)).text_size(px(11.));
        for (i, (t, w)) in cols.iter().enumerate() {
            head = head.child(width(div().flex().items_center().px(px(6.)).when(i + 1 < cols.len(), |d| d.border_r_1().border_color(rgb(0xc5c5c5))).child(*t), *w));
        }
        // the corner over the scroller
        head = head.child(div().flex_none().w(px(15.)).border_l_1().border_color(rgb(0xc5c5c5)));
        let scroll = self.scroll(id);
        let mut body = div().id(SharedString::from(format!("{id}-rows"))).absolute().top_0().left_0().size_full().flex().flex_col().overflow_y_scroll().track_scroll(&scroll).on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()));
        for (i, r) in rows.into_iter().enumerate() {
            let on = pick && self.sel == Some(i);
            let mut row = div().id(SharedString::from(format!("{id}-{i}"))).flex().flex_none().items_center().h(px(ROW)).text_size(px(12.));
            row = if on { row.bg(rgb(SELECT)).text_color(rgb(0xffffff)) } else { row.bg(rgb(if i % 2 == 1 { STRIPE } else { 0xffffff })) };
            for (c, (_, w)) in r.into_iter().zip(cols) {
                row = row.child(width(div().flex().items_center().px(px(6.)).overflow_hidden(), *w).child(c));
            }
            if pick {
                row = row.on_click(cx.listener(move |this, _, _, cx| {
                    this.sel = Some(i);
                    cx.notify();
                }));
            }
            body = body.child(row);
        }
        // the stripes go on under the last row, as in the Finder
        let stripes = div().absolute().top_0().left_0().size_full().flex().flex_col().overflow_hidden().children((0..60).map(|i| div().flex_none().h(px(ROW)).when(i % 2 == 1, |d| d.bg(rgb(STRIPE)))));
        let view = div().relative().flex_1().min_w_0().child(stripes).child(body);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(60.))
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(0x7b7b7b))
            .overflow_hidden()
            .child(head)
            .child(div().flex().flex_1().min_h_0().child(view).child(scroller(id, &scroll, cx)))
    }

    fn scroll(&self, id: &str) -> ScrollHandle {
        self.scrolls.borrow_mut().entry(id.to_string()).or_default().clone()
    }

    fn click(&self, cx: &mut Context<Self>, f: impl Fn(&mut Hilo, &mut Context<Hilo>) + 'static) -> Option<Click> {
        Some(Box::new(cx.listener(move |this, _, _, cx| f(this, cx))))
    }
}

// ---------- pages ----------

impl Hilo {
    /// One choice in the assistant: an Aqua radio button, its name and what it means.
    fn choice(&self, way: Way, name: &'static str, about: &'static str, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        div()
            .id(name)
            .flex()
            .gap(px(10.))
            .py(px(6.))
            .child(div().pt(px(1.)).child(radio(self.way == way)))
            .child(div().flex().flex_col().flex_1().min_w_0().gap(px(2.)).child(div().text_size(px(13.)).font_weight(FontWeight::BOLD).child(name)).child(para(about).text_color(rgb(0x4a4a4a))))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.way = way;
                cx.notify();
            }))
    }

    /// The setup assistant, as in the Mac OS X Installer: the steps down the left, a white pane, and
    /// Go Back / Continue under it. Its last steps run `hilo setup`, `join`, `init` or `invite`.
    fn tour_page(&mut self, step: Step, cx: &mut Context<Self>) -> gpui::Div {
        let folder = tilde(&self.folder.to_string_lossy());
        let job = self.job.lock().unwrap();
        let (running, failed, job_title, lines) = (job.running, job.ok == Some(false), job.title.clone(), job.lines.clone());
        drop(job);
        let next = |id: &'static str, text: String, on: Option<Click>| aqua_button(id, text, if on.is_some() { Btn::Default } else { Btn::Disabled }, on);
        let to = |cx: &mut Context<Self>, s: Step| self.click(cx, move |this, cx| this.step(s, cx));

        let (title, body, back, cont): (String, gpui::Div, Option<Step>, gpui::AnyElement) = match step {
            Step::Welcome => (
                "Welcome to hilo".into(),
                div()
                    .child(para("hilo keeps one folder the same on this Mac and on your other computers. Put a file in it here and it shows up there a moment later, and the other way round."))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .py(px(6.))
                            .child(place("This Mac", folder.clone()))
                            .child(div().text_size(px(18.)).text_color(rgb(0x3875d7)).child("⇄"))
                            .child(place("Your other computer", "its hilo folder")),
                    )
                    .child(para("It takes about a minute. Have your other computer at hand: another Mac, a Linux or Windows PC, or a server.")),
                None,
                next("go", "Continue".into(), to(cx, Step::Choose)),
            ),
            Step::Choose => {
                let mut list = div().flex().flex_col().gap(px(4.)).child(self.choice(
                    Way::Ssh,
                    "Set it up from here",
                    "For a server or a computer you reach with ssh. hilo installs itself there and pairs the two: nothing to do on the other side.",
                    cx,
                ));
                if self.adding {
                    list = list.child(self.choice(Way::Invite, "Give me a code to type there", "For a computer you can't reach with ssh. Install hilo there and paste the code.", cx));
                } else {
                    list = list
                        .child(self.choice(
                            Way::Code,
                            "It already runs hilo",
                            "Get a join code from the computer that keeps the shared copy: Add Computer in its hilo app, or hilo invite in its terminal.",
                            cx,
                        ))
                        .child(self.choice(Way::Server, "This Mac is the first one", "Your other computers will connect to this Mac. You'll get a code to type on each of them.", cx));
                }
                let go = if self.way == Way::Invite { self.click(cx, |this, cx| this.set_up(cx)) } else { to(cx, Step::Connect) };
                ("How do you want to connect?".into(), div().child(list), (!self.adding).then_some(Step::Welcome), next("go", "Continue".into(), go))
            }
            Step::Connect => match self.way {
                Way::Ssh => {
                    let rows: Vec<_> = self.hosts.iter().map(|(h, a)| vec![cell(h.clone()), cell(a.clone())]).collect();
                    let picked = self.sel.and_then(|i| self.hosts.get(i)).map(|h| h.0.clone());
                    let mut d = div().child(para("These come from your SSH config (~/.ssh/config). hilo signs in the same way ssh does, with your key, and installs itself in your home folder there."));
                    d = if rows.is_empty() {
                        d.child(para("There are no computers in your SSH config yet. Add yours there and click Check Again, or go back and use a join code."))
                            .child(div().flex().child(aqua_button("again", "Check Again", Btn::Normal, self.click(cx, |this, cx| {
                                this.hosts = load_hosts(&this.bin);
                                cx.notify();
                            }))))
                    } else {
                        d.child(self.table("hosts", &[("Name", None), ("Address", Some(220.))], rows, true, cx))
                    };
                    let go = match picked {
                        Some(h) => next("go", format!("Set Up {h}"), self.click(cx, |this, cx| this.set_up(cx))),
                        None => next("go", "Set Up".into(), None),
                    };
                    ("Choose your other computer".into(), d, Some(Step::Choose), go)
                }
                Way::Code => {
                    let shown = self.code.clone();
                    (
                        "Paste the join code".into(),
                        div()
                            .child(para("Copy the code on the other computer, then click Paste. It looks like HILO-7KQ2-M9PA-XR4T@192.168.1.20:7710."))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .child(field(shown.clone().unwrap_or("No code yet".into()), 12.).flex_1().when(shown.is_none(), |d| d.text_color(rgb(0x8a8a8a))))
                                    .child(aqua_button("paste", "Paste", Btn::Normal, self.click(cx, |this, cx| this.paste_code(cx)))),
                            ),
                        Some(Step::Choose),
                        next("go", "Join".into(), shown.and_then(|_| self.click(cx, |this, cx| this.set_up(cx)))),
                    )
                }
                _ => (
                    "This Mac keeps the shared copy".into(),
                    div()
                        .child(para("Your other computers connect to this Mac to stay in sync, so it works best when it stays on. They need to reach it: on the same network, or over a VPN like Tailscale."))
                        .child(para("hilo starts in the background now, and every time you log in. Then it gives you a code for the other computers.")),
                    Some(Step::Choose),
                    next("go", "Start".into(), self.click(cx, |this, cx| this.set_up(cx))),
                ),
            },
            Step::Working => {
                let back = if self.way == Way::Invite { Step::Choose } else { Step::Connect };
                (
                    if failed { "That didn't work".into() } else { job_title },
                    div()
                        .child(para(if failed {
                            "Here's what hilo said. Fix it and click Try Again, or go back and choose another way."
                        } else {
                            "This can take a minute. Leave this window open."
                        }))
                        .child(log_box(&lines, 170.))
                        .when(running, |d| d.child(barber())),
                    failed.then_some(back),
                    if failed { next("go", "Try Again".into(), self.click(cx, |this, cx| this.set_up(cx))) } else { next("go", "Continue".into(), None) },
                )
            }
            Step::Done => {
                let mut d = div();
                if matches!(self.way, Way::Server | Way::Invite) {
                    let code = self.job_code().unwrap_or_default();
                    let cmd = format!("curl -fsSL https://raw.githubusercontent.com/damkandev/hilo/main/install.sh | sh -s -- join {code} --as {LABEL} --folder ~/{LABEL}");
                    d = d
                        .child(para("On your other computer, get hilo from hilo.dapan.es, choose “It already runs hilo” and paste this code:"))
                        .child(div().flex().items_center().gap(px(10.)).child(field(code.clone(), 14.).font_weight(FontWeight::BOLD).flex_1()).child(copy_button("copy", "Copy Code", code)))
                        .child(para("Or run this in a terminal there (Linux or Mac): it installs hilo and joins in one go."))
                        .child(div().flex().items_center().gap(px(10.)).child(field(cmd.clone(), 10.).flex_1().overflow_hidden().whitespace_nowrap()).child(copy_button("copycmd", "Copy Command", cmd)))
                        .child(para("Each code works once. For one more computer, click Add Computer in the toolbar."));
                } else {
                    let other = if self.way == Way::Ssh { self.target.as_str() } else { "the other computer" };
                    d = d.child(para(format!("{folder} on this Mac and the hilo folder on {other} are now the same folder. Put a file in it and it shows up there a moment later.")));
                }
                if self.way != Way::Invite {
                    let f = self.folder.clone();
                    d = d.child(para("hilo keeps running in the background. This window shows how it's doing, older versions of every file, and what was deleted.")).child(
                        div().flex().child(aqua_button("open", "Open the hilo Folder", Btn::Normal, Some(Box::new(move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| cx.open_with_system(&f))))),
                    );
                }
                (if self.way == Way::Invite { "One more computer".into() } else { "hilo is ready".into() }, d, None, next("go", "Done".into(), self.click(cx, |this, cx| this.close_tour(cx))))
            }
        };

        let steps = div().flex().flex_col().flex_none().w(px(150.)).pt(px(14.)).gap(px(10.)).children(STEPS.iter().enumerate().map(|(i, s)| {
            let at = i == step as usize;
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(10.))
                .text_size(px(12.))
                .when(at, |d| d.font_weight(FontWeight::BOLD))
                .when(i > step as usize, |d| d.text_color(rgb(0x5e5e5e)))
                .child(lamp(if at { BLUE } else { OFF }, 9.))
                .child(*s)
        }));
        let mut buttons = div().flex().items_center().gap(px(10.)).pt(px(8.));
        if self.adding && step != Step::Done {
            buttons = buttons.child(aqua_button("cancel", "Cancel", Btn::Normal, self.click(cx, |this, cx| this.close_tour(cx))));
        }
        buttons = buttons.child(div().flex_1());
        if step != Step::Done && !(self.adding && step == Step::Choose) {
            buttons = buttons.child(aqua_button("back", "Go Back", if back.is_some() { Btn::Normal } else { Btn::Disabled }, back.and_then(|b| to(cx, b))));
        }
        let pane = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(0x7b7b7b))
            .child(div().px(px(20.)).pt(px(16.)).pb(px(8.)).text_size(px(15.)).font_weight(FontWeight::BOLD).child(title))
            .child(body.flex().flex_col().flex_1().min_h_0().gap(px(10.)).px(px(20.)).pb(px(16.)));
        div().flex().flex_1().min_h_0().gap(px(8.)).child(steps).child(div().flex().flex_col().flex_1().min_w_0().min_h_0().child(pane).child(buttons.child(cont)))
    }

    /// Starts the daemon: through its launchd job when there is one, else as a detached process.
    fn start_daemon(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        let ok = {
            let plist = home().join("Library/LaunchAgents/dev.hilo.daemon.plist");
            if plist.exists() {
                Command::new("launchctl").args(["bootstrap", &format!("gui/{}", unsafe { getuid() }), &plist.to_string_lossy()]).status().is_ok()
            } else {
                let log = std::fs::OpenOptions::new().create(true).append(true).open(home().join("Library/Logs/hilo.log"));
                // ponytail: plain child process, it outlives the app; the launchd job is the real fix (`hilo init/join` installs it)
                Command::new(&self.bin).arg("daemon").stdin(Stdio::null()).stdout(Stdio::null()).stderr(log.map_or(Stdio::null(), Stdio::from)).spawn().is_ok()
            }
        };
        // Windows: a child with no console window, which outlives the app. It logs where the CLI's own daemon does.
        #[cfg(windows)]
        let ok = {
            let log = std::fs::OpenOptions::new().create(true).append(true).open(home().join(".hilo").join("daemon.log"));
            let mut c = Command::new(&self.bin);
            c.arg("daemon").stdin(Stdio::null()).stdout(Stdio::null()).stderr(log.map_or(Stdio::null(), Stdio::from));
            c.creation_flags(0x08000000);
            c.spawn().is_ok()
        };
        self.notice = (!ok).then(|| "Could not start hilo. Run `hilo daemon` in Terminal to see why.".into());
        cx.notify();
    }

    fn daemon_up(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s["daemon"].as_bool() != Some(false))
    }

    fn status_page(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let s = self.status.clone().unwrap_or_default();
        let daemon = self.daemon_up();
        let me = s["machine"].as_str().unwrap_or_default().to_string();
        let n = |k: &str| s[k].as_i64().unwrap_or(0);
        let mut machines: Vec<Vec<gpui::AnyElement>> = s["online"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|m| vec![lit(GREEN, m.to_string()), cell(if m == me { "this Mac" } else { "online" })])
            .collect();
        if !daemon {
            machines.push(vec![lit(OFF, me.clone()), cell("stopped")]);
        }
        let mut here: Vec<Vec<gpui::AnyElement>> = vec![];
        for d in self.list("drives") {
            let paused = d["paused"].as_str().filter(|_| daemon);
            let pending = d["pending"].as_i64().unwrap_or(0);
            let state = match paused {
                Some(p) => format!("paused: {p}"),
                None if !daemon => "not syncing".into(),
                None if pending > 0 => format!("sending {pending}"),
                None => "in sync".into(),
            };
            let l = if !daemon { OFF } else if paused.is_some() { YELLOW } else { GREEN };
            here.push(vec![lit(l, tilde(d["path"].as_str().unwrap_or_default())), cell(d["files"].as_i64().map_or(String::new(), |f| format!("{f} files"))), cell(state)]);
        }
        for f in self.list("folders") {
            let l = if daemon { GREEN } else { OFF };
            here.push(vec![lit(l, tilde(f["path"].as_str().unwrap_or_default())), cell(f["repos"].as_i64().map_or("code".into(), |n| format!("{n} repos"))), cell(if daemon { "in sync" } else { "not syncing" })]);
        }
        let mut alerts: Vec<Vec<gpui::AnyElement>> = vec![];
        if n("conflicts") + n("drive_conflicts") > 0 {
            alerts.push(vec![lit(YELLOW, format!("{} conflicts saved", n("conflicts") + n("drive_conflicts"))), cell("nothing was lost: see Versions")]);
        }
        if n("waiting_approval") > 0 {
            alerts.push(vec![lit(YELLOW, format!("{} changes to files that run code", n("waiting_approval"))), cell("run hilo approve")]);
        }
        let mut page = div().flex().flex_col().flex_1().min_h_0().gap(px(6.));
        if !alerts.is_empty() {
            page = page.child(caption("Needs Attention")).child(div().flex().flex_col().h(px(60.)).child(self.table("alerts", &[("Item", None), ("What to do", Some(240.))], alerts, false, cx)));
        }
        page.child(caption("Machines"))
            .child(div().flex().flex_col().h(px(110.)).child(self.table("machines", &[("Name", None), ("Status", Some(160.))], machines, false, cx)))
            .child(caption("Syncing Here"))
            .child(self.table("here", &[("Folder", None), ("Contents", Some(110.)), ("Status", Some(160.))], here, false, cx))
    }

    fn folders_page(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let entries: Vec<(String, &str, String)> = self
            .list("drives")
            .iter()
            .map(|d| (d["path"].as_str().unwrap_or_default().to_string(), "Files", d["files"].as_i64().map_or(String::new(), |f| format!("{f} files"))))
            .chain(self.list("folders").iter().map(|f| (f["path"].as_str().unwrap_or_default().to_string(), "Code", f["repos"].as_i64().map_or(String::new(), |n| format!("{n} repos")))))
            .collect();
        let picked = self.sel.and_then(|i| entries.get(i)).map(|e| e.0.clone());
        let rows = entries.iter().map(|(p, k, c)| vec![cell(tilde(p)), cell(*k), cell(c.clone())]).collect();
        let (show, stop) = match picked {
            Some(p) => {
                let (open, rm) = (PathBuf::from(&p), p.clone());
                (
                    aqua_button("show", reveal_label(OS), Btn::Normal, Some(Box::new(move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| cx.reveal_path(&open)))),
                    aqua_button("stop", "Stop Syncing", Btn::Normal, self.click(cx, move |this, cx| this.start(&format!("Unlinking {}", tilde(&rm)), vec!["remove".into(), rm.clone()], None, cx))),
                )
            }
            None => (aqua_button("show", reveal_label(OS), Btn::Disabled, None), aqua_button("stop", "Stop Syncing", Btn::Disabled, None)),
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap(px(6.))
            .child(note("Add the same folder on your other machine too: folders with the same name sync with each other."))
            .child(self.table("folders", &[("Folder", None), ("Kind", Some(90.)), ("Contents", Some(110.))], rows, true, cx))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .pt(px(4.))
                    .child(aqua_button("addd", "Add Folder of Files…", Btn::Normal, self.click(cx, |this, cx| this.add_folder(true, cx))))
                    .child(aqua_button("addc", "Add Code Folder…", Btn::Normal, self.click(cx, |this, cx| this.add_folder(false, cx))))
                    .child(div().flex_1())
                    .child(show)
                    .child(stop),
            )
    }

    fn versions_page(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let (path, versions) = self.versions.clone().unwrap_or_default();
        let rows = versions
            .iter()
            .map(|v| {
                let size = if v["size"].is_null() { "deleted".into() } else { bytes(v["size"].as_i64().unwrap_or(0)) };
                vec![cell(ago(v["time"].as_i64().unwrap_or(0))), cell(v["kind"].as_str().unwrap_or_default().to_string()), cell(v["origin"].as_str().unwrap_or_default().to_string()), cell(size)]
            })
            .collect();
        let pick = self.sel.and_then(|i| versions.get(i)).filter(|v| !v["size"].is_null()).and_then(|v| v["n"].as_i64());
        let restore = match pick {
            Some(n) => {
                let p = path.clone();
                aqua_button("restore", "Restore", Btn::Default, self.click(cx, move |this, cx| this.start("Restoring", vec!["restore".into(), p.clone(), "--at".into(), n.to_string()], Some(Hilo::reload_versions), cx)))
            }
            None => aqua_button("restore", "Restore", Btn::Disabled, None),
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(aqua_button("pick", "Choose File…", Btn::Normal, self.click(cx, |this, cx| this.pick_file(cx))))
                    .child(note(if path.is_empty() { "Every version hilo kept of a file, including the one that lost a conflict.".into() } else { tilde(&path) })),
            )
            .child(self.table("versions", &[("Date", Some(130.)), ("Change", Some(90.)), ("Machine", None), ("Size", Some(80.))], rows, true, cx))
            .child(div().flex().justify_end().pt(px(4.)).child(restore))
    }

    fn trash_page(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let rows = self
            .trash
            .iter()
            .map(|v| vec![cell(tilde(v["path"].as_str().unwrap_or_default())), cell(ago(v["time"].as_i64().unwrap_or(0))), cell(v["origin"].as_str().unwrap_or("?").to_string())])
            .collect();
        let back = match self.sel.and_then(|i| self.trash.get(i)).and_then(|v| v["path"].as_str()).map(String::from) {
            Some(p) => aqua_button("back", "Bring Back", Btn::Default, self.click(cx, move |this, cx| this.start("Bringing it back", vec!["restore".into(), p.clone()], Some(Hilo::load_trash), cx))),
            None => aqua_button("back", "Bring Back", Btn::Disabled, None),
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap(px(6.))
            .child(note("Files deleted on any machine, kept for 30 days."))
            .child(self.table("trash", &[("Name", None), ("Deleted", Some(110.)), ("By", Some(140.))], rows, true, cx))
            .child(div().flex().gap(px(8.)).pt(px(4.)).child(aqua_button("reload", "Refresh", Btn::Normal, self.click(cx, |this, cx| this.load_trash(cx)))).child(div().flex_1()).child(back))
    }

    /// A sheet: it drops from under the toolbar, with the command's output and the Aqua progress bar.
    fn sheet(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let (title, lines, running, code) = if let Some(n) = &self.notice {
            ("hilo".to_string(), vec![n.clone()], false, None)
        } else {
            let j = self.job.lock().unwrap();
            // the assistant shows its own commands
            if j.title.is_empty() || self.tour.is_some() {
                return None;
            }
            let code = j.lines.iter().find_map(|l| find_code(l));
            let t = match (j.running, j.ok) {
                (true, _) => j.title.clone(),
                (_, Some(true)) => format!("{}: done", j.title),
                _ => format!("{}: failed", j.title),
            };
            (t, j.lines.clone(), j.running, code)
        };
        let out = log_box(&lines, 130.);
        let mut buttons = div().flex().justify_end().gap(px(10.));
        if let Some(code) = code {
            let cmd = format!("hilo join {code} --as {LABEL} --folder ~/{LABEL}");
            buttons = buttons.child(aqua_button("copy", "Copy Join Command", Btn::Normal, Some(Box::new(move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| cx.write_to_clipboard(ClipboardItem::new_string(cmd.clone()))))));
        }
        buttons = buttons.child(if running {
            aqua_button("ok", "OK", Btn::Disabled, None)
        } else {
            aqua_button(
                "ok",
                "OK",
                Btn::Default,
                self.click(cx, |this, cx| {
                    this.notice = None;
                    *this.job.lock().unwrap() = Job::default();
                    cx.notify();
                }),
            )
        });
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .w(px(500.))
                .p(px(16.))
                .bg(rgb(0xececec))
                .border_1()
                .border_t_0()
                .border_color(rgb(0x8e8e8e))
                .rounded_b(px(6.))
                .shadow(vec![shadow(8., 22., 0.4)])
                .child(div().text_size(px(13.)).font_weight(FontWeight::BOLD).child(title))
                .child(out)
                .when(running, |d| d.child(barber()))
                .when(!running, |d| d.child(div().h(px(14.))))
                .child(buttons),
        )
    }

    /// The iTunes LCD in the middle of the toolbar: how hilo is doing.
    fn lcd(&self) -> gpui::Div {
        let s = self.status.clone().unwrap_or_default();
        let set_up = !self.str("role").is_empty();
        let (l, line1) = match (set_up, self.daemon_up(), s["connection"].as_str().unwrap_or("")) {
            (false, _, _) => (OFF, "Not set up yet".to_string()),
            (_, false, _) => (RED, "hilo isn't running".into()),
            (_, _, "in sync") => (GREEN, "In sync".into()),
            (_, _, "catching up") => (YELLOW, "Catching up".into()),
            _ => (RED, "Offline".into()),
        };
        let me = s["machine"].as_str().unwrap_or_default();
        let line2 = match s["role"].as_str() {
            _ if !set_up => tilde(&self.folder.to_string_lossy()),
            Some("member") => format!("{me} · anchor at {}", s["anchor"].as_str().unwrap_or("?")),
            _ => format!("{me} · the anchor"),
        };
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .w(px(360.))
            .h(px(40.))
            .rounded(px(7.))
            .border_1()
            .border_color(rgb(0x6e6e6e))
            .bg(grad(0xfbfcef, 0xd3d9bf))
            .shadow(vec![gpui::BoxShadow { color: gpui::hsla(0., 0., 1., 0.7), offset: gpui::point(px(0.), px(1.)), blur_radius: px(0.), spread_radius: px(0.) }])
            .text_color(rgb(0x262626))
            .child(div().flex().items_center().gap(px(6.)).text_size(px(12.)).font_weight(FontWeight::BOLD).child(lamp(l, 9.)).child(line1))
            .child(div().text_size(px(11.)).child(line2))
    }
}

impl Render for Hilo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let set_up = !self.str("role").is_empty();
        let cur = self.page;

        // title bar: the system lights on the left, the title centred. Where the system draws the bar there's none of ours.
        let bar = if transparent_titlebar(OS) { 22. } else { 0. };
        let title = div().flex().flex_none().justify_center().items_center().h(px(bar)).text_size(px(13.)).text_color(rgb(0x262626)).when(bar > 0., |d| d.child("hilo"));

        // toolbar: a textured button on each side, the LCD in the middle
        let mut left = div().flex().flex_1().items_center();
        if let Some(f) = self.main_folder() {
            left = left.child(metal_button("open", "Open Folder").on_click(cx.listener(move |_, _, _, cx| cx.open_with_system(&f))));
        }
        let mut right = div().flex().flex_1().items_center().justify_end();
        if set_up && !self.daemon_up() {
            right = right.child(metal_button("start", "Start hilo").on_click(cx.listener(|this, _, _, cx| this.start_daemon(cx))));
        } else if self.tour.is_none() && self.str("role") == "anchor" {
            // ponytail: only the anchor adds machines here; on a member, its anchor hands out the codes
            right = right.child(metal_button("add", "Add Computer").on_click(cx.listener(|this, _, _, cx| this.open_tour(true, cx))));
        }
        let toolbar = div().flex().flex_none().items_center().h(px(50.)).px(px(12.)).child(left).child(self.lcd()).child(right);

        // the source list
        let nav = |id: &'static str, text: &'static str, p: Page, cx: &mut Context<Self>| {
            let on = p == cur;
            div()
                .id(id)
                .flex()
                .items_center()
                .h(px(22.))
                .px(px(14.))
                .text_size(px(12.))
                .when(on, |d| d.bg(grad(0x6d9ddb, 0x3c70c4)).border_t_1().border_color(rgb(0x5a88cc)).text_color(rgb(0xffffff)).font_weight(FontWeight::BOLD))
                .child(text)
                .on_click(cx.listener(move |this, _, _, cx| this.go(p, cx)))
        };
        let mut body = div().flex().flex_1().min_h_0().gap(px(8.)).px(px(10.));
        if set_up && self.tour.is_none() {
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(px(150.))
                    .bg(rgb(SOURCE))
                    .border_1()
                    .border_color(rgb(0x7b7b7b))
                    .child(div().px(px(8.)).pt(px(6.)).pb(px(2.)).text_size(px(11.)).font_weight(FontWeight::BOLD).text_color(rgb(0x6f7c8f)).child("HILO"))
                    .child(nav("n-status", "Status", Page::Status, cx))
                    .child(nav("n-folders", "Folders", Page::Folders, cx))
                    .child(nav("n-versions", "Versions", Page::Versions, cx))
                    .child(nav("n-trash", "Trash", Page::Trash, cx)),
            );
        }
        let page = match (self.tour, set_up, self.page) {
            (Some(s), ..) => self.tour_page(s, cx),
            // the first status hasn't arrived yet
            (None, false, _) => div(),
            (_, _, Page::Status) => self.status_page(cx),
            (_, _, Page::Folders) => self.folders_page(cx),
            (_, _, Page::Versions) => self.versions_page(cx),
            (_, _, Page::Trash) => self.trash_page(cx),
        };
        body = body.child(div().flex().flex_col().flex_1().min_w_0().min_h_0().child(page));

        // status bar, as at the bottom of iTunes and the Finder
        let s = self.status.clone().unwrap_or_default();
        let online = s["online"].as_array().map_or(0, Vec::len);
        let foot = if set_up {
            format!("{online} machine{} online, {} versions kept, {}", if online == 1 { "" } else { "s" }, s["versions"].as_i64().unwrap_or(0), bytes(s["history_bytes"].as_i64().unwrap_or(0)))
        } else {
            String::new()
        };
        let status_bar = div().flex().flex_none().justify_center().items_center().h(px(24.)).text_size(px(11.)).text_color(rgb(0x262626)).child(foot);

        let mut root = div()
            .relative()
            .size_full()
            .font_family(UI)
            .text_color(rgb(0x000000))
            .text_size(px(13.))
            .child(gpui::img(self.metal.clone()).absolute().top_0().left_0().size_full().object_fit(gpui::ObjectFit::Fill))
            .child(div().absolute().top_0().left_0().size_full().flex().flex_col().child(title).child(toolbar).child(body).child(status_bar));
        if let Some(sheet) = self.sheet(cx) {
            root = root.child(div().absolute().top(px(bar + 50.)).left_0().right_0().flex().justify_center().child(sheet));
        }
        root
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus(vec![Menu { name: "hilo".into(), items: vec![MenuItem::action("Quit hilo", Quit)] }]);
        cx.bind_keys([gpui::KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_window_closed(|cx| cx.quit()).detach();
        let bounds = Bounds::centered(None, size(px(860.), px(600.)), cx);
        // Windows draws its own title bar: the drag area and the close, minimise and maximise buttons
        let titlebar = if transparent_titlebar(OS) {
            TitlebarOptions { title: Some("hilo".into()), appears_transparent: true, traffic_light_position: Some(gpui::point(px(9.), px(5.))) }
        } else {
            TitlebarOptions { title: Some("hilo".into()), appears_transparent: false, traffic_light_position: None }
        };
        let opts = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(titlebar),
            window_min_size: Some(size(px(700.), px(460.))),
            ..Default::default()
        };
        cx.open_window(opts, |_, cx| cx.new(Hilo::new)).unwrap();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: [Os; 2] = [Os::Mac, Os::Windows];

    fn env(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    fn local_bin(home: &str, os: Os) -> PathBuf {
        Path::new(home).join(".local").join("bin").join(bin_name(os))
    }

    #[test]
    fn home_windows_falls_back_to_userprofile() {
        assert_eq!(home_in(Os::Windows, env(&[("USERPROFILE", r"C:\Users\ana")])), PathBuf::from(r"C:\Users\ana"));
    }

    #[test]
    fn home_windows_prefers_home() {
        assert_eq!(home_in(Os::Windows, env(&[("HOME", r"C:\msys\ana"), ("USERPROFILE", r"C:\Users\ana")])), PathBuf::from(r"C:\msys\ana"));
        assert_eq!(home_in(Os::Windows, |k| Some(format!("/{k}"))), PathBuf::from("/HOME"));
    }

    #[test]
    fn home_mac_uses_home() {
        assert_eq!(home_in(Os::Mac, env(&[("HOME", "/Users/ana")])), PathBuf::from("/Users/ana"));
    }

    #[test]
    fn home_mac_ignores_userprofile() {
        assert_eq!(home_in(Os::Mac, env(&[("USERPROFILE", "x")])), PathBuf::new());
    }

    #[test]
    fn home_is_empty_without_variables() {
        for os in BOTH {
            assert_eq!(home_in(os, |_| None), PathBuf::new());
        }
    }

    #[test]
    fn bin_name_per_platform() {
        assert_eq!(bin_name(Os::Mac), "hilo");
        assert_eq!(bin_name(Os::Windows), "hilo.exe");
    }

    #[test]
    fn find_hilo_prefers_the_one_beside_the_app() {
        for os in BOTH {
            assert_eq!(find_hilo_in(os, Some(Path::new("/app")), Path::new("/h"), |_| true), Path::new("/app").join(bin_name(os)));
        }
    }

    #[test]
    fn find_hilo_falls_back_to_local_bin() {
        for os in BOTH {
            let want = local_bin("/h", os);
            assert_eq!(find_hilo_in(os, Some(Path::new("/app")), Path::new("/h"), |p| p == want), want);
        }
    }

    #[test]
    fn find_hilo_falls_back_to_path() {
        assert_eq!(find_hilo_in(Os::Mac, Some(Path::new("/app")), Path::new("/h"), |_| false), PathBuf::from("hilo"));
        assert_eq!(find_hilo_in(Os::Windows, Some(Path::new("/app")), Path::new("/h"), |_| false), PathBuf::from("hilo.exe"));
    }

    #[test]
    fn find_hilo_without_exe_dir() {
        for os in BOTH {
            let want = local_bin("/h", os);
            assert_eq!(find_hilo_in(os, None, Path::new("/h"), |_| true), want);
            assert_eq!(find_hilo_in(os, None, Path::new("/h"), |p| p == want), want);
            assert_eq!(find_hilo_in(os, None, Path::new("/h"), |_| false), PathBuf::from(bin_name(os)));
        }
    }

    #[test]
    fn find_hilo_only_looks_for_this_platforms_name() {
        // a Mac binary beside a Windows app is not the CLI, and the other way round
        let mac = Path::new("/app").join("hilo");
        assert_eq!(find_hilo_in(Os::Windows, Some(Path::new("/app")), Path::new("/h"), |p| p == mac), PathBuf::from("hilo.exe"));
        let win = Path::new("/app").join("hilo.exe");
        assert_eq!(find_hilo_in(Os::Mac, Some(Path::new("/app")), Path::new("/h"), |p| p == win), PathBuf::from("hilo"));
    }

    #[test]
    fn reveal_label_per_platform() {
        assert_eq!(reveal_label(Os::Mac), "Show in Finder");
        assert_eq!(reveal_label(Os::Windows), "Show in Explorer");
    }

    #[test]
    fn titlebar_per_platform() {
        assert!(transparent_titlebar(Os::Mac));
        assert!(!transparent_titlebar(Os::Windows));
    }
}
