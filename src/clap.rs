use std::cell::UnsafeCell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[doc(hidden)]
pub use clap_sys as sys;

use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE, clap_event_param_value, clap_input_events, clap_output_events};
use clap_sys::ext::audio_ports::{CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, CLAP_PORT_MONO, CLAP_PORT_STEREO, clap_audio_port_info, clap_plugin_audio_ports};
use clap_sys::ext::audio_ports_config::{CLAP_EXT_AUDIO_PORTS_CONFIG, clap_audio_ports_config, clap_plugin_audio_ports_config};
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_gui_resize_hints, clap_plugin_gui, clap_window};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use clap_sys::ext::params::{CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_STEPPED, CLAP_PARAM_RESCAN_VALUES, clap_host_params, clap_param_info, clap_plugin_params};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::host::clap_host;
use clap_sys::id::{CLAP_INVALID_ID, clap_id};
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR, clap_process, clap_process_status};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::{CLAP_VERSION, clap_version_is_compatible};

use crate::Plugin;

fn guard<R>(failed: R, body: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(failed)
}

fn same(a: *const c_char, b: &CStr) -> bool {
    !a.is_null() && unsafe { CStr::from_ptr(a) } == b
}

/// Writes `text` NUL-terminated into `out`, cut at a character boundary if it does not fit.
fn copy_str(text: &str, out: &mut [c_char]) {
    let Some(room) = out.len().checked_sub(1) else { return };
    let mut len = text.len().min(room);
    while !text.is_char_boundary(len) {
        len -= 1;
    }
    for (o, &b) in out.iter_mut().zip(&text.as_bytes()[..len]) {
        *o = b as c_char;
    }
    out[len] = 0;
}

struct Descriptor {
    _strings: Vec<CString>,
    _features: Vec<*const c_char>,
    clap: clap_plugin_descriptor,
}

// The raw pointers point into the owned strings and the 'static feature names, never written.
unsafe impl Send for Descriptor {}
unsafe impl Sync for Descriptor {}

/// One per library: a library exports a single `clap_entry` and so a single plugin.
static DESCRIPTOR: OnceLock<Descriptor> = OnceLock::new();

fn descriptor<P: Plugin>() -> &'static clap_plugin_descriptor {
    &DESCRIPTOR
        .get_or_init(|| {
            let info = &P::INFO;
            let strings: Vec<CString> = [info.clap_id, info.name, info.vendor, info.version, info.description]
                .iter()
                .map(|s| CString::new(*s).unwrap_or_default())
                .collect();
            let features: Vec<*const c_char> = info.clap_features.iter().map(|f| f.as_ptr()).chain([ptr::null()]).collect();
            let clap = clap_plugin_descriptor {
                clap_version: CLAP_VERSION,
                id: strings[0].as_ptr(),
                name: strings[1].as_ptr(),
                vendor: strings[2].as_ptr(),
                url: c"".as_ptr(),
                manual_url: c"".as_ptr(),
                support_url: c"".as_ptr(),
                version: strings[3].as_ptr(),
                description: strings[4].as_ptr(),
                features: features.as_ptr(),
            };
            Descriptor { _strings: strings, _features: features, clap }
        })
        .clap
}

pub const fn entry<P: Plugin>() -> clap_plugin_entry {
    clap_plugin_entry { clap_version: CLAP_VERSION, init: Some(entry_init), deinit: Some(entry_deinit), get_factory: Some(get_factory::<P>) }
}

unsafe extern "C" fn entry_init(_path: *const c_char) -> bool {
    true
}

unsafe extern "C" fn entry_deinit() {}

unsafe extern "C" fn get_factory<P: Plugin>(id: *const c_char) -> *const c_void {
    guard(ptr::null(), || {
        if same(id, CLAP_PLUGIN_FACTORY_ID) { &Factory::<P>::FACTORY as *const clap_plugin_factory as *const c_void } else { ptr::null() }
    })
}

struct Factory<P>(std::marker::PhantomData<P>);

impl<P: Plugin> Factory<P> {
    const FACTORY: clap_plugin_factory = clap_plugin_factory {
        get_plugin_count: Some(Self::count),
        get_plugin_descriptor: Some(Self::descriptor),
        create_plugin: Some(Self::create),
    };

