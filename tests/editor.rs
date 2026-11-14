//! Opens the editor in a real window as a CLAP host does, again and again. Needs a display: on
//! Linux an X server such as Xvfb, else the test is skipped; in CI (`CI` set) it fails instead.
//! Runs on the main thread, without the test harness, as AppKit requires.

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_plugin_gui, clap_window, clap_window_handle};
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;
use clap_sys::version::CLAP_VERSION;
use plugin_format::{Block, GuiContext, Info, ParamInfo, Plugin, Shared, egui};

static FRAMES: AtomicUsize = AtomicUsize::new(0);
/// What the last frame measured, `width << 32 | height` at a scale of 1.0.
static DRAWN: AtomicU64 = AtomicU64::new(0);

struct Counting;

impl Shared for Counting {
    fn params(&self) -> &'static [ParamInfo] {
        &[]
    }
    fn param(&self, _: u32) -> f64 {
        0.0
    }
    fn set_param(&self, _: u32, _: f64) {}
    fn param_text(&self, _: u32, _: f64) -> String {
        String::new()
    }
    fn param_from_text(&self, _: u32, _: &str) -> Option<f64> {
        None
    }
    fn save(&self) -> Vec<u8> {
        Vec::new()
    }
    fn load(&self, _: &[u8]) -> bool {
        true
    }
    fn editor(&self, _: GuiContext) -> Box<dyn FnMut(&mut egui::Ui) + Send> {
        Box::new(|ui| {
            let size = ui.ctx().content_rect().size();
            DRAWN.store((size.x.round() as u64) << 32 | size.y.round() as u64, Ordering::SeqCst);
            ui.label("Counting");
            FRAMES.fetch_add(1, Ordering::SeqCst);
        })
    }
}

struct Resizable;

impl Plugin for Resizable {
    const INFO: Info = Info {
        name: "Resizable",
        vendor: "test",
        version: "0",
        clap_id: "test.resizable",
        description: "",
        clap_features: &[c"audio-effect"],
        vst3_class_id: [0; 16],
        vst3_subcategories: "Fx",
        sidechain: false,
    };
    const EDITOR_SIZE: (u32, u32) = (400, 300);
    const RESIZABLE: bool = true;
    const EDITOR_MIN: (u32, u32) = (200, 150);
    type Shared = Counting;
    fn new() -> (Self, Arc<Counting>) {
        (Resizable, Arc::new(Counting))
    }
    fn activate(&mut self, _: f64, _: usize, _: usize) -> u32 {
        0
    }
    fn deactivate(&mut self) {}
    fn latency(&self) -> u32 {
        0
    }
    fn process(&mut self, _: Block<'_>) {}
    fn take_latency_changed(&mut self) -> bool {
        false
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt, CreateWindowAux, WindowClass};
    use x11rb::rust_connection::RustConnection;

    use super::*;

    pub const API: &std::ffi::CStr = clap_sys::ext::gui::CLAP_WINDOW_API_X11;

    /// The host's window the editor is embedded in.
    pub struct Parent {
        connection: RustConnection,
        id: u32,
    }

    impl Parent {
        /// `None` without a display.
        pub fn new() -> Option<Self> {
            let (connection, screen) = x11rb::connect(None).ok()?;
            let root = connection.setup().roots[screen].root;
            let id = connection.generate_id().ok()?;
            connection
                .create_window(x11rb::COPY_DEPTH_FROM_PARENT, id, root, 0, 0, 1200, 800, 0, WindowClass::INPUT_OUTPUT, 0, &CreateWindowAux::new())
                .ok()?;
            connection.map_window(id).ok()?;
            connection.flush().ok()?;
            Some(Parent { connection, id })
        }

        pub fn handle(&self) -> clap_window_handle {
            clap_window_handle { x11: self.id as std::ffi::c_ulong }
        }
    }

    impl Drop for Parent {
        fn drop(&mut self) {
            let _ = self.connection.destroy_window(self.id);
            let _ = self.connection.flush();
        }
    }

    /// The window runs on a thread of its own.
    pub fn pump() {}

    pub fn threads() -> usize {
        std::fs::read_dir("/proc/self/task").unwrap().count()
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    use super::*;

    pub const API: &std::ffi::CStr = clap_sys::ext::gui::CLAP_WINDOW_API_WIN32;

    /// The host's window the editor is embedded in.
    pub struct Parent(*mut std::ffi::c_void);

    impl Parent {
        /// `None` without a display.
        pub fn new() -> Option<Self> {
            let class = windows_sys::w!("STATIC");
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class,
                    ptr::null(),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    0,
                    0,
                    1200,
                    800,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                )
            };
            (!hwnd.is_null()).then_some(Parent(hwnd))
        }

        pub fn handle(&self) -> clap_window_handle {
            clap_window_handle { win32: self.0 }
        }
    }

    impl Drop for Parent {
        fn drop(&mut self) {
            unsafe { DestroyWindow(self.0) };
        }
    }

