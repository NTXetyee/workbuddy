//! Minimize-to-tray for the console host (T26).
//!
//! While `wb-host` runs in its classic console window, minimizing that window hides it and leaves
//! a tray icon in its place. The icon restores the window on click and offers "Show window" and
//! "Quit WorkBuddy host" in its menu. Quit sets the same stop flag that Ctrl+C sets.
//!
//! Only a classic console window can be hidden. Windows Terminal hosts consoles in a pseudo-window
//! that `GetConsoleWindow` reports but that cannot be hidden, so `start` returns `None` there. The
//! Desktop shortcut starts the launcher through `conhost.exe` to force the classic window.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Console::GetConsoleWindow;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    ExtractIconExW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, IsIconic, IsWindowVisible,
    KillTimer, LoadIconW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SetTimer, ShowWindow, TrackPopupMenu, HICON, IDI_APPLICATION, MF_STRING,
    MSG, SW_HIDE, SW_MINIMIZE, SW_RESTORE, SW_SHOW, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_APP, WM_CLOSE, WM_COMMAND, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER,
    WNDCLASSW,
};

/// Callback message the shell sends for mouse events on our icon.
const TRAY_MSG: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;
const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 250;
const CMD_SHOW: usize = 1;
const CMD_QUIT: usize = 2;
/// Tooltip capacity in UTF-16 units, not counting the terminating zero.
const TIP_MAX: usize = 127;
/// Stock icon in imageres.dll (a monitor-style icon on Windows 10 and 11).
const TRAY_ICON_INDEX: i32 = 109;
const READY_WAIT: Duration = Duration::from_secs(5);
const SELFTEST_STEP: Duration = Duration::from_millis(50);
const SELFTEST_WAIT: Duration = Duration::from_secs(2);

/// Message id of the broadcast Explorer sends when it recreates the taskbar (0 until registered).
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// State of the tray thread. Only that thread reads or writes it.
struct Tray {
    hwnd: HWND,
    console: HWND,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    /// Tooltip the icon currently shows.
    tip: String,
    hicon: HICON,
    /// `hicon` was extracted from imageres.dll and must be destroyed; a stock icon must not be.
    owns_icon: bool,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

/// Keeps the tray icon alive. Dropping it removes the icon and waits for the tray thread to end.
pub struct TrayHandle {
    hwnd: usize,
    thread: Option<JoinHandle<()>>,
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        // SAFETY: posting to a window handle is harmless even when that window is already gone.
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut c_void)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts the tray icon. Returns `None` when the process has no classic console window (Windows
/// Terminal, redirected output) or when the icon cannot be added. `--no-tray` is handled by the
/// caller, which does not call this.
///
/// `stop` is the process stop flag: the menu's Quit sets it, and the tray stops when it is set.
/// `status` is the one-line status text shown as the icon tooltip.
pub fn start(stop: Arc<AtomicBool>, status: Arc<Mutex<String>>) -> Option<TrayHandle> {
    // SAFETY: GetConsoleWindow takes no arguments and only reads process state.
    if unsafe { GetConsoleWindow() }.0.is_null() {
        return None;
    }
    let (ready_tx, ready_rx) = mpsc::channel::<Option<usize>>();
    let thread = thread::Builder::new()
        .name("wb-tray".into())
        .spawn(move || tray_thread(stop, status, ready_tx))
        .ok()?;
    match ready_rx.recv_timeout(READY_WAIT) {
        Ok(Some(hwnd)) => Some(TrayHandle {
            hwnd,
            thread: Some(thread),
        }),
        _ => None,
    }
}

/// Shows the console window again and brings it to the front.
pub fn restore_console() {
    let console = console_window();
    if console.0.is_null() {
        return;
    }
    // SAFETY: plain Win32 window calls on this process's own console window.
    unsafe {
        let _ = ShowWindow(console, SW_SHOW);
        let _ = ShowWindow(console, SW_RESTORE);
        let _ = SetForegroundWindow(console);
    }
}

/// Hides the console window. The tray icon is then the only way back.
pub fn hide_console() {
    let console = console_window();
    if console.0.is_null() {
        return;
    }
    // SAFETY: plain Win32 window call on this process's own console window.
    unsafe {
        let _ = ShowWindow(console, SW_HIDE);
    }
}

fn console_window() -> HWND {
    // SAFETY: GetConsoleWindow takes no arguments and only reads process state.
    unsafe { GetConsoleWindow() }
}

fn is_visible(hwnd: HWND) -> bool {
    // SAFETY: IsWindowVisible only reads window state.
    unsafe { IsWindowVisible(hwnd) }.as_bool()
}

fn is_iconic(hwnd: HWND) -> bool {
    // SAFETY: IsIconic only reads window state.
    unsafe { IsIconic(hwnd) }.as_bool()
}

/// Thread body: creates the hidden window and icon, reports readiness, pumps messages until the
/// window is closed, then removes the icon.
fn tray_thread(
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    ready: mpsc::Sender<Option<usize>>,
) {
    match create(stop, status) {
        Some(hwnd) => {
            let _ = ready.send(Some(hwnd.0 as usize));
            pump();
            teardown();
        }
        None => {
            let _ = ready.send(None);
        }
    }
}

/// Registers the window class, creates the hidden window, adds the icon and starts the timer.
fn create(stop: Arc<AtomicBool>, status: Arc<Mutex<String>>) -> Option<HWND> {
    let console = console_window();
    if console.0.is_null() {
        return None;
    }
    let class = w!("WorkBuddyTrayWindow");
    // SAFETY: every call below is a plain Win32 call on handles this thread creates and owns.
    unsafe {
        let module = GetModuleHandleW(None).ok()?;
        let instance = HINSTANCE(module.0);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance,
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return None;
        }
        // A normal hidden top-level window: Shell_NotifyIcon does not work with HWND_MESSAGE.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("WorkBuddy tray"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .ok()?;
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), SeqCst);