    unsafe extern "C" fn count(_: *const clap_plugin_factory) -> u32 {
        1
    }

    unsafe extern "C" fn descriptor(_: *const clap_plugin_factory, index: u32) -> *const clap_plugin_descriptor {
        guard(ptr::null(), || if index == 0 { descriptor::<P>() as *const _ } else { ptr::null() })
    }

    unsafe extern "C" fn create(_: *const clap_plugin_factory, host: *const clap_host, id: *const c_char) -> *const clap_plugin {
        guard(ptr::null(), || unsafe {
            let desc = descriptor::<P>();
            if host.is_null() || !clap_version_is_compatible((*host).clap_version) || !same(id, CStr::from_ptr(desc.id)) {
                return ptr::null();
            }
            Instance::<P>::create(host, desc)
        })
    }
}

/// `plugin` is touched through `&mut` only by `activate`, `deactivate` and `destroy` (main
/// thread, never while processing, per CLAP) and by `process` (audio thread, only while active).
/// Everything else takes `&P` and relies on `Plugin`'s `&self` methods being callable from any
/// thread.
struct Instance<P> {
    clap: clap_plugin,
    host: *const clap_host,
    plugin: UnsafeCell<P>,
    active: AtomicBool,
    inputs: AtomicU32,
    /// What `activate` reported; the host only learns a new latency by reactivating.
    latency: AtomicU32,
}

impl<P: Plugin> Instance<P> {
    fn create(host: *const clap_host, desc: &'static clap_plugin_descriptor) -> *const clap_plugin {
        let instance = Box::into_raw(Box::new(Instance {
            clap: clap_plugin {
                desc,
                plugin_data: ptr::null_mut(),
                init: Some(Self::init),
                destroy: Some(Self::destroy),
                activate: Some(Self::activate),
                deactivate: Some(Self::deactivate),
                start_processing: Some(Self::start_processing),
                stop_processing: Some(Self::stop_processing),
                reset: Some(Self::reset),
                process: Some(Self::process),
                get_extension: Some(Self::get_extension),
                on_main_thread: Some(Self::on_main_thread),
            },
            host,
            plugin: UnsafeCell::new(P::new()),
            active: AtomicBool::new(false),
            inputs: AtomicU32::new(1),
            latency: AtomicU32::new(0),
        }));
        unsafe {
            (*instance).clap.plugin_data = instance as *mut c_void;
            &(*instance).clap
        }
    }

    unsafe fn from<'a>(plugin: *const clap_plugin) -> Option<&'a Self> {
        unsafe {
            if plugin.is_null() || (*plugin).plugin_data.is_null() {
                return None;
            }
            Some(&*((*plugin).plugin_data as *const Self))
        }
    }

    fn plugin(&self) -> &P {
        unsafe { &*self.plugin.get() }
    }

    /// Callers hold the exclusivity described on `Instance`.
    #[allow(clippy::mut_from_ref)]
    unsafe fn plugin_mut(&self) -> &mut P {
        unsafe { &mut *self.plugin.get() }
    }

    fn with<R>(plugin: *const clap_plugin, failed: impl FnOnce() -> R, body: impl FnOnce(&Self) -> R) -> R {
        catch_unwind(AssertUnwindSafe(|| unsafe { Self::from(plugin) }.map(body))).ok().flatten().unwrap_or_else(failed)
    }

    unsafe extern "C" fn init(plugin: *const clap_plugin) -> bool {
        Self::with(plugin, || false, |_| true)
    }

    unsafe extern "C" fn destroy(plugin: *const clap_plugin) {
        guard((), || unsafe {
            if plugin.is_null() || (*plugin).plugin_data.is_null() {
                return;
            }
            let instance = Box::from_raw((*plugin).plugin_data as *mut Self);
            if instance.active.load(Ordering::Acquire) {
                instance.plugin_mut().deactivate();
            }
        })
    }

