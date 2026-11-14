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
pub struct Window(ManuallyDrop<baseview::Window>);

impl Window {
    pub fn show(&self) -> bool {
        self.0.show().is_ok()
    }

    pub fn hide(&self) -> bool {
        self.0.hide().is_ok()
    }
}

impl Drop for Window {
    /// On Linux closing joins the window's thread and passes on its panic, which must not reach the host.
    fn drop(&mut self) {
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe { ManuallyDrop::drop(&mut self.0) }));
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
    let window = EguiWindow::create(settings, Editor { ui, on_frame }).ok()?;
    let window = Window(ManuallyDrop::new(window));
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
}

impl App for Editor {
    /// On Windows this runs inside the window procedure, where a panic would unwind into the host.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut Frame) {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            (self.on_frame)();
            (self.ui)(ui);
        }));
    }
}