    /// The window runs on this thread, as on the host's UI thread.
    pub fn pump() {
        unsafe {
            let mut message: MSG = std::mem::zeroed();
            while PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    pub fn threads() -> usize {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            assert_ne!(snapshot, INVALID_HANDLE_VALUE);
            let process = GetCurrentProcessId();
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            let mut count = 0;
            let mut more = Thread32First(snapshot, &mut entry) != 0;
            while more {
                if entry.th32OwnerProcessID == process {
                    count += 1;
                }
                more = Thread32Next(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
            count
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEventMask, NSWindow, NSWindowStyleMask};
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize};

    use super::*;

    pub const API: &std::ffi::CStr = clap_sys::ext::gui::CLAP_WINDOW_API_COCOA;

    /// The host's window the editor is embedded in. On screen, though without a border or a place in
    /// the Dock: AppKit draws a view only in a window that is.
    pub struct Parent(Retained<NSWindow>);

    impl Parent {
        /// `None` off the main thread, where AppKit opens no window.
        pub fn new() -> Option<Self> {
            let mtm = MainThreadMarker::new()?;
            let app = NSApplication::sharedApplication(mtm);
            app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            app.finishLaunching();
            let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1200.0, 800.0));
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    rect,
                    NSWindowStyleMask::Borderless,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            unsafe { window.setReleasedWhenClosed(false) };
            window.orderFrontRegardless();
            Some(Parent(window))
        }

        pub fn handle(&self) -> clap_window_handle {
            let view = self.0.contentView().expect("a content view");
            clap_window_handle { cocoa: Retained::as_ptr(&view) as *mut std::ffi::c_void }
        }
    }

    impl Drop for Parent {
        fn drop(&mut self) {
            self.0.close();
        }
    }

    /// The window runs on this thread, as on the host's main thread. Waits a moment for the first
    /// event, so the run loop also gets to draw what needs drawing before it would wait.
    pub fn pump() {
        let app = NSApplication::sharedApplication(MainThreadMarker::new().unwrap());
        let mut until = NSDate::dateWithTimeIntervalSinceNow(0.001);
        while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(NSEventMask::Any, Some(&until), unsafe { NSDefaultRunLoopMode }, true) {
            app.sendEvent(&event);
            until = NSDate::distantPast();
        }
    }
}

/// Lets the window run until `done`, or fails after a while.
fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "no {what} in time");
        platform::pump();
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn drawn() -> (u32, u32) {
    let drawn = DRAWN.load(Ordering::SeqCst);
    ((drawn >> 32) as u32, drawn as u32)
}

/// Opens the editor, lets it draw, resizes it as a host would and closes it again.
unsafe fn open_and_close(plugin: *const clap_plugin, gui: &clap_plugin_gui, parent: &platform::Parent, width: u32) {
    unsafe {
        assert!((gui.create.unwrap())(plugin, platform::API.as_ptr(), false));
        let window = clap_window { api: platform::API.as_ptr(), specific: parent.handle() };
        assert!((gui.set_parent.unwrap())(plugin, &window));
        assert!((gui.show.unwrap())(plugin));
        let frames = FRAMES.load(Ordering::SeqCst);
        wait_for("frame", || FRAMES.load(Ordering::SeqCst) > frames);
        assert!((gui.set_size.unwrap())(plugin, width, 200));
        wait_for("resize", || drawn() == (width, 200));
        (gui.destroy.unwrap())(plugin);
    }
}

fn main() {
    editor_opens_and_closes_fifty_times();
}

/// The thread count once it holds still: a closed window's thread may still be on its way out.
#[cfg(not(target_os = "macos"))]
fn settled() -> usize {
    let mut count = platform::threads();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        platform::pump();
        let now = platform::threads();
        if now == count {
            return count;
        }
        count = now;
    }
}

fn editor_opens_and_closes_fifty_times() {
    let Some(parent) = platform::Parent::new() else {
        assert!(std::env::var_os("CI").is_none(), "no display to open the editor on");
        eprintln!("skipped: no display to open the editor on");
        return;
    };
    let host = clap_host {
        clap_version: CLAP_VERSION,
        host_data: ptr::null_mut(),
        name: c"test".as_ptr(),
        vendor: c"".as_ptr(),
        url: c"".as_ptr(),
        version: c"".as_ptr(),
        get_extension: None,
        request_restart: None,
        request_process: None,
        request_callback: None,
    };
    let entry = plugin_format::clap::entry::<Resizable>();
    unsafe {
        assert!((entry.init.unwrap())(c"".as_ptr()));
        let factory = (entry.get_factory.unwrap())(CLAP_PLUGIN_FACTORY_ID.as_ptr()) as *const clap_plugin_factory;
        let plugin = ((*factory).create_plugin.unwrap())(factory, &host, c"test.resizable".as_ptr());
        assert!(!plugin.is_null());
        assert!(((*plugin).init.unwrap())(plugin));
        let gui = &*(((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_GUI.as_ptr()) as *const clap_plugin_gui);

        // Whatever the first window starts once for the whole process, e.g. in the GL driver.
        open_and_close(plugin, gui, &parent, 300);
        #[cfg(not(target_os = "macos"))]
        let before = settled();
        for round in 0..50 {
            open_and_close(plugin, gui, &parent, 300 + round);
        }
        // On macOS the window runs on this thread, and the system's worker threads come and go.
        #[cfg(not(target_os = "macos"))]
        {
            wait_for("thread exit", || platform::threads() <= before);
            assert_eq!(platform::threads(), before);
        }

        ((*plugin).destroy.unwrap())(plugin);
        (entry.deinit.unwrap())();
    }
    println!("test editor_opens_and_closes_fifty_times ... ok");
}