    unsafe extern "C" fn activate(plugin: *const clap_plugin, sample_rate: f64, _min: u32, max: u32) -> bool {
        Self::with(plugin, || false, |instance| {
            if instance.active.load(Ordering::Acquire) {
                return false;
            }
            let inputs = instance.inputs.load(Ordering::Relaxed) as usize;
            let latency = unsafe { instance.plugin_mut() }.activate(sample_rate, max as usize, inputs);
            instance.latency.store(latency, Ordering::Relaxed);
            instance.active.store(true, Ordering::Release);
            true
        })
    }

    unsafe extern "C" fn deactivate(plugin: *const clap_plugin) {
        Self::with(plugin, || (), |instance| {
            if instance.active.swap(false, Ordering::AcqRel) {
                unsafe { instance.plugin_mut() }.deactivate();
            }
        })
    }

    unsafe extern "C" fn start_processing(plugin: *const clap_plugin) -> bool {
        Self::with(plugin, || false, |instance| instance.active.load(Ordering::Acquire))
    }

    unsafe extern "C" fn stop_processing(_: *const clap_plugin) {}

    unsafe extern "C" fn reset(_: *const clap_plugin) {}

    unsafe extern "C" fn on_main_thread(_: *const clap_plugin) {}

    unsafe extern "C" fn process(plugin: *const clap_plugin, process: *const clap_process) -> clap_process_status {
        Self::with(plugin, || CLAP_PROCESS_ERROR, |instance| unsafe {
            if process.is_null() || !instance.active.load(Ordering::Acquire) {
                return CLAP_PROCESS_ERROR;
            }
            let process = &*process;
            instance.apply(process.in_events);
            let status = instance.run(process);
            if instance.plugin_mut().take_latency_changed()
                && let Some(host) = instance.host.as_ref()
                && let Some(restart) = host.request_restart
            {
                restart(instance.host);
            }
            status
        })
    }

    /// The audio thread has `plugin` to itself while active, which makes `plugin_mut` sound here.
    unsafe fn run(&self, process: &clap_process) -> clap_process_status {
        unsafe {
            let frames = process.frames_count as usize;
            if frames == 0 || process.audio_outputs_count == 0 || process.audio_outputs.is_null() {
                return CLAP_PROCESS_CONTINUE;
            }
            let output = &*process.audio_outputs;
            let count = (output.channel_count as usize).min(2);
            if output.data32.is_null() || count == 0 {
                return CLAP_PROCESS_ERROR;
            }
            let mut outs = [ptr::null_mut::<f32>(); 2];
            for (c, out) in outs.iter_mut().enumerate().take(count) {
                *out = *output.data32.add(c);
                if out.is_null() {
                    return CLAP_PROCESS_ERROR;
                }
            }
            if count == 2 && outs[0] == outs[1] {
                return CLAP_PROCESS_ERROR;
            }

            let input = (process.audio_inputs_count > 0 && !process.audio_inputs.is_null()).then(|| &*process.audio_inputs);
            let configured = self.inputs.load(Ordering::Relaxed) as usize;
            let inputs = match input {
                Some(input) if !input.data32.is_null() => (input.channel_count as usize).min(configured),
                _ => 0,
            };
            for (c, &out) in outs.iter().enumerate().take(count) {
                let source = match input {
                    Some(input) if c < inputs => *input.data32.add(c),
                    _ => ptr::null_mut(),
                };
                if !source.is_null() {
                    if source != out {
                        ptr::copy(source, out, frames);
                    }
                } else if c < configured {
                    ptr::write_bytes(out, 0, frames);
                }
            }

            let [left, right] = outs;
            let mut channels: [&mut [f32]; 2] = [&mut [], &mut []];
            channels[0] = std::slice::from_raw_parts_mut(left, frames);
            if count == 2 {
                channels[1] = std::slice::from_raw_parts_mut(right, frames);
            }
            self.plugin_mut().process(&mut channels[..count]);
            CLAP_PROCESS_CONTINUE
        }
    }

