use core::ffi::CStr;

pub mod clap;
mod param;

pub use egui;
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
    /// Called on the GUI thread for every frame while the window is open.
    fn editor(&self) -> Box<dyn FnMut(&egui::Context) + Send>;
}

/// The audio half, which the wrapper only ever uses from one thread at a time.
pub trait Plugin: Send + Sized + 'static {
    const INFO: Info;
    const EDITOR_SIZE: (u32, u32);
    type Shared: Shared;
    fn new() -> (Self, std::sync::Arc<Self::Shared>);
    /// `inputs` is the main input bus the host chose: 1 (mono in, stereo out) or 2 (stereo/stereo).
    /// Returns the latency to report.
    fn activate(&mut self, sample_rate: f64, max_block: usize, inputs: usize) -> u32;
    fn deactivate(&mut self);
    fn latency(&self) -> u32;
    /// `channels` are the output channels, filled with the input beforehand by the wrapper
    /// (mono input copied to output channel 0 only).
    fn process(&mut self, channels: &mut [&mut [f32]]);
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