        let (hicon, owns_icon) = load_icon();
        let tip = tooltip(&status);
        let nid = notify_data(hwnd, hicon, &tip);
        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            if owns_icon {
                let _ = DestroyIcon(hicon);
            }
            let _ = DestroyWindow(hwnd);
            return None;
        }
        SetTimer(Some(hwnd), TIMER_ID, TIMER_MS, None);
        TRAY.with(|t| {
            *t.borrow_mut() = Some(Tray {
                hwnd,
                console,
                stop,
                status,
                tip,
                hicon,
                owns_icon,
            });
        });
        Some(hwnd)
    }
}

/// Stock imageres.dll icon 109, falling back to the stock application icon.
fn load_icon() -> (HICON, bool) {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let path: Vec<u16> = format!("{root}\\System32\\imageres.dll")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut icon = HICON(std::ptr::null_mut());
    // SAFETY: `path` is NUL-terminated and outlives the call; `icon` is a valid out-slot.
    unsafe {
        ExtractIconExW(
            PCWSTR(path.as_ptr()),
            TRAY_ICON_INDEX,
            Some(&mut icon as *mut HICON),
            None,
            1,
        );
    }
    if !icon.0.is_null() {
        return (icon, true);
    }
    // SAFETY: IDI_APPLICATION is a stock icon id; no instance handle is needed.
    let stock = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or(HICON(std::ptr::null_mut()));
    (stock, false)
}

/// The icon tooltip: the current status text, or a plain name when there is none.
fn tooltip(status: &Mutex<String>) -> String {
    let text = status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if text.trim().is_empty() {
        "WorkBuddy host".to_string()
    } else {
        text
    }
}

fn notify_data(hwnd: HWND, hicon: HICON, tip: &str) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: TRAY_MSG,
        hIcon: hicon,
        ..Default::default()
    };
    for (slot, unit) in nid.szTip.iter_mut().take(TIP_MAX).zip(tip.encode_utf16()) {
        *slot = unit;
    }
    nid
}

fn pump() {
    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a valid out-pointer; this thread owns the window it dispatches to.
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        if result <= 0 {
            break;
        }
        // SAFETY: `msg` was just filled in by GetMessageW.
        unsafe {
            DispatchMessageW(&msg);
        }
    }
}

/// Removes the icon, makes sure the console is visible, and destroys the window.
fn teardown() {
    let Some(state) = TRAY.with(|t| t.borrow_mut().take()) else {
        return;
    };
    // SAFETY: all handles belong to this thread and are still valid here.
    unsafe {
        let nid = notify_data(state.hwnd, state.hicon, &state.tip);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        let _ = KillTimer(Some(state.hwnd), TIMER_ID);
        // The process is about to end: never leave it running with no window and no icon.
        if !is_visible(state.console) || is_iconic(state.console) {
            restore_console();
        }
        let _ = DestroyWindow(state.hwnd);
        if state.owns_icon {
            let _ = DestroyIcon(state.hicon);
        }
    }
}