    fn apply(&self, events: *const clap_input_events) {
        if events.is_null() {
            return;
        }
        unsafe {
            let (Some(size), Some(get)) = ((*events).size, (*events).get) else { return };
            for i in 0..size(events) {
                let header = get(events, i);
                if header.is_null()
                    || (*header).space_id != CLAP_CORE_EVENT_SPACE_ID
                    || (*header).type_ != CLAP_EVENT_PARAM_VALUE
                    || ((*header).size as usize) < size_of::<clap_event_param_value>()
                {
                    continue;
                }
                let event = &*(header as *const clap_event_param_value);
                self.plugin().set_param(event.param_id, event.value);
            }
        }
    }

    fn info(&self, id: clap_id) -> Option<&'static crate::ParamInfo> {
        self.plugin().params().iter().find(|p| p.id == id)
    }

    unsafe extern "C" fn get_extension(plugin: *const clap_plugin, id: *const c_char) -> *const c_void {
        Self::with(plugin, ptr::null, |_| {
            let ext: *const c_void = if same(id, CLAP_EXT_PARAMS) {
                &Self::PARAMS as *const _ as _
            } else if same(id, CLAP_EXT_STATE) {
                &Self::STATE as *const _ as _
            } else if same(id, CLAP_EXT_LATENCY) {
                &Self::LATENCY as *const _ as _
            } else if same(id, CLAP_EXT_AUDIO_PORTS) {
                &Self::AUDIO_PORTS as *const _ as _
            } else if same(id, CLAP_EXT_AUDIO_PORTS_CONFIG) {
                &Self::AUDIO_PORTS_CONFIG as *const _ as _
            } else if same(id, CLAP_EXT_GUI) {
                &Self::GUI as *const _ as _
            } else {
                ptr::null()
            };
            ext
        })
    }

    const PARAMS: clap_plugin_params = clap_plugin_params {
        count: Some(Self::param_count),
        get_info: Some(Self::param_info),
        get_value: Some(Self::param_value),
        value_to_text: Some(Self::value_to_text),
        text_to_value: Some(Self::text_to_value),
        flush: Some(Self::flush),
    };

    unsafe extern "C" fn param_count(plugin: *const clap_plugin) -> u32 {
        Self::with(plugin, || 0, |instance| instance.plugin().params().len() as u32)
    }

    unsafe extern "C" fn param_info(plugin: *const clap_plugin, index: u32, out: *mut clap_param_info) -> bool {
        Self::with(plugin, || false, |instance| {
            let Some(info) = instance.plugin().params().get(index as usize) else { return false };
            if out.is_null() {
                return false;
            }
            let mut flags = CLAP_PARAM_IS_STEPPED;
            if info.automatable {
                flags |= CLAP_PARAM_IS_AUTOMATABLE;
            }
            let mut clap = clap_param_info {
                id: info.id,
                flags,
                cookie: ptr::null_mut(),
                name: [0; _],
                module: [0; _],
                min_value: info.min(),
                max_value: info.max(),
                default_value: info.default,
            };
            copy_str(info.name, &mut clap.name);
            unsafe { out.write(clap) };
            true
        })
    }

    unsafe extern "C" fn param_value(plugin: *const clap_plugin, id: clap_id, out: *mut f64) -> bool {
        Self::with(plugin, || false, |instance| {
            if out.is_null() || instance.info(id).is_none() {
                return false;
            }
            unsafe { out.write(instance.plugin().param(id)) };
            true
        })
    }

    unsafe extern "C" fn value_to_text(plugin: *const clap_plugin, id: clap_id, value: f64, out: *mut c_char, capacity: u32) -> bool {
        Self::with(plugin, || false, |instance| {
            if out.is_null() || capacity == 0 || instance.info(id).is_none() {
                return false;
            }
            let out = unsafe { std::slice::from_raw_parts_mut(out, capacity as usize) };
            copy_str(&instance.plugin().param_text(id, value), out);
            true
        })
    }

    unsafe extern "C" fn text_to_value(plugin: *const clap_plugin, id: clap_id, text: *const c_char, out: *mut f64) -> bool {
        Self::with(plugin, || false, |instance| {
            if text.is_null() || out.is_null() || instance.info(id).is_none() {
                return false;
            }
            let Ok(text) = unsafe { CStr::from_ptr(text) }.to_str() else { return false };
            let Some(value) = instance.plugin().param_from_text(id, text) else { return false };
            unsafe { out.write(value) };
            true
        })
    }

    unsafe extern "C" fn flush(plugin: *const clap_plugin, events: *const clap_input_events, _out: *const clap_output_events) {
        Self::with(plugin, || (), |instance| instance.apply(events))
    }

    const STATE: clap_plugin_state = clap_plugin_state { save: Some(Self::save), load: Some(Self::load) };

    unsafe extern "C" fn save(plugin: *const clap_plugin, stream: *const clap_ostream) -> bool {
        Self::with(plugin, || false, |instance| unsafe {
            let Some(write) = stream.as_ref().and_then(|s| s.write) else { return false };
            let bytes = instance.plugin().save();
            let mut rest = &bytes[..];
            while !rest.is_empty() {
                let written = write(stream, rest.as_ptr() as *const c_void, rest.len() as u64);
                if written <= 0 {
                    return false;
                }
                rest = &rest[(written as usize).min(rest.len())..];
            }
            true
        })
    }

    unsafe extern "C" fn load(plugin: *const clap_plugin, stream: *const clap_istream) -> bool {
        Self::with(plugin, || false, |instance| unsafe {
            let Some(read) = stream.as_ref().and_then(|s| s.read) else { return false };
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let got = read(stream, chunk.as_mut_ptr() as *mut c_void, chunk.len() as u64);
                if got < 0 {
                    return false;
                }
                if got == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..(got as usize).min(chunk.len())]);
            }
            if !instance.plugin().load(&bytes) {
                return false;
            }
            instance.rescan_values();
            true
        })
    }

    fn rescan_values(&self) {
        unsafe {
            let Some(host) = self.host.as_ref() else { return };
            let Some(get_extension) = host.get_extension else { return };
            let params = get_extension(self.host, CLAP_EXT_PARAMS.as_ptr()) as *const clap_host_params;
            if let Some(rescan) = params.as_ref().and_then(|p| p.rescan) {
                rescan(self.host, CLAP_PARAM_RESCAN_VALUES);
            }
        }
    }

    const LATENCY: clap_plugin_latency = clap_plugin_latency { get: Some(Self::latency) };

    unsafe extern "C" fn latency(plugin: *const clap_plugin) -> u32 {
        Self::with(plugin, || 0, |instance| {
            if instance.active.load(Ordering::Acquire) { instance.latency.load(Ordering::Relaxed) } else { instance.plugin().latency() }
        })
    }

    const AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports { count: Some(Self::port_count), get: Some(Self::port) };

    unsafe extern "C" fn port_count(_: *const clap_plugin, _is_input: bool) -> u32 {
        1
    }

    unsafe extern "C" fn port(plugin: *const clap_plugin, index: u32, is_input: bool, out: *mut clap_audio_port_info) -> bool {
        Self::with(plugin, || false, |instance| {
            if index != 0 || out.is_null() {
                return false;
            }
            let channels = if is_input { instance.inputs.load(Ordering::Relaxed) } else { 2 };
            let mut info = clap_audio_port_info {
                id: 0,
                name: [0; _],
                flags: CLAP_AUDIO_PORT_IS_MAIN,
                channel_count: channels,
                port_type: port_type(channels),
                in_place_pair: CLAP_INVALID_ID,
            };
            copy_str(if is_input { "Input" } else { "Output" }, &mut info.name);
            unsafe { out.write(info) };
            true
        })
    }

    const AUDIO_PORTS_CONFIG: clap_plugin_audio_ports_config =
        clap_plugin_audio_ports_config { count: Some(Self::config_count), get: Some(Self::config), select: Some(Self::select) };

    unsafe extern "C" fn config_count(_: *const clap_plugin) -> u32 {
        CONFIGS.len() as u32
    }

    unsafe extern "C" fn config(_: *const clap_plugin, index: u32, out: *mut clap_audio_ports_config) -> bool {
        guard(false, || {
            let Some(&(name, inputs)) = CONFIGS.get(index as usize) else { return false };
            if out.is_null() {
                return false;
            }
            let mut config = clap_audio_ports_config {
                id: index,
                name: [0; _],
                input_port_count: 1,
                output_port_count: 1,
                has_main_input: true,
                main_input_channel_count: inputs,
                main_input_port_type: port_type(inputs),
                has_main_output: true,
                main_output_channel_count: 2,
                main_output_port_type: port_type(2),
            };
            copy_str(name, &mut config.name);
            unsafe { out.write(config) };
            true
        })
    }

    unsafe extern "C" fn select(plugin: *const clap_plugin, id: clap_id) -> bool {
        Self::with(plugin, || false, |instance| {
            let Some(&(_, inputs)) = CONFIGS.get(id as usize) else { return false };
            if instance.active.load(Ordering::Acquire) {
                return false;
            }
            instance.inputs.store(inputs, Ordering::Relaxed);
            true
        })
    }

    const GUI: clap_plugin_gui = clap_plugin_gui {
        is_api_supported: Some(gui::is_api_supported),
        get_preferred_api: Some(gui::get_preferred_api),
        create: Some(gui::create),
        destroy: Some(gui::destroy),
        set_scale: Some(gui::set_scale),
        get_size: Some(gui::get_size),
        can_resize: Some(gui::unsupported),
        get_resize_hints: Some(gui::get_resize_hints),
        adjust_size: Some(gui::get_size),
        set_size: Some(gui::set_size),
        set_parent: Some(gui::set_window),
        set_transient: Some(gui::set_window),
        suggest_title: Some(gui::suggest_title),
        show: Some(gui::unsupported),
        hide: Some(gui::unsupported),
    };
}

