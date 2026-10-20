//! THROWAWAY SPIKE (branch `feature/tabs-spike`, never merged): one native window hosting N child webviews (tabs),
//! each running the normal frontend with its own workspace (registry keyed by webview label).
//!
//! Env (spike mode = `PERMARK_TABS_SPIKE` set):
//! - `PERMARK_TABS_SPIKE=N`: 0 -> normal `main` window (baseline); 1..9 -> window `w-spike` + N tab webviews.
//! - `PERMARK_TABS_SPIKE_PATHS`: `;`-separated workspaces; tab i opens path i (cycled). Unset -> Welcome.
//! - `PERMARK_TABS_SPIKE_LAZY=1`: only tab 1 created at start; others on first activation.
//! - `PERMARK_TABS_SPIKE_PLAN=1`: scripted run: READY -> cycle tabs -> CYCLED -> resize -> DONE -> exit.
//! - `PERMARK_TABS_SPIKE_HOLD_MS`: pause after READY | CYCLED (default 6000; the measuring script samples then).
//! - `PERMARK_TABS_SPIKE_LOG=<file>`: log lines `<ms since start> <event>`; else stderr.
//!
//! Spike mode: isolated profile (`%TEMP%/permark-tabs-spike/profile-<pid>`, shared webview cache next to it), no
//! single-instance socket -> never touches the person's PerMark state, never forwards to a running PerMark.
//! Keys (init script, every tab): Ctrl+Tab | Ctrl+Shift+Tab | Mod+1..9 -> `spike_tab`; other keydowns logged.

#![allow(clippy::needless_pass_by_value)] // Tauri commands take their arguments by value

use std::fmt::Display;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use permark_core::state::AppDirs;
use permark_core::workspace::WorkspaceRef;
use tauri::webview::{PageLoadEvent, PageLoadPayload, WebviewBuilder};
use tauri::window::WindowBuilder;
use tauri::{
    AppHandle, Manager, PhysicalPosition, PhysicalSize, Rect, Webview, WebviewEvent, WebviewUrl,
    Window, WindowEvent,
};

use crate::shell::state;

pub const WINDOW: &str = "w-spike";
const MAX_TABS: usize = 9;

const INIT: &str = r#"(() => {
  const inv = (cmd, args) => { try { return window.__TAURI_INTERNALS__.invoke(cmd, args).catch(() => {}); } catch (_) {} };
  const log = (msg) => inv("spike_log", { msg });
  window.addEventListener("keydown", (e) => {
    const mod = e.ctrlKey || e.metaKey;
    if (e.ctrlKey && e.key === "Tab") {
      e.preventDefault(); e.stopImmediatePropagation();
      log("key " + (e.shiftKey ? "Ctrl+Shift+Tab" : "Ctrl+Tab"));
      inv("spike_tab", { to: e.shiftKey ? "prev" : "next" });
      return;
    }
    if (mod && !e.shiftKey && !e.altKey && e.key >= "1" && e.key <= "9") {
      e.preventDefault(); e.stopImmediatePropagation();
      log("key Mod+" + e.key);
      inv("spike_tab", { to: e.key });
      return;
    }
    const t = e.target;
    log("keydown " + e.key + " target=" + ((t && t.tagName) || "?") + " hasFocus=" + document.hasFocus());
  }, true);
  window.addEventListener("focus", () => log("dom focus"));
  window.addEventListener("blur", () => log("dom blur"));
  document.addEventListener("visibilitychange", () => log("visibility " + document.visibilityState));
  window.__PERMARK_SPIKE_PROBE__ = (why) => {
    const a = document.activeElement;
    log("probe(" + why + ") hasFocus=" + document.hasFocus() + " visibility=" + document.visibilityState +
      " active=" + ((a && a.tagName) || "-") + " size=" + innerWidth + "x" + innerHeight + " title=" + document.title);
  };
})();"#;

struct Config {
    tabs: usize,
    paths: Vec<PathBuf>,
    lazy: bool,
    plan: bool,
    hold: Duration,
}

struct Spike {
    config: Config,
    start: Instant,
    log: Mutex<Option<File>>,
    active: AtomicUsize,
    created: [AtomicBool; MAX_TABS],
    loads: AtomicUsize,
    /// Held by background threads only (never the main thread): serializes lazy creation.
    creating: Mutex<()>,
}

static SPIKE: OnceLock<Option<Spike>> = OnceLock::new();

