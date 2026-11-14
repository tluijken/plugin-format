use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::panic::{AssertUnwindSafe, catch_unwind};

use egui_baseview::baseview;
use egui_baseview::baseview::dpi::NativeSize;
use egui_baseview::{App, EguiWindow, EguiWindowSettings, Frame};
use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, WindowHandle};

/// The host's window to embed the editor in. Only the platform's own kind is accepted.
pub enum Parent {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    X11(std::ffi::c_ulong),
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Win32(*mut c_void),
}

/// The open editor, closed when dropped. Not `Send`, so it is closed on the thread that opened it,
/// which Windows requires of a window.
pub struct Window {
    window: ManuallyDrop<baseview::Window>,
    #[cfg(target_os = "linux")]
    finished: std::sync::Arc<x11::Finished>,
}

impl Window {
    pub fn show(&self) -> bool {
        self.window.show().is_ok()
    }

    pub fn hide(&self) -> bool {
        self.window.hide().is_ok()
    }
}

impl Drop for Window {
    /// On X11 baseview's drop may return before its window thread is done, detaching it, and the
    /// host may unload the library right after: so wait for the thread too. The panic baseview
    /// passes on from that thread must not reach the host.
    fn drop(&mut self) {
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe { ManuallyDrop::drop(&mut self.window) }));
        #[cfg(target_os = "linux")]
        self.finished.wait(x11::CLOSE_TIMEOUT);
    }
}

/// `size` is in the platform's pixels, as CLAP and VST3 hosts measure the parent on Linux and Windows.
/// `on_frame` runs before `ui` on the window's thread. `None` when the parent is no window of this
/// platform or the window cannot be opened, e.g. without a display.
pub fn open(parent: Parent, size: (u32, u32), ui: Box<dyn FnMut(&mut egui::Ui) + Send>, on_frame: Box<dyn FnMut() + Send>) -> Option<Window> {
    let parent = Handle(raw(parent)?);
    let settings = EguiWindowSettings::new()
        .with_size(NativeSize::new(size.0, size.1))
        .with_resizable(false)
        .with_parent(&parent);
    #[cfg(target_os = "linux")]
    let finished = std::sync::Arc::new(x11::Finished::default());
    let editor = Editor {
        ui,
        on_frame,
        #[cfg(target_os = "linux")]
        finished: finished.clone(),
    };
    let window = EguiWindow::create(settings, editor).ok()?;
    let window = Window {
        window: ManuallyDrop::new(window),
        #[cfg(target_os = "linux")]
        finished,
    };
    window.show().then_some(window)
}

fn raw(parent: Parent) -> Option<RawWindowHandle> {
    match parent {
        #[cfg(target_os = "linux")]
        Parent::X11(id) => {
            let id = std::num::NonZeroU32::new(u32::try_from(id).ok()?)?;
            Some(raw_window_handle::XcbWindowHandle::new(id).into())
        }
        #[cfg(target_os = "windows")]
        Parent::Win32(hwnd) => Some(raw_window_handle::Win32WindowHandle::new(std::num::NonZeroIsize::new(hwnd as isize)?).into()),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

struct Handle(RawWindowHandle);

impl HasWindowHandle for Handle {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // The host keeps the parent alive for as long as the editor is embedded in it.
        Ok(unsafe { WindowHandle::borrow_raw(self.0) })
    }
}

struct Editor {
    ui: Box<dyn FnMut(&mut egui::Ui) + Send>,
    on_frame: Box<dyn FnMut() + Send>,
    #[cfg(target_os = "linux")]
    finished: std::sync::Arc<x11::Finished>,
}

impl App for Editor {
    /// Runs on baseview's own thread on X11, before anything of the window exists on it.
    fn build(&mut self, _ctx: egui::Context, _frame: &mut Frame) -> Result<(), baseview::HandlerError> {
        #[cfg(target_os = "linux")]
        x11::signal_on_thread_exit(self.finished.clone());
        Ok(())
    }

    /// On Windows this runs inside the window procedure, where a panic would unwind into the host.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut Frame) {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            (self.on_frame)();
            (self.ui)(ui);
        }));
    }
}

#[cfg(target_os = "linux")]
mod x11 {
    use std::cell::RefCell;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

    #[derive(Default)]
    pub struct Finished {
        done: Mutex<bool>,
        changed: Condvar,
    }

    impl Finished {
        fn signal(&self) {
            *self.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
            self.changed.notify_all();
        }

        /// False when `timeout` passed first.
        pub fn wait(&self, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
            while !*done {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else { return false };
                done = self.changed.wait_timeout(done, left).unwrap_or_else(|e| e.into_inner()).0;
            }
            true
        }
    }

    struct Signal(Arc<Finished>);

    impl Drop for Signal {
        fn drop(&mut self) {
            self.0.signal();
        }
    }

    thread_local! {
        static ON_EXIT: RefCell<Vec<Signal>> = const { RefCell::new(Vec::new()) };
    }

    /// Thread-local destructors run after everything the thread's closure owned has been dropped,
    /// and glibc keeps the library loaded until they have run.
    pub fn signal_on_thread_exit(finished: Arc<Finished>) {
        ON_EXIT.with(|signals| signals.borrow_mut().push(Signal(finished)));
    }

    #[cfg(test)]
    mod tests {
        use std::sync::atomic::{AtomicBool, Ordering};

        use super::*;

        struct SlowDrop(Arc<AtomicBool>);

        impl Drop for SlowDrop {
            fn drop(&mut self) {
                std::thread::sleep(Duration::from_millis(100));
                self.0.store(true, Ordering::SeqCst);
            }
        }

        #[test]
        fn the_wait_ends_only_after_the_thread_has_dropped_what_it_owned() {
            let finished = Arc::new(Finished::default());
            let dropped = Arc::new(AtomicBool::new(false));
            let owned = SlowDrop(dropped.clone());
            let signalled = finished.clone();
            let thread = std::thread::spawn(move || {
                signal_on_thread_exit(signalled);
                let _owned = owned;
            });
            assert!(finished.wait(Duration::from_secs(5)));
            assert!(dropped.load(Ordering::SeqCst));
            thread.join().unwrap();
        }

        #[test]
        fn the_wait_gives_up_after_the_timeout() {
            let finished = Finished::default();
            let start = Instant::now();
            assert!(!finished.wait(Duration::from_millis(30)));
            assert!(start.elapsed() >= Duration::from_millis(30));
        }

        #[test]
        fn a_finished_thread_ends_the_wait_at_once() {
            let finished = Arc::new(Finished::default());
            let signalled = finished.clone();
            std::thread::spawn(move || signal_on_thread_exit(signalled)).join().unwrap();
            assert!(finished.wait(Duration::ZERO));
        }
    }
}