/// Config id is the index; the second field is the main input's channel count.
const CONFIGS: [(&str, u32); 2] = [("Mono in, stereo out", 1), ("Stereo in, stereo out", 2)];

fn port_type(channels: u32) -> *const c_char {
    if channels == 1 { CLAP_PORT_MONO.as_ptr() } else { CLAP_PORT_STEREO.as_ptr() }
}

mod gui {
    use super::*;

    pub unsafe extern "C" fn is_api_supported(_: *const clap_plugin, _: *const c_char, _: bool) -> bool {
        false
    }
    pub unsafe extern "C" fn get_preferred_api(_: *const clap_plugin, _: *mut *const c_char, _: *mut bool) -> bool {
        false
    }
    pub unsafe extern "C" fn create(_: *const clap_plugin, _: *const c_char, _: bool) -> bool {
        false
    }
    pub unsafe extern "C" fn destroy(_: *const clap_plugin) {}
    pub unsafe extern "C" fn set_scale(_: *const clap_plugin, _: f64) -> bool {
        false
    }
    pub unsafe extern "C" fn get_size(_: *const clap_plugin, _: *mut u32, _: *mut u32) -> bool {
        false
    }
    pub unsafe extern "C" fn unsupported(_: *const clap_plugin) -> bool {
        false
    }
    pub unsafe extern "C" fn get_resize_hints(_: *const clap_plugin, _: *mut clap_gui_resize_hints) -> bool {
        false
    }
    pub unsafe extern "C" fn set_size(_: *const clap_plugin, _: u32, _: u32) -> bool {
        false
    }
    pub unsafe extern "C" fn set_window(_: *const clap_plugin, _: *const clap_window) -> bool {
        false
    }
    pub unsafe extern "C" fn suggest_title(_: *const clap_plugin, _: *const c_char) {}
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use clap_sys::audio_buffer::clap_audio_buffer;