fn spike() -> Option<&'static Spike> {
    SPIKE
        .get_or_init(|| {
            let tabs = std::env::var("PERMARK_TABS_SPIKE").ok()?;
            let tabs = tabs.trim().parse::<usize>().unwrap_or(1).min(MAX_TABS);
            let flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1");
            let paths = std::env::var("PERMARK_TABS_SPIKE_PATHS")
                .map(|v| {
                    v.split(';')
                        .filter(|p| !p.trim().is_empty())
                        .map(|p| PathBuf::from(p.trim()))
                        .collect()
                })
                .unwrap_or_default();
            let hold = std::env::var("PERMARK_TABS_SPIKE_HOLD_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(6000);
            let log = std::env::var("PERMARK_TABS_SPIKE_LOG")
                .ok()
                .and_then(|p| File::create(p).ok());
            Some(Spike {
                config: Config {
                    tabs,
                    paths,
                    lazy: flag("PERMARK_TABS_SPIKE_LAZY"),
                    plan: flag("PERMARK_TABS_SPIKE_PLAN"),
                    hold: Duration::from_millis(hold),
                },
                start: Instant::now(),
                log: Mutex::new(log),
                active: AtomicUsize::new(0),
                created: Default::default(),
                loads: AtomicUsize::new(0),
                creating: Mutex::new(()),
            })
        })
        .as_ref()
}

/// Spike mode on (env set). Call first thing in `run` (starts the clock).
pub fn active() -> bool {
    spike().is_some()
}

/// Spike mode: isolated profile + shared webview cache under the temp folder.
pub fn dirs() -> Option<AppDirs> {
    spike()?;
    let base = std::env::temp_dir().join("permark-tabs-spike");
    Some(AppDirs {
        profile: base.join(format!("profile-{}", std::process::id())),
        cache: base.join("cache"),
    })
}

