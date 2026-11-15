use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use egui_baseview::baseview;
use egui_baseview::baseview::dpi::NativeSize;
use egui_baseview::{App, EguiWindow, EguiWindowSettings, Frame, RepaintNotifier};
use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, WindowHandle};

use crate::{Plugin, Shared};

/// The editor's size and the host's scale factor, shared by the format's view and the window.
///
/// The size is at a scale of 1.0 and changes only for a resizable editor, never to below its
/// minimum or above `Geometry::MAX_SIDE`. The scale is the host's, 1.0 until it gives one. On Linux and
/// Windows the scale the system reports to the window is never used, since the host would not know the size that gives
/// the window. On macOS sizes are in points, which the system draws at the screen's backing scale.
///
/// When either changes while the window is open, the window follows on its next frame by resizing
/// itself, and the parent is resized to match by the host or at the format's request: that keeps the
/// open window instead of reopening it in a parent the host may not have resized yet.
#[derive(Clone)]
pub struct Geometry(Arc<GeometryInner>);

struct GeometryInner {
    factor: AtomicU64,
    /// `width << 32 | height`.
    size: AtomicU64,
    /// `None` for an editor of fixed size.
    min: Option<(u32, u32)>,
    shared: Weak<dyn Shared>,
    repaint: RepaintNotifier,
}

fn pack((width, height): (u32, u32)) -> u64 {
    (width as u64) << 32 | height as u64
}

fn unpack(size: u64) -> (u32, u32) {
    ((size >> 32) as u32, size as u32)
}

impl Geometry {
    const RANGE: std::ops::RangeInclusive<f64> = 0.25..=8.0;

    /// The largest side of a resizable editor at a scale of 1.0, beyond what any window or texture takes: a
    /// size nobody asked for sensibly must not reach the window or the 32-bit sizes of the formats.
    const MAX_SIDE: u32 = 16384;

    /// At the size the plugin keeps for a resizable editor, or else `P::EDITOR_SIZE`.
    pub fn new<P: Plugin>(shared: &Arc<P::Shared>) -> Self {
        let shared: Arc<dyn Shared> = shared.clone();
        Self::with(P::EDITOR_SIZE, P::RESIZABLE.then_some(P::EDITOR_MIN), &shared)
    }

    /// `min` is `None` for an editor of fixed size.
    fn with(size: (u32, u32), min: Option<(u32, u32)>, shared: &Arc<dyn Shared>) -> Self {
        let geometry = Geometry(Arc::new(GeometryInner {
            factor: AtomicU64::new(1f64.to_bits()),
            size: AtomicU64::new(pack(size)),
            min,
            shared: Arc::downgrade(shared),
            repaint: RepaintNotifier::new(),
        }));
        if let Some(size) = min.and(shared.editor_size()) {
            geometry.0.size.store(pack(geometry.fit(size)), Ordering::Relaxed);
        }
        geometry
    }

    /// False, leaving the factor as it was, for one no window could be drawn at.
    pub fn set_scale(&self, factor: f64) -> bool {
        if !Self::RANGE.contains(&factor) {
            return false;
        }
        self.0.factor.store(factor.to_bits(), Ordering::Relaxed);
        self.0.repaint.request_repaint();
        true
    }

    pub fn scale(&self) -> f64 {
        f64::from_bits(self.0.factor.load(Ordering::Relaxed))
    }

    /// `size` at this scale, in the platform's pixels.
    pub fn of(&self, size: (u32, u32)) -> (u32, u32) {
        let factor = self.scale();
        ((size.0 as f64 * factor).round() as u32, (size.1 as f64 * factor).round() as u32)
    }

    /// At a scale of 1.0.
    pub fn size(&self) -> (u32, u32) {
        unpack(self.0.size.load(Ordering::Relaxed))
    }

    /// The size at this scale, in the platform's pixels.
    pub fn physical(&self) -> (u32, u32) {
        self.of(self.size())
    }

    pub fn resizable(&self) -> bool {
        self.0.min.is_some()
    }

    /// The size nearest `size` the editor can take, both at a scale of 1.0.
    pub fn fit(&self, size: (u32, u32)) -> (u32, u32) {
        match self.0.min {
            Some(min) => (size.0.clamp(min.0, Self::MAX_SIDE.max(min.0)), size.1.clamp(min.1, Self::MAX_SIDE.max(min.1))),
            None => self.size(),
        }
    }