    use super::*;
    use crate::{Info, Kind, ParamInfo};

    static RESTARTS: AtomicUsize = AtomicUsize::new(0);

    struct Doubler {
        changed: bool,
    }

    impl Plugin for Doubler {
        const INFO: Info = Info {
            name: "Doubler",
            vendor: "test",
            version: "0",
            clap_id: "test.doubler",
            description: "",
            clap_features: &[c"audio-effect"],
            vst3_class_id: [0; 16],
            vst3_subcategories: "Fx",
        };
        const EDITOR_SIZE: (u32, u32) = (1, 1);
        fn new() -> Self {
            Doubler { changed: false }
        }
        fn params(&self) -> &'static [ParamInfo] {
            const PARAMS: &[ParamInfo] = &[ParamInfo { id: 7, name: "Gain", kind: Kind::Bool, default: 1.0, automatable: true }];
            PARAMS
        }
        fn param(&self, _: u32) -> f64 {
            1.0
        }
        fn set_param(&self, _: u32, _: f64) {}
        fn param_text(&self, _: u32, _: f64) -> String {
            "a much longer text than fits".into()
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
        fn activate(&mut self, _: f64, _: usize, _: usize) -> u32 {
            5
        }
        fn deactivate(&mut self) {}
        fn latency(&self) -> u32 {
            0
        }
        fn process(&mut self, channels: &mut [&mut [f32]]) {
            for s in channels[0].iter_mut() {
                *s *= 2.0;
            }
            self.changed = true;
        }
        fn take_latency_changed(&mut self) -> bool {
            std::mem::take(&mut self.changed)
        }
        fn editor(&self) -> Box<dyn FnMut(&egui::Context) + Send> {
            Box::new(|_| {})
        }
    }

