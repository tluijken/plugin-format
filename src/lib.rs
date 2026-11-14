use core::ffi::CStr;

pub mod clap;
mod editor;
mod gui_context;
mod param;
pub mod vst3;

pub use egui;
pub use gui_context::GuiContext;
pub use param::*;

pub struct Info {
    pub name: &'static str,
    pub vendor: &'static str,
    pub version: &'static str,
    pub clap_id: &'static str,
    pub description: &'static str,
    pub clap_features: &'static [&'static CStr],
    pub vst3_class_id: [u8; 16],
    pub vst3_subcategories: &'static str,
    /// A stereo sidechain input the host may leave unconnected: a second CLAP input port, a VST3 aux
    /// input bus that starts inactive.
    pub sidechain: bool,
}

/// What one call of `Plugin::process` works on.
pub struct Block<'a> {
    /// The output channels, filled with the input beforehand by the wrapper (mono input copied to
    /// output channel 0 only).
    pub channels: &'a mut [&'a mut [f32]],
    /// `None` unless `Info::sidechain` is set and the host connected both channels of the sidechain.
    pub sidechain: Option<[&'a [f32]; 2]>,
    /// The timeline position in samples of the block's first frame; `None` when the host gave none.
    pub position: Option<i64>,
    pub playing: bool,
}

/// The half of a plugin that any thread may use at any time, also while the audio half processes.
pub trait Shared: Send + Sync + 'static {
    fn params(&self) -> &'static [ParamInfo];
    /// Plain values.
    fn param(&self, id: u32) -> f64;
    /// Ignores unknown ids and clamps through `ParamInfo::clamp`.
    fn set_param(&self, id: u32, plain: f64);
    fn param_text(&self, id: u32, plain: f64) -> String;
    fn param_from_text(&self, id: u32, text: &str) -> Option<f64>;
    fn save(&self) -> Vec<u8>;
    fn load(&self, bytes: &[u8]) -> bool;
    /// A new one for every window the host opens. The window's thread calls it with the root `Ui` for
    /// every frame while the window is open: on Linux a thread of its own, on Windows and macOS the host's UI thread.
    /// `cx` is the instance's, the same one `attach` was given.
    fn editor(&self, cx: GuiContext) -> Box<dyn FnMut(&mut egui::Ui) + Send>;
    /// Called once the host is known, before any window opens, and again should a VST3 host replace
    /// its component handler; `cx` is the same each time. For changes made without an open editor.
    fn attach(&self, _cx: GuiContext) {}
    /// What the host says about the track, once the host is known and whenever it changes. Called on
    /// the host's main thread.
    fn set_track(&self, _info: TrackInfo) {}
    /// The size, at a scale of 1.0, to open a `Plugin::RESIZABLE` editor at instead of
    /// `Plugin::EDITOR_SIZE`. Asked each time the host opens the editor.
    fn editor_size(&self) -> Option<(u32, u32)> {
        None
    }
    /// The size, at a scale of 1.0, the editor was resized to, for `editor_size` to give back.
    fn set_editor_size(&self, _width: u32, _height: u32) {}
}

/// What the host says about the track the plugin is on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackInfo {
    pub name: Option<String>,
    pub color: Option<[u8; 3]>,
}

/// The audio half, which the wrapper only ever uses from one thread at a time.
pub trait Plugin: Send + Sized + 'static {
    const INFO: Info;
    /// The editor's size at a scale of 1.0; for a `RESIZABLE` one, the size it starts at.
    const EDITOR_SIZE: (u32, u32);
    /// Whether the host may resize the editor, and the editor may ask the host to.
    const RESIZABLE: bool = false;
    /// The smallest size, at a scale of 1.0, a `RESIZABLE` editor takes.
    const EDITOR_MIN: (u32, u32) = Self::EDITOR_SIZE;
    type Shared: Shared;
    fn new() -> (Self, std::sync::Arc<Self::Shared>);
    /// `inputs` is the main input bus the host chose: 1 (mono in, stereo out) or 2 (stereo/stereo).
    /// Returns the latency to report.
    fn activate(&mut self, sample_rate: f64, max_block: usize, inputs: usize) -> u32;
    fn deactivate(&mut self);
    /// Read once before the first `activate`; after that the wrapper reports what `activate` returned.
    fn latency(&self) -> u32;
    fn process(&mut self, block: Block<'_>);
    /// True once after `process` changed the latency; the wrapper then tells the host.
    fn take_latency_changed(&mut self) -> bool;
}