    /// Like `fit`, in the platform's pixels at this scale.
    pub fn fit_physical(&self, size: (u32, u32)) -> (u32, u32) {
        match self.0.min {
            Some(min) => {
                let (largest, min) = (self.of((Self::MAX_SIDE, Self::MAX_SIDE)), self.of(min));
                (size.0.clamp(min.0, largest.0.max(min.0)), size.1.clamp(min.1, largest.1.max(min.1)))
            }
            None => self.physical(),
        }
    }

    /// Takes the size nearest `size`, at a scale of 1.0, and tells the plugin when that changed it.
    pub fn resize(&self, size: (u32, u32)) {
        if !self.resizable() {
            return;
        }
        let size = self.fit(size);
        if self.0.size.swap(pack(size), Ordering::Relaxed) != pack(size) {
            self.0.repaint.request_repaint();
            if let Some(shared) = self.0.shared.upgrade() {
                shared.set_editor_size(size.0, size.1);
            }
        }
    }

    /// Like `resize`, for a size in the platform's pixels at this scale.
    pub fn resize_physical(&self, size: (u32, u32)) {
        let factor = self.scale();
        self.resize(((size.0 as f64 / factor).round() as u32, (size.1 as f64 / factor).round() as u32));
    }
}

/// The host's window to embed the editor in. Only the platform's own kind is accepted.
pub enum Parent {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    X11(std::ffi::c_ulong),
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Win32(*mut c_void),
    /// An `NSView`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    AppKit(*mut c_void),
}

/// Whether a host's scale applies: not on macOS, where CLAP and VST3 hosts measure in points and
/// leave the backing scale to the system.
pub const HOST_SCALE: bool = !cfg!(target_os = "macos");

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