    unsafe extern "C" fn restart(_: *const clap_host) {
        RESTARTS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn in_place_mono_is_processed_once_and_a_latency_change_asks_for_a_restart() {
        let host = clap_host {
            clap_version: CLAP_VERSION,
            host_data: ptr::null_mut(),
            name: c"".as_ptr(),
            vendor: c"".as_ptr(),
            url: c"".as_ptr(),
            version: c"".as_ptr(),
            get_extension: None,
            request_restart: Some(restart),
            request_process: None,
            request_callback: None,
        };
        unsafe {
            let factory = &Factory::<Doubler>::FACTORY;
            let plugin = (factory.create_plugin.unwrap())(factory, &host, c"test.doubler".as_ptr());
            let p = &*plugin;
            let mut left = vec![0.5f32; 16];
            let mut right = vec![9.0f32; 16];
            let mut in_ptrs = [left.as_mut_ptr()];
            let mut out_ptrs = [left.as_mut_ptr(), right.as_mut_ptr()];
            let input = clap_audio_buffer { data32: in_ptrs.as_mut_ptr(), data64: ptr::null_mut(), channel_count: 1, latency: 0, constant_mask: 0 };
            let mut output = clap_audio_buffer { data32: out_ptrs.as_mut_ptr(), data64: ptr::null_mut(), channel_count: 2, latency: 0, constant_mask: 0 };
            let process = clap_process {
                steady_time: 0,
                frames_count: 16,
                transport: ptr::null(),
                audio_inputs: &input,
                audio_outputs: &mut output,
                audio_inputs_count: 1,
                audio_outputs_count: 1,
                in_events: ptr::null(),
                out_events: ptr::null(),
            };
            assert_eq!((p.process.unwrap())(plugin, &process), CLAP_PROCESS_ERROR, "not active");
            assert!((p.activate.unwrap())(plugin, 48_000.0, 1, 16));
            let latency = (p.get_extension.unwrap())(plugin, CLAP_EXT_LATENCY.as_ptr()) as *const clap_plugin_latency;
            assert_eq!(((*latency).get.unwrap())(plugin), 5);
            assert_eq!((p.process.unwrap())(plugin, &process), CLAP_PROCESS_CONTINUE);
            assert!(left.iter().all(|&s| s == 1.0), "{left:?}");
            assert!(right.iter().all(|&s| s == 9.0), "{right:?}");
            assert_eq!(RESTARTS.load(Ordering::SeqCst), 1);

            let params = (p.get_extension.unwrap())(plugin, CLAP_EXT_PARAMS.as_ptr()) as *const clap_plugin_params;
            let mut text = [0 as c_char; 8];
            assert!(((*params).value_to_text.unwrap())(plugin, 7, 1.0, text.as_mut_ptr(), 8));
            assert_eq!(CStr::from_ptr(text.as_ptr()), c"a much ");
            assert!(!((*params).value_to_text.unwrap())(plugin, 8, 1.0, text.as_mut_ptr(), 8));
            (p.destroy.unwrap())(plugin);
        }
    }
}
