use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use egui_baseview::baseview;
use egui_baseview::baseview::dpi::NativeSize;
use egui_baseview::{App, EguiWindow, EguiWindowSettings, Frame, RepaintNotifier};
use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, WindowHandle};

/// The editor's scale factor, as the host gives it; 1.0 until it does. The scale the system reports
/// to the window is never used, since the host would not know the size that gives the window.
///
/// When the host changes it while the window is open, the window follows on its next frame by
/// resizing itself, and the format asks the host to resize the parent to match: that keeps the open
/// window instead of reopening it in a parent the host may not have resized yet.
#[derive(Clone)]
pub struct Scale(Arc<ScaleInner>);

struct ScaleInner {
    factor: AtomicU64,
    repaint: RepaintNotifier,
}

impl Default for Scale {
    fn default() -> Self {
        Scale(Arc::new(ScaleInner { factor: AtomicU64::new(1f64.to_bits()), repaint: RepaintNotifier::new() }))
    }
}

impl Scale {
    const RANGE: std::ops::RangeInclusive<f64> = 0.25..=8.0;

    /// False, leaving the factor as it was, for one no window could be drawn at.
    pub fn set(&self, factor: f64) -> bool {
        if !Self::RANGE.contains(&factor) {
            return false;
        }
        self.0.factor.store(factor.to_bits(), Ordering::Relaxed);
        self.0.repaint.request_repaint();
        true
    }

    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.factor.load(Ordering::Relaxed))
    }

    /// `size` at this scale, in the platform's pixels.
    pub fn of(&self, size: (u32, u32)) -> (u32, u32) {
        let factor = self.get();
        ((size.0 as f64 * factor).round() as u32, (size.1 as f64 * factor).round() as u32)
    }
}

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

/// `size` is the editor's size at a scale of 1.0, in the platform's pixels, as CLAP and VST3 hosts
/// measure the parent on Linux and Windows. `on_frame` runs before `ui` on the window's thread.
/// `None` when the parent is no window of this platform or the window cannot be opened, e.g.
/// without a display.
pub fn open(parent: Parent, size: (u32, u32), scale: Scale, ui: Box<dyn FnMut(&mut egui::Ui) + Send>, on_frame: Box<dyn FnMut() + Send>) -> Option<Window> {
    let parent = Handle(raw(parent)?);
    let (width, height) = scale.of(size);
    let settings = EguiWindowSettings::new()
        .with_size(NativeSize::new(width, height))
        .with_resizable(false)
        .with_repaint_notifier(scale.0.repaint.clone())
        .with_parent(&parent);
    #[cfg(target_os = "linux")]
    let finished = std::sync::Arc::new(x11::Finished::default());
    let editor = Editor {
        ui,
        on_frame,
        fit: Fit { size, scale, applied: settings.zoom_factor },
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
    fit: Fit,
    #[cfg(target_os = "linux")]
    finished: std::sync::Arc<x11::Finished>,
}

impl App for Editor {
    /// Runs on baseview's own thread on X11, before anything of the window exists on it.
    fn build(&mut self, ctx: egui::Context, frame: &mut Frame) -> Result<(), baseview::HandlerError> {
        #[cfg(target_os = "linux")]
        x11::signal_on_thread_exit(self.finished.clone());
        ctx.set_zoom_factor(self.fit.zoom(frame.baseview_window().scale_factor() as f32));
        Ok(())
    }

    /// On Windows this runs inside the window procedure, where a panic would unwind into the host.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut Frame) {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            self.fit.follow(ui.ctx());
            (self.on_frame)();
            (self.ui)(ui);
        }));
    }
}

/// Keeps the window at `size` times the host's scale, drawn at that scale, whatever scale the system
/// reports to the window: egui's zoom makes up the difference.
struct Fit {
    size: (u32, u32),
    scale: Scale,
    /// The zoom egui-baseview last saw. It answers a change of zoom by resizing the window by the
    /// same ratio, unless the pass also asks for a size.
    applied: f32,
}

impl Fit {
    fn zoom(&self, system: f32) -> f32 {
        self.scale.get() as f32 / system
    }

    /// A zoom set here takes effect from the next pass.
    fn follow(&mut self, ctx: &egui::Context) {
        let zoom = self.zoom(ctx.native_pixels_per_point().unwrap_or(1.0));
        if ctx.zoom_factor() != zoom {
            ctx.set_zoom_factor(zoom);
        }
        if ctx.zoom_factor() != self.applied {
            self.applied = ctx.zoom_factor();
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(self.size.0 as f32, self.size.1 as f32)));
        }
    }
}

#[cfg(test)]
mod tests {
    use egui::{RawInput, ViewportCommand, ViewportId, vec2};

    use super::*;

    #[test]
    fn a_scale_no_window_could_be_drawn_at_is_refused() {
        let scale = Scale::default();
        assert_eq!(scale.of((380, 220)), (380, 220));
        assert!(scale.set(1.25));
        assert_eq!(scale.of((380, 220)), (475, 275));
        for factor in [0.0, -1.0, f64::NAN, f64::INFINITY, 100.0] {
            assert!(!scale.set(factor), "{factor}");
        }
        assert_eq!(scale.get(), 1.25);
    }

    /// One pass as egui-baseview runs it, with the scale the system reports; the sizes it was asked for.
    fn pass(ctx: &egui::Context, fit: &mut Fit, system: f32) -> Vec<egui::Vec2> {
        let mut input = RawInput::default();
        input.viewports.entry(ViewportId::ROOT).or_default().native_pixels_per_point = Some(system);
        let mut output = ctx.run_ui(input, |ui| fit.follow(ui.ctx()));
        output.textures_delta.clear();
        let commands = output.viewport_output.get(&ViewportId::ROOT).map(|v| v.commands.clone()).unwrap_or_default();
        commands.into_iter().filter_map(|c| if let ViewportCommand::InnerSize(size) = c { Some(size) } else { None }).collect()
    }

    /// The physical size egui-baseview resizes the window to when asked for `size` at `zoom`.
    fn physical(size: egui::Vec2, zoom: f32, system: f32) -> (u32, u32) {
        let size = size * zoom * system;
        (size.x.round() as u32, size.y.round() as u32)
    }

    #[test]
    fn the_window_draws_at_the_hosts_scale_whatever_scale_the_system_reports() {
        let ctx = egui::Context::default();
        let mut fit = Fit { size: (380, 220), scale: Scale::default(), applied: 1.0 };
        assert!(fit.scale.set(1.5));

        assert_eq!(pass(&ctx, &mut fit, 2.0), vec![], "the zoom applies from the next pass");
        assert_eq!(pass(&ctx, &mut fit, 2.0), vec![vec2(380.0, 220.0)]);
        assert_eq!(ctx.pixels_per_point(), 1.5);
        assert_eq!(physical(vec2(380.0, 220.0), ctx.zoom_factor(), 2.0), (570, 330));
        assert_eq!(pass(&ctx, &mut fit, 2.0), vec![]);

        assert!(fit.scale.set(1.0));
        pass(&ctx, &mut fit, 2.0);
        assert_eq!(pass(&ctx, &mut fit, 2.0), vec![vec2(380.0, 220.0)]);
        assert_eq!(ctx.pixels_per_point(), 1.0);

        pass(&ctx, &mut fit, 1.25);
        assert_eq!(pass(&ctx, &mut fit, 1.25), vec![vec2(380.0, 220.0)], "the system's scale changed under the window");
        assert_eq!(ctx.pixels_per_point(), 1.0);
        assert_eq!(physical(vec2(380.0, 220.0), ctx.zoom_factor(), 1.25), (380, 220));
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