pub fn log(message: impl Display) {
    let Some(spike) = spike() else {
        return;
    };
    let line = format!("{} {message}\n", spike.start.elapsed().as_millis());
    let mut file = spike.log.lock().unwrap_or_else(PoisonError::into_inner);
    match file.as_mut() {
        Some(file) => {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
        None => eprint!("{line}"),
    }
}

fn tab_label(index: usize) -> String {
    format!("{WINDOW}-t{}", index + 1)
}

fn tab_index(label: &str) -> Option<usize> {
    label
        .strip_prefix(WINDOW)?
        .strip_prefix("-t")?
        .parse::<usize>()
        .ok()?
        .checked_sub(1)
}

/// Setup hook. `true` = the spike made the window (tabs >= 1): skip `main` + first launch.
///
/// # Errors
/// Window | webview creation failure.
pub fn setup(handle: &AppHandle) -> tauri::Result<bool> {
    let Some(spike) = spike() else {
        return Ok(false);
    };
    log(format!(
        "setup tabs={} lazy={} plan={} paths={} pid={}",
        spike.config.tabs,
        spike.config.lazy,
        spike.config.plan,
        spike.config.paths.len(),
        std::process::id()
    ));
    if spike.config.plan {
        let handle = handle.clone();
        std::thread::spawn(move || plan(&handle));
    }
    if spike.config.tabs == 0 {
        return Ok(false);
    }
    let t0 = Instant::now();
    let window = WindowBuilder::new(handle, WINDOW)
        .title("PerMark tabs spike")
        .inner_size(1280.0, 800.0)
        .min_inner_size(900.0, 600.0)
        .visible(false)
        .build()?;
    log(format!("window created in {}us", t0.elapsed().as_micros()));
    let count = if spike.config.lazy {
        1
    } else {
        spike.config.tabs
    };
    for index in 0..count {
        let webview = create_tab(handle, &window, index)?;
        if index != 0 {
            let _ = webview.hide();
        }
    }
    let _ = window.set_title("PerMark spike - tab 1");
    Ok(true)
}

fn create_tab(handle: &AppHandle, window: &Window, index: usize) -> tauri::Result<Webview> {
    let t0 = Instant::now();
    let label = tab_label(index);
    let size = window.inner_size()?;
    let builder = WebviewBuilder::new(&label, WebviewUrl::App("index.html".into()))
        .initialization_script(INIT);
    #[cfg(not(target_os = "macos"))]
    let builder = builder.data_directory(state(handle).env.dirs.cache.join("webview"));
    let webview = window.add_child(builder, PhysicalPosition::new(0, 0), size)?;
    if let Some(spike) = spike() {
        spike.created[index].store(true, Ordering::Release);
    }
    let shell = state(handle);
    shell.registry.add_window(&label);
    let drop_label = label.clone();
    webview.on_webview_event(move |event| {
        if let WebviewEvent::DragDrop(drop) = event {
            log(format!("dragdrop {drop_label} {drop:?}"));
        }
    });
    log(format!(
        "tab {label} created in {}us ({}x{})",
        t0.elapsed().as_micros(),
        size.width,
        size.height
    ));
    let path = spike().and_then(|s| {
        (!s.config.paths.is_empty()).then(|| s.config.paths[index % s.config.paths.len()].clone())
    });
    if let Some(path) = path {
        let (handle, label) = (handle.clone(), label.clone());
        std::thread::spawn(move || match WorkspaceRef::classify(&path) {
            Ok(reference) => {
                let opened = crate::open::open_in(&handle, &label, &reference, None, true);
                log(format!(
                    "open {label} {} -> {:?}",
                    path.display(),
                    opened.map(|id| format!("{id:?}"))
                ));
            }
            Err(error) => log(format!("open {label} {} failed {error:?}", path.display())),
        });
    }
    Ok(webview)
}

/// Activates tab `to` (creates it when lazy + new): hide old, show new, focus new, window title. Any thread except
/// main (lazy creation waits on the main thread).
pub fn activate(handle: &AppHandle, to: usize) {
    let Some(spike) = spike() else {
        return;
    };
    if to >= spike.config.tabs {
        log(format!(
            "activate {to} ignored (tabs={})",
            spike.config.tabs
        ));
        return;
    }
    let Some(window) = handle.get_window(WINDOW) else {
        return;
    };
    let t0 = Instant::now();
    let from = spike.active.swap(to, Ordering::AcqRel);
    let new = if spike.created[to].load(Ordering::Acquire) {
        handle.get_webview(&tab_label(to))
    } else {
        let _guard = spike
            .creating
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match handle.get_webview(&tab_label(to)) {
            Some(existing) => Some(existing),
            None => create_tab(handle, &window, to)
                .map_err(|e| log(format!("create {to} failed {e}")))
                .ok(),
        }
    };
    let Some(new) = new else {
        return;
    };
    let created = t0.elapsed();
    // hide first: on Linux the tabs share a GtkBox, so two visible tabs would split the height for a moment
    let hidden = if from == to {
        Ok(())
    } else {
        handle
            .get_webview(&tab_label(from))
            .map_or(Ok(()), |old| old.hide())
    };
    let shown = new.show();
    let focused = new.set_focus();
    let titled = window.set_title(&format!("PerMark spike - tab {}", to + 1));
    log(format!(
        "activate {} -> {} total={}us (create/lookup {}us) show={shown:?} hide={hidden:?} focus={focused:?} title={titled:?}",
        from + 1,
        to + 1,
        t0.elapsed().as_micros(),
        created.as_micros()
    ));
    let handle = handle.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        probe(&handle, "after-activate");
    });
}

/// Every tab logs focus + visibility + size; the native window logs title + focus.
fn probe(handle: &AppHandle, why: &str) {
    if let Some(window) = handle.get_window(WINDOW) {
        log(format!(
            "window title={:?} focused={:?} inner={:?}",
            window.title().ok(),
            window.is_focused().ok(),
            window.inner_size().ok().map(|s| (s.width, s.height))
        ));
        for webview in window.webviews() {
            let bounds = webview.bounds().ok().map(|r| format!("{r:?}"));
            log(format!("bounds {} {bounds:?}", webview.label()));
            let _ = webview.eval(format!(
                "window.__PERMARK_SPIKE_PROBE__ && window.__PERMARK_SPIKE_PROBE__({why:?})"
            ));
        }
    }
}

/// Window event hook (main thread): resize -> every tab fills the window.
pub fn on_window_event(handle: &AppHandle, label: &str, event: &WindowEvent) {
    if label != WINDOW {
        return;
    }
    match event {
        WindowEvent::Resized(size) => {
            let Some(window) = handle.get_window(WINDOW) else {
                return;
            };
            let t0 = Instant::now();
            let webviews = window.webviews();
            for webview in &webviews {
                let _ = webview.set_bounds(Rect {
                    position: PhysicalPosition::new(0, 0).into(),
                    size: PhysicalSize::new(size.width, size.height).into(),
                });
            }
            log(format!(
                "resized {}x{} -> {} tabs in {}us",
                size.width,
                size.height,
                webviews.len(),
                t0.elapsed().as_micros()
            ));
        }
        WindowEvent::Focused(focused) => log(format!("window focused={focused}")),
        WindowEvent::DragDrop(drop) => log(format!("window dragdrop {drop:?}")),
        WindowEvent::Destroyed => {
            let shell = state(handle);
            for index in 0..MAX_TABS {
                shell.registry.forget_window(&tab_label(index));
            }
        }
        _ => {}
    }
}