/// Window procedure. Runs on the tray thread only.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let taskbar = TASKBAR_CREATED.load(SeqCst);
    if taskbar != 0 && msg == taskbar {
        // Explorer restarted and dropped the icon: add it again.
        readd_icon();
        return LRESULT(0);
    }
    match msg {
        WM_TIMER => {
            on_timer(wparam.0);
            LRESULT(0)
        }
        TRAY_MSG => {
            on_tray_event((lparam.0 as u32) & 0xFFFF);
            LRESULT(0)
        }
        WM_COMMAND => {
            on_command(wparam.0 & 0xFFFF);
            LRESULT(0)
        }
        WM_CLOSE => {
            // Ends the message loop; `teardown` then removes the icon.
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// `(window, console, stop flag, status)`: see [`snapshot`].
type Snapshot = (HWND, HWND, Arc<AtomicBool>, Arc<Mutex<String>>);

/// Snapshot of the state a handler needs, copied out so no `RefCell` borrow is held while Win32
/// calls can pump messages back into this thread.
fn snapshot() -> Option<Snapshot> {
    TRAY.with(|t| {
        t.borrow().as_ref().map(|s| {
            (
                s.hwnd,
                s.console,
                Arc::clone(&s.stop),
                Arc::clone(&s.status),
            )
        })
    })
}

fn on_timer(id: usize) {
    if id != TIMER_ID {
        return;
    }
    let Some((hwnd, console, stop, status)) = snapshot() else {
        return;
    };
    if stop.load(SeqCst) {
        // SAFETY: plain message-loop call on this thread.
        unsafe { PostQuitMessage(0) };
        return;
    }
    // A minimized console is hidden: the tray icon is its place from now on.
    if is_iconic(console) {
        // SAFETY: plain Win32 window call on this process's own console window.
        unsafe {
            let _ = ShowWindow(console, SW_HIDE);
        }
    }
    let text = tooltip(&status);
    let current = TRAY.with(|t| t.borrow().as_ref().map(|s| s.tip.clone()));
    if current.as_deref() == Some(text.as_str()) {
        return;
    }
    let hicon = TRAY.with(|t| {
        let mut slot = t.borrow_mut();
        slot.as_mut().map(|s| {
            s.tip = text.clone();
            s.hicon
        })
    });
    if let Some(hicon) = hicon {
        let nid = notify_data(hwnd, hicon, &text);
        // SAFETY: `nid` is fully initialized and the icon id matches the one added at start.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }
}

fn readd_icon() {
    let Some(hwnd) = TRAY.with(|t| t.borrow().as_ref().map(|s| s.hwnd)) else {
        return;
    };
    let (hicon, tip) = TRAY
        .with(|t| t.borrow().as_ref().map(|s| (s.hicon, s.tip.clone())))
        .unwrap_or((HICON(std::ptr::null_mut()), String::new()));
    let nid = notify_data(hwnd, hicon, &tip);
    // SAFETY: `nid` is fully initialized.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

fn on_tray_event(event: u32) {
    match event {
        WM_LBUTTONUP | WM_LBUTTONDBLCLK => restore_console(),
        WM_RBUTTONUP => show_menu(),
        _ => {}
    }
}

fn show_menu() {
    let Some((hwnd, _, _, _)) = snapshot() else {
        return;
    };
    // SAFETY: menu and window handles belong to this thread; the menu is destroyed below.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = AppendMenuW(menu, MF_STRING, CMD_SHOW, w!("Show window"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, w!("Quit WorkBuddy host"));
        // Required so that the menu dismisses when the user clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

fn on_command(id: usize) {
    match id {
        CMD_SHOW => restore_console(),
        CMD_QUIT => {
            if let Some((hwnd, _, stop, _)) = snapshot() {
                stop.store(true, SeqCst);
                // SAFETY: posting to our own window; WM_CLOSE ends the message loop.
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
        _ => {}
    }
}

/// `--tray-selftest <seconds>`: starts the tray, minimizes the console window and checks that the
/// tray hid it; then restores it and checks that. Prints PASS/FAIL lines and returns the exit code.
/// Touches no USB device, capture or encoder.
pub fn selftest(seconds: u64) -> i32 {
    let started = Instant::now();
    let budget = Duration::from_secs(seconds);
    let stop = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new("WorkBuddy: tray selftest".to_string()));
    let Some(handle) = start(Arc::clone(&stop), Arc::clone(&status)) else {
        println!("FAIL tray (needs a classic console window and a tray icon)");
        println!("SELFTEST DONE");
        return 1;
    };
    thread::sleep(Duration::from_secs(1).min(budget));
    let console = console_window();
    // SAFETY: plain Win32 window call on this process's own console window.
    unsafe {
        let _ = ShowWindow(console, SW_MINIMIZE);
    }
    let hidden = wait_for(started, budget, || !is_visible(console));
    report("hidden", hidden);
    restore_console();
    let restored = wait_for(started, budget, || {
        is_visible(console) && !is_iconic(console)
    });
    report("restored", restored);
    drop(handle);
    println!("SELFTEST DONE");
    if hidden && restored {
        0
    } else {
        1
    }
}

fn report(name: &str, ok: bool) {
    println!("{} {name}", if ok { "PASS" } else { "FAIL" });
}

/// Polls `cond` until it holds, for up to 2 s and never past the selftest budget.
fn wait_for(started: Instant, budget: Duration, cond: impl Fn() -> bool) -> bool {
    let deadline = (Instant::now() + SELFTEST_WAIT).min(started + budget);
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(SELFTEST_STEP);
    }
}