/// Opens at `geometry`'s size, in the platform's pixels as CLAP and VST3 hosts measure the parent:
/// physical pixels on Linux and Windows, points on macOS. `on_frame` runs before `ui` on the window's
/// thread. `None` when the parent is no window of this platform or the window cannot be opened, e.g.
/// without a display.
pub fn open(parent: Parent, geometry: Geometry, ui: Box<dyn FnMut(&mut egui::Ui) + Send>, on_frame: Box<dyn FnMut() + Send>) -> Option<Window> {
    let parent = Handle(raw(parent)?);
    let (width, height) = geometry.physical();
    // Resizable to baseview, which otherwise ignores the sizes the window asks for when the host's
    // scale or the editor's size changes.
    let settings = EguiWindowSettings::new()
        .with_size(NativeSize::new(width, height))
        .with_resizable(true)
        .with_repaint_notifier(geometry.0.repaint.clone())
        .with_parent(&parent);
    #[cfg(target_os = "linux")]
    let finished = std::sync::Arc::new(x11::Finished::default());
    let editor = Editor {
        ui,
        on_frame,
        fit: Fit::new(geometry, settings.zoom_factor),
        #[cfg(target_os = "linux")]
        finished: finished.clone(),
    };
    let window = match EguiWindow::create(settings, editor) {
        Ok(window) => window,
        Err(e) => {
            eprintln!("plugin-format: cannot open the editor window: {e}");
            return None;
        }
    };
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
        #[cfg(target_os = "macos")]
        Parent::AppKit(view) => Some(raw_window_handle::AppKitWindowHandle::new(std::ptr::NonNull::new(view)?).into()),
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
    /// Runs once on the window's thread, before the first frame.
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

/// Keeps the window at the editor's size times the host's scale, drawn at that scale, whatever scale
/// the system reports to the window: egui's zoom makes up the difference. On macOS, where the window
/// is measured in points, it draws at the system's scale instead.
struct Fit {
    geometry: Geometry,
    /// The zoom egui-baseview last saw. It answers a change of zoom by resizing the window by the
    /// same ratio, unless the pass also asks for a size.
    applied: f32,
    /// The size the window was last asked for, at a scale of 1.0.
    size: (u32, u32),
}

impl Fit {
    fn new(geometry: Geometry, zoom: f32) -> Self {
        let size = geometry.size();
        Fit { geometry, applied: zoom, size }
    }

    fn zoom(&self, system: f32) -> f32 {
        if cfg!(target_os = "macos") { self.geometry.scale() as f32 } else { self.geometry.scale() as f32 / system }
    }

    /// A zoom set here takes effect from the next pass.
    fn follow(&mut self, ctx: &egui::Context) {
        let zoom = self.zoom(ctx.native_pixels_per_point().unwrap_or(1.0));
        if ctx.zoom_factor() != zoom {
            ctx.set_zoom_factor(zoom);
        }
        let size = self.geometry.size();
        if ctx.zoom_factor() != self.applied || size != self.size {
            self.applied = ctx.zoom_factor();
            self.size = size;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(size.0 as f32, size.1 as f32)));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use egui::{RawInput, ViewportCommand, ViewportId, vec2};

    use super::*;
    use crate::{GuiContext, ParamInfo};

    /// Keeps the editor's size as a plugin would, starting from `kept`.
    struct Kept {
        kept: Option<(u32, u32)>,
        told: Mutex<Vec<(u32, u32)>>,
    }

    impl Shared for Kept {
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
            Box::new(|_| {})
        }
        fn editor_size(&self) -> Option<(u32, u32)> {
            self.kept
        }
        fn set_editor_size(&self, width: u32, height: u32) {
            self.told.lock().unwrap().push((width, height));
        }
    }

    fn kept(kept: Option<(u32, u32)>) -> Arc<Kept> {
        Arc::new(Kept { kept, told: Mutex::default() })
    }

    fn fixed(size: (u32, u32)) -> Geometry {
        let shared: Arc<dyn Shared> = kept(None);
        Geometry::with(size, None, &shared)
    }

    #[test]
    fn a_scale_no_window_could_be_drawn_at_is_refused() {
        let geometry = fixed((380, 220));
        assert_eq!(geometry.physical(), (380, 220));
        assert!(geometry.set_scale(1.25));
        assert_eq!(geometry.physical(), (475, 275));
        for factor in [0.0, -1.0, f64::NAN, f64::INFINITY, 100.0] {
            assert!(!geometry.set_scale(factor), "{factor}");
        }
        assert_eq!(geometry.scale(), 1.25);
    }

    #[test]
    fn a_fixed_editor_keeps_its_size_whatever_the_plugin_kept_or_the_host_asks() {
        let shared = kept(Some((900, 600)));
        let dynamic: Arc<dyn Shared> = shared.clone();
        let geometry = Geometry::with((380, 220), None, &dynamic);
        assert_eq!(geometry.size(), (380, 220));
        assert!(!geometry.resizable());
        assert!(geometry.set_scale(2.0));
        assert_eq!(geometry.fit((10, 10)), (380, 220));
        assert_eq!(geometry.fit_physical((10, 10)), (760, 440));
        geometry.resize((500, 500));
        geometry.resize_physical((1000, 1000));
        assert_eq!(geometry.size(), (380, 220));
        assert!(shared.told.lock().unwrap().is_empty());
    }

    #[test]
    fn a_resizable_editor_opens_at_the_size_the_plugin_kept_and_never_below_its_minimum() {
        let shared = kept(Some((1000, 300)));
        let dynamic: Arc<dyn Shared> = shared.clone();
        let geometry = Geometry::with((1180, 680), Some((760, 440)), &dynamic);
        assert_eq!(geometry.size(), (1000, 440), "the kept size, made no smaller than the minimum");
        assert!(shared.told.lock().unwrap().is_empty(), "opening is no resize");

        geometry.resize((800, 500));
        assert_eq!(geometry.size(), (800, 500));
        geometry.resize((800, 500));
        geometry.resize((10, 10));
        assert_eq!(geometry.size(), (760, 440));
        assert!(geometry.set_scale(1.5));
        assert_eq!(geometry.fit_physical((10, 2000)), (1140, 2000));
        geometry.resize_physical((1500, 900));
        assert_eq!((geometry.size(), geometry.physical()), ((1000, 600), (1500, 900)));
        assert_eq!(*shared.told.lock().unwrap(), [(800, 500), (760, 440), (1000, 600)], "once for each change");

        let fresh: Arc<dyn Shared> = kept(None);
        assert_eq!(Geometry::with((1180, 680), Some((760, 440)), &fresh).size(), (1180, 680));
    }

    #[test]
    fn a_resizable_editor_never_takes_a_size_above_the_largest() {
        let shared = kept(Some((u32::MAX, 5)));
        let dynamic: Arc<dyn Shared> = shared.clone();
        let geometry = Geometry::with((1180, 680), Some((760, 440)), &dynamic);
        let largest = Geometry::MAX_SIDE;
        assert_eq!(geometry.size(), (largest, 440), "a kept size no window could take");

        geometry.resize((u32::MAX, u32::MAX));
        assert_eq!(geometry.size(), (largest, largest));
        assert_eq!(*shared.told.lock().unwrap(), [(largest, largest)]);
        assert!(geometry.set_scale(8.0));
        assert_eq!(geometry.physical(), (largest * 8, largest * 8));
        assert_eq!(geometry.fit_physical((u32::MAX, 10)), (largest * 8, 3520));
        geometry.resize_physical((u32::MAX, 2000));
        assert_eq!(geometry.size(), (largest, 440));
        assert!(i32::try_from(geometry.physical().0).is_ok());

        let fixed = fixed((380, 220));
        fixed.resize((u32::MAX, u32::MAX));
        assert_eq!(fixed.size(), (380, 220));
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

    #[cfg_attr(target_os = "macos", ignore = "macOS draws at the system's scale")]
    #[test]
    fn the_window_draws_at_the_hosts_scale_whatever_scale_the_system_reports() {
        let ctx = egui::Context::default();
        let mut fit = Fit::new(fixed((380, 220)), 1.0);
        assert!(fit.geometry.set_scale(1.5));

        assert_eq!(pass(&ctx, &mut fit, 2.0), vec![], "the zoom applies from the next pass");
        assert_eq!(pass(&ctx, &mut fit, 2.0), vec![vec2(380.0, 220.0)]);
        assert_eq!(ctx.pixels_per_point(), 1.5);
        assert_eq!(physical(vec2(380.0, 220.0), ctx.zoom_factor(), 2.0), (570, 330));
        assert_eq!(pass(&ctx, &mut fit, 2.0), vec![]);

        assert!(fit.geometry.set_scale(1.0));
        pass(&ctx, &mut fit, 2.0);
        assert_eq!(pass(&ctx, &mut fit, 2.0), vec![vec2(380.0, 220.0)]);
        assert_eq!(ctx.pixels_per_point(), 1.0);

        pass(&ctx, &mut fit, 1.25);
        assert_eq!(pass(&ctx, &mut fit, 1.25), vec![vec2(380.0, 220.0)], "the system's scale changed under the window");
        assert_eq!(ctx.pixels_per_point(), 1.0);
        assert_eq!(physical(vec2(380.0, 220.0), ctx.zoom_factor(), 1.25), (380, 220));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_window_draws_at_the_systems_scale_in_points() {
        let ctx = egui::Context::default();
        let mut fit = Fit::new(fixed((380, 220)), 1.0);

        assert_eq!(pass(&ctx, &mut fit, 2.0), vec![]);
        assert_eq!(ctx.pixels_per_point(), 2.0);
        assert_eq!(ctx.zoom_factor(), 1.0, "a window of the editor's size in points");

        assert_eq!(pass(&ctx, &mut fit, 1.0), vec![], "the window moved to a screen of another scale");
        assert_eq!(ctx.pixels_per_point(), 1.0);
    }

    #[test]
    fn the_window_follows_a_resize_at_the_hosts_scale() {
        let ctx = egui::Context::default();
        let shared: Arc<dyn Shared> = kept(None);
        let mut fit = Fit::new(Geometry::with((1180, 680), Some((760, 440)), &shared), 1.0);
        assert!(fit.geometry.set_scale(2.0));
        pass(&ctx, &mut fit, 1.0);
        assert_eq!(pass(&ctx, &mut fit, 1.0), vec![vec2(1180.0, 680.0)]);
        assert_eq!(pass(&ctx, &mut fit, 1.0), vec![]);

        fit.geometry.resize((900, 500));
        assert_eq!(pass(&ctx, &mut fit, 1.0), vec![vec2(900.0, 500.0)]);
        assert_eq!(physical(vec2(900.0, 500.0), ctx.zoom_factor(), 1.0), (1800, 1000));
        assert_eq!(pass(&ctx, &mut fit, 1.0), vec![]);
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