/// Global page-load hook (every webview, spike mode only): cold start marks.
pub fn page_load(webview: &Webview, payload: &PageLoadPayload<'_>) {
    let Some(spike) = spike() else {
        return;
    };
    match payload.event() {
        PageLoadEvent::Started => log(format!("page-load started {}", webview.label())),
        PageLoadEvent::Finished => {
            let loads = spike.loads.fetch_add(1, Ordering::AcqRel) + 1;
            log(format!(
                "page-load finished {} (loads={loads})",
                webview.label()
            ));
        }
    }
}

/// Scripted run (own thread): wait loads -> READY -> cycle -> CYCLED -> resize -> DONE -> exit.
fn plan(handle: &AppHandle) {
    let Some(spike) = spike() else {
        return;
    };
    let expected = if spike.config.lazy || spike.config.tabs == 0 {
        1
    } else {
        spike.config.tabs
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while spike.loads.load(Ordering::Acquire) < expected && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    log(format!(
        "loaded {} of {expected}",
        spike.loads.load(Ordering::Acquire)
    ));
    // let the frontends boot + workspaces open
    std::thread::sleep(Duration::from_millis(2500));
    // the frontend shows its window from requestAnimationFrame; a hidden native window may never paint
    let window = handle
        .get_window(WINDOW)
        .or_else(|| handle.get_window(crate::windows::MAIN));
    if let Some(window) = window {
        let visible = window.is_visible().ok();
        log(format!("window {} visible={visible:?}", window.label()));
        if visible == Some(false) {
            log("WARN window still hidden after load: shown from Rust");
            let _ = window.show();
            let _ = window.set_focus();
            std::thread::sleep(Duration::from_millis(1500));
        }
    }
    probe(handle, "ready");
    log("MARK READY");
    std::thread::sleep(spike.config.hold);
    if spike.config.tabs >= 2 {
        for round in 0..2 {
            for index in (0..spike.config.tabs).chain(std::iter::once(0)) {
                activate(handle, index);
                std::thread::sleep(Duration::from_millis(if round == 0 { 1500 } else { 300 }));
            }
        }
        std::thread::sleep(Duration::from_millis(2000));
        log("MARK CYCLED");
        std::thread::sleep(spike.config.hold);
    }
    if let Some(window) = handle.get_window(WINDOW) {
        let _ = window.set_size(PhysicalSize::new(1000, 700));
        std::thread::sleep(Duration::from_millis(800));
        probe(handle, "after-resize-small");
        let _ = window.maximize();
        std::thread::sleep(Duration::from_millis(800));
        probe(handle, "after-maximize");
        let _ = window.unmaximize();
        std::thread::sleep(Duration::from_millis(800));
    }
    log("MARK DONE");
    std::thread::sleep(Duration::from_millis(500));
    handle.exit(0);
}

#[tauri::command(async)]
/// `to` = "next" | "prev" | "1".."9".
pub fn spike_tab(handle: AppHandle, webview: Webview, to: String) {
    let Some(spike) = spike() else {
        return;
    };
    let tabs = spike.config.tabs.max(1);
    let current = spike.active.load(Ordering::Acquire);
    let target = match to.as_str() {
        "next" => (current + 1) % tabs,
        "prev" => (current + tabs - 1) % tabs,
        digit => digit.parse::<usize>().unwrap_or(1).saturating_sub(1),
    };
    log(format!(
        "spike_tab from={} (active index {}) to={to}",
        webview.label(),
        current + 1
    ));
    if tab_index(webview.label()) != Some(current) {
        log(format!(
            "WARN key came from a non-active tab {}",
            webview.label()
        ));
    }
    activate(&handle, target);
}

#[tauri::command(async)]
pub fn spike_log(webview: Webview, msg: String) {
    log(format!("js {} {msg}", webview.label()));
}

pub fn handler() -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static {
    tauri::generate_handler![spike_tab, spike_log]
}