/// Exports `$plugin` as the library's CLAP plugin through the `clap_entry` symbol.
#[macro_export]
macro_rules! export_clap {
    ($plugin:ty) => {
        #[unsafe(no_mangle)]
        pub static clap_entry: $crate::clap::sys::entry::clap_plugin_entry = $crate::clap::entry::<$plugin>();
    };
}

/// Exports `$plugin` as the library's VST3 plugin through `GetPluginFactory` and the platform's module entry points.
#[macro_export]
macro_rules! export_vst3 {
    ($plugin:ty) => {
        #[unsafe(no_mangle)]
        pub extern "system" fn GetPluginFactory() -> *mut ::core::ffi::c_void {
            $crate::vst3::factory::<$plugin>()
        }

        #[cfg(target_os = "linux")]
        #[unsafe(no_mangle)]
        pub extern "system" fn ModuleEntry(_: *mut ::core::ffi::c_void) -> bool {
            true
        }

        #[cfg(target_os = "linux")]
        #[unsafe(no_mangle)]
        pub extern "system" fn ModuleExit() -> bool {
            true
        }

        #[cfg(target_os = "windows")]
        #[unsafe(no_mangle)]
        pub extern "system" fn InitDll() -> bool {
            true
        }

        #[cfg(target_os = "windows")]
        #[unsafe(no_mangle)]
        pub extern "system" fn ExitDll() -> bool {
            true
        }

        #[cfg(target_os = "macos")]
        #[unsafe(no_mangle)]
        pub extern "system" fn bundleEntry(_: *mut ::core::ffi::c_void) -> bool {
            true
        }

        #[cfg(target_os = "macos")]
        #[unsafe(no_mangle)]
        pub extern "system" fn bundleExit() -> bool {
            true
        }
    };
}

fn guard<R>(failed: R, body: impl FnOnce() -> R) -> R {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or(failed)
}

/// What the host said about transport, and the sidechain if it gave one.
struct Extras {
    sidechain: Option<[*const f32; 2]>,
    position: Option<i64>,
    playing: bool,
}

/// Fills the output channels from the input and lets `plugin` process them in place: input channel
/// `c` goes to output `c` when `c < configured`, a missing one becomes silence, and outputs from
/// `configured` on are left to the plugin.
///
/// # Safety
/// `outs` holds one or two distinct channels, each valid for `frames` writes. Every non-null `ins[c]`
/// is valid for `frames` reads and is either the same memory as `outs[c]` or overlaps no output.
/// A `sidechain` is valid for `frames` reads; one that overlaps an output is dropped.
unsafe fn process_block<P: Plugin>(plugin: &mut P, frames: usize, outs: &[*mut f32], ins: &[*const f32], configured: usize, extras: Extras) {
    unsafe {
        for (c, &out) in outs.iter().enumerate() {
            let source = if c < configured { ins.get(c).copied().unwrap_or(std::ptr::null()) } else { std::ptr::null() };
            if !source.is_null() {
                if !std::ptr::eq(source, out) {
                    std::ptr::copy(source, out, frames);
                }
            } else if c < configured {
                std::ptr::write_bytes(out, 0, frames);
            }
        }
        let apart = |side: *const f32| outs.iter().all(|&out| (side as usize).abs_diff(out as usize) >= frames * size_of::<f32>());
        let sidechain = extras
            .sidechain
            .filter(|side| side.iter().all(|&s| apart(s)))
            .map(|[left, right]| [std::slice::from_raw_parts(left, frames), std::slice::from_raw_parts(right, frames)]);
        let mut channels: [&mut [f32]; 2] = [&mut [], &mut []];
        for (channel, &out) in channels.iter_mut().zip(outs) {
            *channel = std::slice::from_raw_parts_mut(out, frames);
        }
        let count = outs.len().min(2);
        plugin.process(Block { channels: &mut channels[..count], sidechain, position: extras.position, playing: extras.playing });
    }
}

/// Writes `text` NUL-terminated into `out`, cut at a character boundary if it does not fit.
fn copy_str(text: &str, out: &mut [core::ffi::c_char]) {
    let Some(room) = out.len().checked_sub(1) else { return };
    let mut len = text.len().min(room);
    while !text.is_char_boundary(len) {
        len -= 1;
    }
    for (o, &b) in out.iter_mut().zip(&text.as_bytes()[..len]) {
        *o = b as core::ffi::c_char;
    }
    out[len] = 0;
}
