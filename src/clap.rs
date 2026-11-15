use std::cell::{Cell, UnsafeCell};
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[doc(hidden)]
pub use clap_sys as sys;

use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_VALUE, CLAP_TRANSPORT_HAS_SECONDS_TIMELINE,
    CLAP_TRANSPORT_IS_PLAYING, clap_event_header, clap_event_param_gesture, clap_event_param_value, clap_input_events, clap_output_events,
};
use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, CLAP_PORT_MONO, CLAP_PORT_STEREO, clap_audio_port_info, clap_plugin_audio_ports,
};
use clap_sys::ext::audio_ports_config::{CLAP_EXT_AUDIO_PORTS_CONFIG, clap_audio_ports_config, clap_plugin_audio_ports_config};
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_gui_resize_hints, clap_host_gui, clap_plugin_gui, clap_window};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_host_latency, clap_plugin_latency};
use clap_sys::ext::params::{
    CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_STEPPED, CLAP_PARAM_RESCAN_VALUES, clap_host_params, clap_param_info, clap_plugin_params,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state, clap_plugin_state};
use clap_sys::ext::track_info::{
    CLAP_EXT_TRACK_INFO, CLAP_EXT_TRACK_INFO_COMPAT, CLAP_TRACK_INFO_HAS_TRACK_COLOR, CLAP_TRACK_INFO_HAS_TRACK_NAME, clap_host_track_info,
    clap_plugin_track_info, clap_track_info,
};
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::fixedpoint::CLAP_SECTIME_FACTOR;
use clap_sys::host::clap_host;
use clap_sys::id::{CLAP_INVALID_ID, clap_id};
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR, clap_process, clap_process_status};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::{CLAP_VERSION, clap_version_is_compatible};

use crate::editor::{self, Parent};
use crate::gui_context::{Event, Wake};
use crate::{Extras, GuiContext, Kind, Layout, Plugin, Shared, TrackInfo, copy_str, guard, process_block};

thread_local! {
    /// Set while the wrapper applies the host's events on its own thread, which `send` empties the queue of right after.
    static APPLYING: Cell<bool> = const { Cell::new(false) };
}

/// Clears `APPLYING` again, also when a plugin panics in between.
struct Applying;

impl Applying {
    fn start() -> Self {
        APPLYING.set(true);
        Applying
    }
}

impl Drop for Applying {
    fn drop(&mut self) {
        APPLYING.set(false);
    }
}

fn same(a: *const c_char, b: &CStr) -> bool {
    !a.is_null() && unsafe { CStr::from_ptr(a) } == b
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
            let strings: Vec<CString> =
                [info.clap_id, info.name, info.vendor, info.version, info.description].iter().map(|s| CString::new(*s).unwrap_or_default()).collect();
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

/// A new editor for `plugin`, to draw into any `Ui` without a window, e.g. in tests: what it changes reaches the
/// host as from an editor in a window, through the instance's own `GuiContext`. `None` for a null `plugin` or one
/// another library made.
///
/// # Safety
///
/// `plugin` is null or a live plugin instance, and one this library made is an instance of `P`.
#[doc(hidden)]
pub unsafe fn editor<P: Plugin>(plugin: *const clap_plugin) -> Option<crate::Editor> {
    if !ptr::eq(unsafe { plugin.as_ref() }?.desc, descriptor::<P>()) {
        return None;
    }
    let instance = unsafe { Instance::<P>::from(plugin) }?;
    Some(instance.shared.editor(instance.cx.clone()))
}

unsafe extern "C" fn entry_init(_path: *const c_char) -> bool {
    true
}

unsafe extern "C" fn entry_deinit() {}

unsafe extern "C" fn get_factory<P: Plugin>(id: *const c_char) -> *const c_void {
    guard(ptr::null(), || if same(id, CLAP_PLUGIN_FACTORY_ID) { &Factory::<P>::FACTORY as *const clap_plugin_factory as *const c_void } else { ptr::null() })
}

struct Factory<P>(std::marker::PhantomData<P>);

impl<P: Plugin> Factory<P> {
    const FACTORY: clap_plugin_factory =
        clap_plugin_factory { get_plugin_count: Some(Self::count), get_plugin_descriptor: Some(Self::descriptor), create_plugin: Some(Self::create) };

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

/// `plugin` is used by the main thread only in `activate`, `deactivate` and `destroy`, which CLAP
/// never runs alongside processing; by the audio thread only in `process`, while active. Everything any thread may call goes through `shared`.
struct Instance<P: Plugin> {
    clap: clap_plugin,
    host: *const clap_host,
    plugin: UnsafeCell<P>,
    shared: Arc<P::Shared>,
    active: AtomicBool,
    inputs: AtomicU32,
    sample_rate: AtomicU64,
    /// What `activate` last reported. `latency.get` serves only this, since a host may call it from
    /// inside `host_latency.changed()` while `activate` still holds the processor.
    latency: AtomicU32,
    host_latency: AtomicPtr<clap_host_latency>,
    host_state: AtomicPtr<clap_host_state>,
    host_track_info: AtomicPtr<clap_host_track_info>,
    /// Its events go out from `process` or `flush`, its dirty mark from `on_main_thread`.
    cx: GuiContext,
    gui: Mutex<Gui>,
}

impl<P: Plugin> Instance<P> {
    fn create(host: *const clap_host, desc: &'static clap_plugin_descriptor) -> *const clap_plugin {
        let (plugin, shared) = P::new();
        let latency = plugin.latency();
        let weak: Weak<dyn Shared> = Arc::downgrade(&shared) as Weak<P::Shared>;
        let cx = GuiContext::new(weak);
        let gui = Mutex::new(Gui::new::<P>(&shared));
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
            plugin: UnsafeCell::new(plugin),
            shared,
            active: AtomicBool::new(false),
            inputs: AtomicU32::new(crate::default_inputs::<P>()),
            sample_rate: AtomicU64::new(0),
            latency: AtomicU32::new(latency),
            host_latency: AtomicPtr::new(ptr::null_mut()),
            host_state: AtomicPtr::new(ptr::null_mut()),
            host_track_info: AtomicPtr::new(ptr::null_mut()),
            cx,
            gui,
        }));
        unsafe {
            (*instance).clap.plugin_data = instance as *mut c_void;
            &(*instance).clap
        }
    }

    /// The instance behind `plugin`, or `None` for a null one or one without instance data.
    ///
    /// # Safety
    /// `plugin` is null or one this library made that has not been destroyed, and stays so for `'a`.
    unsafe fn from<'a>(plugin: *const clap_plugin) -> Option<&'a Self> {
        unsafe {
            if plugin.is_null() || (*plugin).plugin_data.is_null() {
                return None;
            }
            Some(&*((*plugin).plugin_data as *const Self))
        }
    }

    /// # Safety
    /// The caller has the exclusivity described on `Instance`: nothing else uses the plugin meanwhile.
    #[allow(clippy::mut_from_ref)]
    unsafe fn plugin_mut(&self) -> &mut P {
        unsafe { &mut *self.plugin.get() }
    }

    fn with<R>(plugin: *const clap_plugin, failed: impl FnOnce() -> R, body: impl FnOnce(&Self) -> R) -> R {
        catch_unwind(AssertUnwindSafe(|| unsafe { Self::from(plugin) }.map(body))).ok().flatten().unwrap_or_else(failed)
    }

    unsafe extern "C" fn init(plugin: *const clap_plugin) -> bool {
        Self::with(
            plugin,
            || false,
            |instance| {
                let latency = unsafe { instance.host_extension(CLAP_EXT_LATENCY) } as *mut clap_host_latency;
                instance.host_latency.store(latency, Ordering::Release);
                let state = unsafe { instance.host_extension(CLAP_EXT_STATE) } as *mut clap_host_state;
                instance.host_state.store(state, Ordering::Release);
                let params = unsafe { instance.host_extension(CLAP_EXT_PARAMS) } as *const clap_host_params;
                // Both host calls are thread-safe; `destroy` disconnects before the host may go away.
                let (host, request_flush) = (instance.host as usize, unsafe { params.as_ref() }.and_then(|p| p.request_flush));
                let request_callback = unsafe { instance.host.as_ref() }.and_then(|h| h.request_callback).filter(|_| !state.is_null());
                instance.cx.connect(Box::new(move |wake| {
                    let request = match wake {
                        Wake::Events => request_flush.filter(|_| !APPLYING.get()),
                        Wake::Dirty => request_callback,
                    };
                    if let Some(request) = request {
                        unsafe { request(host as *const clap_host) };
                    }
                }));
                instance.shared.attach(instance.cx.clone());
                let mut track = unsafe { instance.host_extension(CLAP_EXT_TRACK_INFO) };
                if track.is_null() {
                    track = unsafe { instance.host_extension(CLAP_EXT_TRACK_INFO_COMPAT) };
                }
                instance.host_track_info.store(track as *mut clap_host_track_info, Ordering::Release);
                instance.read_track();
                true
            },
        )
    }

    unsafe extern "C" fn destroy(plugin: *const clap_plugin) {
        guard((), || unsafe {
            if plugin.is_null() || (*plugin).plugin_data.is_null() {
                return;
            }
            let instance = Box::from_raw((*plugin).plugin_data as *mut Self);
            instance.cx.disconnect();
            if instance.active.load(Ordering::Acquire) {
                instance.plugin_mut().deactivate();
            }
        })
    }

    unsafe extern "C" fn activate(plugin: *const clap_plugin, sample_rate: f64, _min: u32, max: u32) -> bool {
        Self::with(
            plugin,
            || false,
            |instance| {
                if max == 0 || instance.active.load(Ordering::Acquire) {
                    return false;
                }
                instance.sample_rate.store(sample_rate.to_bits(), Ordering::Relaxed);
                let inputs = instance.inputs.load(Ordering::Relaxed) as usize;
                let latency = unsafe { instance.plugin_mut() }.activate(sample_rate, max as usize, inputs);
                let previous = instance.latency.swap(latency, Ordering::AcqRel);
                instance.active.store(true, Ordering::Release);
                if previous != latency
                    && let Some(host_latency) = unsafe { instance.host_latency.load(Ordering::Acquire).as_ref() }
                    && let Some(changed) = host_latency.changed
                {
                    unsafe { changed(instance.host) };
                }
                true
            },
        )
    }

    unsafe extern "C" fn deactivate(plugin: *const clap_plugin) {
        Self::with(
            plugin,
            || (),
            |instance| {
                if instance.active.swap(false, Ordering::AcqRel) {
                    unsafe { instance.plugin_mut() }.deactivate();
                }
            },
        )
    }

    unsafe extern "C" fn start_processing(plugin: *const clap_plugin) -> bool {
        Self::with(plugin, || false, |instance| instance.active.load(Ordering::Acquire))
    }

    unsafe extern "C" fn stop_processing(_: *const clap_plugin) {}

    unsafe extern "C" fn reset(_: *const clap_plugin) {}

    unsafe extern "C" fn on_main_thread(plugin: *const clap_plugin) {
        Self::with(
            plugin,
            || (),
            |instance| {
                if instance.cx.take_dirty()
                    && let Some(mark_dirty) = unsafe { instance.host_state.load(Ordering::Acquire).as_ref() }.and_then(|s| s.mark_dirty)
                {
                    unsafe { mark_dirty(instance.host) };
                }
            },
        )
    }

    unsafe extern "C" fn process(plugin: *const clap_plugin, process: *const clap_process) -> clap_process_status {
        Self::with(
            plugin,
            || CLAP_PROCESS_ERROR,
            |instance| unsafe {
                if process.is_null() || !instance.active.load(Ordering::Acquire) {
                    return CLAP_PROCESS_ERROR;
                }
                let process = &*process;
                instance.apply(process.in_events);
                let status = instance.run(process);
                instance.send(process.out_events);
                if instance.plugin_mut().take_latency_changed()
                    && let Some(host) = instance.host.as_ref()
                    && let Some(restart) = host.request_restart
                {
                    restart(instance.host);
                }
                status
            },
        )
    }

    /// # Safety
    /// Called from the audio thread of an active plugin, which has `plugin` to itself, and with the
    /// host's `process`: its buffers hold `frames_count` valid frames for the call.
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

            let mut ins = [ptr::null::<f32>(); 2];
            let mut inputs = 0;
            if process.audio_inputs_count > 0 && !process.audio_inputs.is_null() && !(*process.audio_inputs).data32.is_null() {
                let input = &*process.audio_inputs;
                inputs = (input.channel_count as usize).min(2);
                for (c, source) in ins.iter_mut().enumerate().take(inputs) {
                    *source = *input.data32.add(c);
                }
            }
            let configured = self.inputs.load(Ordering::Relaxed) as usize;
            let extras = Extras { sidechain: self.sidechain(process), position: self.position(process), playing: self.playing(process) };
            process_block(self.plugin_mut(), frames, &outs[..count], &ins[..inputs], configured, extras);
            CLAP_PROCESS_CONTINUE
        }
    }

    /// The second input port when the plugin has one and the host connected both its channels.
    ///
    /// # Safety
    /// `process.audio_inputs` holds `audio_inputs_count` valid buffers, each with `channel_count`
    /// pointers in `data32`.
    unsafe fn sidechain(&self, process: &clap_process) -> Option<[*const f32; 2]> {
        unsafe {
            if !P::INFO.sidechain || process.audio_inputs_count < 2 || process.audio_inputs.is_null() {
                return None;
            }
            let port = &*process.audio_inputs.add(1);
            if port.channel_count < 2 || port.data32.is_null() {
                return None;
            }
            let (left, right) = (*port.data32 as *const f32, *port.data32.add(1) as *const f32);
            (!left.is_null() && !right.is_null()).then_some([left, right])
        }
    }

    fn position(&self, process: &clap_process) -> Option<i64> {
        let transport = unsafe { process.transport.as_ref() }?;
        if transport.flags & CLAP_TRANSPORT_HAS_SECONDS_TIMELINE == 0 {
            return None;
        }
        let rate = f64::from_bits(self.sample_rate.load(Ordering::Relaxed));
        Some((transport.song_pos_seconds as f64 / CLAP_SECTIME_FACTOR as f64 * rate).round() as i64)
    }

    fn playing(&self, process: &clap_process) -> bool {
        unsafe { process.transport.as_ref() }.is_some_and(|transport| transport.flags & CLAP_TRANSPORT_IS_PLAYING != 0)
    }

    fn apply(&self, events: *const clap_input_events) {
        if events.is_null() {
            return;
        }
        let _applying = Applying::start();
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
                self.shared.set_param(event.param_id, event.value);
            }
        }
    }

    fn info(&self, id: clap_id) -> Option<&'static crate::ParamInfo> {
        self.shared.params().iter().find(|p| p.id == id)
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
            } else if same(id, CLAP_EXT_TRACK_INFO) || same(id, CLAP_EXT_TRACK_INFO_COMPAT) {
                &Self::TRACK_INFO as *const _ as _
            } else {
                ptr::null()
            };
            ext
        })
    }

    const TRACK_INFO: clap_plugin_track_info = clap_plugin_track_info { changed: Some(Self::track_changed) };

    unsafe extern "C" fn track_changed(plugin: *const clap_plugin) {
        Self::with(plugin, || (), |instance| instance.read_track())
    }

    fn read_track(&self) {
        let Some(host_track) = (unsafe { self.host_track_info.load(Ordering::Acquire).as_ref() }) else { return };
        let Some(get) = host_track.get else { return };
        let mut raw: clap_track_info = unsafe { std::mem::zeroed() };
        if !unsafe { get(self.host, &mut raw) } {
            return;
        }
        let name = (raw.flags & CLAP_TRACK_INFO_HAS_TRACK_NAME != 0).then(|| {
            let bytes: Vec<u8> = raw.name.iter().map(|&c| c as u8).take_while(|&c| c != 0).collect();
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let color = (raw.flags & CLAP_TRACK_INFO_HAS_TRACK_COLOR != 0).then_some([raw.color.red, raw.color.green, raw.color.blue]);
        self.cx.set_track(TrackInfo { name, color });
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
        Self::with(plugin, || 0, |instance| instance.shared.params().len() as u32)
    }

    unsafe extern "C" fn param_info(plugin: *const clap_plugin, index: u32, out: *mut clap_param_info) -> bool {
        Self::with(
            plugin,
            || false,
            |instance| {
                let Some(info) = instance.shared.params().get(index as usize) else { return false };
                if out.is_null() {
                    return false;
                }
                let mut flags = if matches!(info.kind, Kind::Float { .. }) { 0 } else { CLAP_PARAM_IS_STEPPED };
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
            },
        )
    }

    unsafe extern "C" fn param_value(plugin: *const clap_plugin, id: clap_id, out: *mut f64) -> bool {
        Self::with(
            plugin,
            || false,
            |instance| {
                if out.is_null() || instance.info(id).is_none() {
                    return false;
                }
                unsafe { out.write(instance.shared.param(id)) };
                true
            },
        )
    }

    unsafe extern "C" fn value_to_text(plugin: *const clap_plugin, id: clap_id, value: f64, out: *mut c_char, capacity: u32) -> bool {
        Self::with(
            plugin,
            || false,
            |instance| {
                if out.is_null() || capacity == 0 || instance.info(id).is_none() {
                    return false;
                }
                let out = unsafe { std::slice::from_raw_parts_mut(out, capacity as usize) };
                copy_str(&instance.shared.param_text(id, value), out);
                true
            },
        )
    }

    unsafe extern "C" fn text_to_value(plugin: *const clap_plugin, id: clap_id, text: *const c_char, out: *mut f64) -> bool {
        Self::with(
            plugin,
            || false,
            |instance| {
                if text.is_null() || out.is_null() || instance.info(id).is_none() {
                    return false;
                }
                let Ok(text) = unsafe { CStr::from_ptr(text) }.to_str() else { return false };
                let Some(value) = instance.shared.param_from_text(id, text) else { return false };
                unsafe { out.write(value) };
                true
            },
        )
    }

    unsafe extern "C" fn flush(plugin: *const clap_plugin, events: *const clap_input_events, out: *const clap_output_events) {
        Self::with(
            plugin,
            || (),
            |instance| {
                instance.apply(events);
                instance.send(out);
            },
        )
    }

    /// What the editor did, for as much as the host takes; the rest goes with the next call.
    fn send(&self, out: *const clap_output_events) {
        let Some(try_push) = (unsafe { out.as_ref() }).and_then(|o| o.try_push) else { return };
        let header = |type_, size: usize| clap_event_header { size: size as u32, time: 0, space_id: CLAP_CORE_EVENT_SPACE_ID, type_, flags: 0 };
        self.cx.drain(|event| unsafe {
            match event {
                Event::Begin(id) | Event::End(id) => {
                    let type_ = if matches!(event, Event::Begin(_)) { CLAP_EVENT_PARAM_GESTURE_BEGIN } else { CLAP_EVENT_PARAM_GESTURE_END };
                    let gesture = clap_event_param_gesture { header: header(type_, size_of::<clap_event_param_gesture>()), param_id: id };
                    try_push(out, &gesture.header)
                }
                Event::Value(id, value) => {
                    let change = clap_event_param_value {
                        header: header(CLAP_EVENT_PARAM_VALUE, size_of::<clap_event_param_value>()),
                        param_id: id,
                        cookie: ptr::null_mut(),
                        note_id: -1,
                        port_index: -1,
                        channel: -1,
                        key: -1,
                        value,
                    };
                    try_push(out, &change.header)
                }
            }
        });
    }

    const STATE: clap_plugin_state = clap_plugin_state { save: Some(Self::save), load: Some(Self::load) };

    unsafe extern "C" fn save(plugin: *const clap_plugin, stream: *const clap_ostream) -> bool {
        Self::with(
            plugin,
            || false,
            |instance| unsafe {
                let Some(write) = stream.as_ref().and_then(|s| s.write) else { return false };
                let bytes = instance.shared.save();
                let mut rest = &bytes[..];
                while !rest.is_empty() {
                    let written = write(stream, rest.as_ptr() as *const c_void, rest.len() as u64);
                    if written <= 0 {
                        return false;
                    }
                    rest = &rest[(written as usize).min(rest.len())..];
                }
                true
            },
        )
    }

    unsafe extern "C" fn load(plugin: *const clap_plugin, stream: *const clap_istream) -> bool {
        Self::with(
            plugin,
            || false,
            |instance| unsafe {
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
                if !instance.shared.load(&bytes) {
                    return false;
                }
                instance.rescan_values();
                true
            },
        )
    }

    /// Null when the host lacks the extension.
    ///
    /// # Safety
    /// `self.host` is the host this plugin was created with, which is still alive.
    unsafe fn host_extension(&self, id: &CStr) -> *const c_void {
        unsafe {
            match self.host.as_ref().and_then(|h| h.get_extension) {
                Some(get_extension) => get_extension(self.host, id.as_ptr()),
                None => ptr::null(),
            }
        }
    }

    fn rescan_values(&self) {
        unsafe {
            let params = self.host_extension(CLAP_EXT_PARAMS) as *const clap_host_params;
            if let Some(rescan) = params.as_ref().and_then(|p| p.rescan) {
                rescan(self.host, CLAP_PARAM_RESCAN_VALUES);
            }
        }
    }

    const LATENCY: clap_plugin_latency = clap_plugin_latency { get: Some(Self::latency) };

    unsafe extern "C" fn latency(plugin: *const clap_plugin) -> u32 {
        Self::with(plugin, || 0, |instance| instance.latency.load(Ordering::Acquire))
    }

    const AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports { count: Some(Self::port_count), get: Some(Self::port) };

    unsafe extern "C" fn port_count(_: *const clap_plugin, is_input: bool) -> u32 {
        1 + (is_input && P::INFO.sidechain) as u32
    }

    unsafe extern "C" fn port(plugin: *const clap_plugin, index: u32, is_input: bool, out: *mut clap_audio_port_info) -> bool {
        Self::with(
            plugin,
            || false,
            |instance| {
                if out.is_null() || index > (is_input && P::INFO.sidechain) as u32 {
                    return false;
                }
                if index == 1 {
                    let mut info = clap_audio_port_info {
                        id: 1,
                        name: [0; _],
                        flags: 0,
                        channel_count: 2,
                        port_type: CLAP_PORT_STEREO.as_ptr(),
                        in_place_pair: CLAP_INVALID_ID,
                    };
                    copy_str("Sidechain", &mut info.name);
                    unsafe { out.write(info) };
                    return true;
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
            },
        )
    }

    const AUDIO_PORTS_CONFIG: clap_plugin_audio_ports_config =
        clap_plugin_audio_ports_config { count: Some(Self::config_count), get: Some(Self::config), select: Some(Self::select) };

    unsafe extern "C" fn config_count(_: *const clap_plugin) -> u32 {
        P::LAYOUTS.len() as u32
    }

    unsafe extern "C" fn config(_: *const clap_plugin, index: u32, out: *mut clap_audio_ports_config) -> bool {
        guard(false, || {
            let Some(&Layout { name, main_inputs: inputs }) = P::LAYOUTS.get(index as usize) else { return false };
            if out.is_null() {
                return false;
            }
            let mut config = clap_audio_ports_config {
                id: index,
                name: [0; _],
                input_port_count: 1 + P::INFO.sidechain as u32,
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
        Self::with(
            plugin,
            || false,
            |instance| {
                let Some(&Layout { main_inputs: inputs, .. }) = P::LAYOUTS.get(id as usize) else { return false };
                if instance.active.load(Ordering::Acquire) {
                    return false;
                }
                instance.inputs.store(inputs, Ordering::Relaxed);
                true
            },
        )
    }

    const GUI: clap_plugin_gui = clap_plugin_gui {
        is_api_supported: Some(Self::gui_supported),
        get_preferred_api: Some(Self::gui_preferred_api),
        create: Some(Self::gui_create),
        destroy: Some(Self::gui_destroy),
        set_scale: Some(Self::gui_set_scale),
        get_size: Some(Self::gui_size),
        can_resize: Some(Self::gui_can_resize),
        get_resize_hints: Some(Self::gui_resize_hints),
        adjust_size: Some(Self::gui_adjust_size),
        set_size: Some(Self::gui_set_size),
        set_parent: Some(Self::gui_set_parent),
        set_transient: Some(Self::gui_set_transient),
        suggest_title: Some(Self::gui_suggest_title),
        show: Some(Self::gui_show),
        hide: Some(Self::gui_hide),
    };

    /// A call that overlaps another, against CLAP's main-thread rule, gets `failed` rather than wait.
    fn with_gui<R>(plugin: *const clap_plugin, failed: impl FnOnce() -> R, body: impl FnOnce(&mut Gui) -> R) -> R {
        Self::with(plugin, || None, |instance| instance.gui.try_lock().ok().map(|mut gui| body(&mut gui))).unwrap_or_else(failed)
    }

    unsafe extern "C" fn gui_supported(_: *const clap_plugin, api: *const c_char, floating: bool) -> bool {
        guard(false, || supported(api, floating))
    }

    unsafe extern "C" fn gui_preferred_api(_: *const clap_plugin, api: *mut *const c_char, floating: *mut bool) -> bool {
        guard(false, || {
            let Some(native) = NATIVE_API else { return false };
            if api.is_null() || floating.is_null() {
                return false;
            }
            unsafe {
                api.write(native.as_ptr());
                floating.write(false);
            }
            true
        })
    }

    unsafe extern "C" fn gui_create(plugin: *const clap_plugin, api: *const c_char, floating: bool) -> bool {
        Self::with(
            plugin,
            || false,
            |instance| {
                let Ok(mut gui) = instance.gui.try_lock() else { return false };
                if !supported(api, floating) {
                    return false;
                }
                instance.close(&mut gui);
                *gui = Gui { created: true, ..Gui::new::<P>(&instance.shared) };
                true
            },
        )
    }

    unsafe extern "C" fn gui_destroy(plugin: *const clap_plugin) {
        Self::with(
            plugin,
            || (),
            |instance| {
                let Ok(mut gui) = instance.gui.try_lock() else { return };
                instance.close(&mut gui);
                *gui = Gui::new::<P>(&instance.shared);
            },
        )
    }

    /// Drops the window first: on Linux that waits for the window's thread, which may be asking the
    /// host for a size.
    fn close(&self, gui: &mut Gui) {
        gui.window = None;
        self.cx.disconnect_resizers();
    }

    /// X11 and Win32 sizes are physical pixels, so the scale applies to both. Cocoa's are points, for
    /// which CLAP has the host not set one.
    unsafe extern "C" fn gui_set_scale(plugin: *const clap_plugin, scale: f64) -> bool {
        Self::with(
            plugin,
            || false,
            |instance| {
                if !editor::HOST_SCALE {
                    return false;
                }
                let Ok(gui) = instance.gui.try_lock() else { return false };
                let before = gui.geometry.physical();
                if !gui.geometry.set_scale(scale) {
                    return false;
                }
                let (width, height) = gui.geometry.physical();
                let open = gui.window.is_some();
                // The host may answer the request with `set_size` before it returns.
                drop(gui);
                if open
                    && (width, height) != before
                    && let Some(request_resize) = instance.host_request_resize()
                {
                    unsafe { request_resize(instance.host, width, height) };
                }
                true
            },
        )
    }

    fn host_request_resize(&self) -> Option<unsafe extern "C" fn(*const clap_host, u32, u32) -> bool> {
        let host_gui = unsafe { self.host_extension(CLAP_EXT_GUI) } as *const clap_host_gui;
        unsafe { host_gui.as_ref() }.and_then(|g| g.request_resize)
    }

    unsafe extern "C" fn gui_size(plugin: *const clap_plugin, width: *mut u32, height: *mut u32) -> bool {
        Self::with_gui(
            plugin,
            || false,
            |gui| {
                if width.is_null() || height.is_null() {
                    return false;
                }
                let size = gui.geometry.physical();
                unsafe {
                    width.write(size.0);
                    height.write(size.1);
                }
                true
            },
        )
    }

    unsafe extern "C" fn gui_can_resize(_: *const clap_plugin) -> bool {
        P::RESIZABLE
    }

    unsafe extern "C" fn gui_resize_hints(_: *const clap_plugin, hints: *mut clap_gui_resize_hints) -> bool {
        guard(false, || {
            if !P::RESIZABLE || hints.is_null() {
                return false;
            }
            let free = clap_gui_resize_hints {
                can_resize_horizontally: true,
                can_resize_vertically: true,
                preserve_aspect_ratio: false,
                aspect_ratio_width: 0,
                aspect_ratio_height: 0,
            };
            unsafe { hints.write(free) };
            true
        })
    }

    unsafe extern "C" fn gui_adjust_size(plugin: *const clap_plugin, width: *mut u32, height: *mut u32) -> bool {
        Self::with_gui(
            plugin,
            || false,
            |gui| {
                if width.is_null() || height.is_null() {
                    return false;
                }
                let size = gui.geometry.fit_physical(unsafe { (width.read(), height.read()) });
                unsafe {
                    width.write(size.0);
                    height.write(size.1);
                }
                true
            },
        )
    }

    /// Only a size `adjust_size` would leave as it is.
    unsafe extern "C" fn gui_set_size(plugin: *const clap_plugin, width: u32, height: u32) -> bool {
        Self::with_gui(
            plugin,
            || false,
            |gui| {
                if gui.geometry.fit_physical((width, height)) != (width, height) {
                    return false;
                }
                gui.geometry.resize_physical((width, height));
                true
            },
        )
    }

    unsafe extern "C" fn gui_set_parent(plugin: *const clap_plugin, window: *const clap_window) -> bool {
        Self::with(
            plugin,
            || false,
            |instance| {
                let Ok(mut gui) = instance.gui.try_lock() else { return false };
                let Some(window) = (unsafe { window.as_ref() }) else { return false };
                if !gui.created || !supported(window.api, false) {
                    return false;
                }
                #[cfg(target_os = "windows")]
                let parent = Parent::Win32(unsafe { window.specific.win32 });
                #[cfg(target_os = "macos")]
                let parent = Parent::AppKit(unsafe { window.specific.cocoa });
                #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                let parent = Parent::X11(unsafe { window.specific.x11 });
                instance.close(&mut gui);
                gui.window = editor::open(parent, gui.geometry.clone(), instance.shared.editor(instance.cx.clone()), Box::new(|| {}));
                if gui.window.is_some() && P::RESIZABLE {
                    instance.connect_resize(gui.geometry.clone());
                }
                gui.window.is_some()
            },
        )
    }

    /// The host's `request_resize` is thread-safe, so the editor asks it from its own thread.
    fn connect_resize(&self, geometry: editor::Geometry) {
        let Some(request_resize) = self.host_request_resize() else { return };
        // `destroy` disconnects before the host may go away.
        let host = self.host as usize;
        self.cx.connect_resize(Box::new(move |size| {
            let size = geometry.fit(size);
            let (width, height) = geometry.of(size);
            // The host may answer with `set_size` before it returns, but need not.
            if !unsafe { request_resize(host as *const clap_host, width, height) } {
                return false;
            }
            geometry.resize(size);
            true
        }));
    }

    /// Embedded only, so there is no floating window to keep above the host's.
    unsafe extern "C" fn gui_set_transient(_: *const clap_plugin, _: *const clap_window) -> bool {
        false
    }

    unsafe extern "C" fn gui_suggest_title(_: *const clap_plugin, _: *const c_char) {}

    unsafe extern "C" fn gui_show(plugin: *const clap_plugin) -> bool {
        Self::with_gui(plugin, || false, |gui| gui.window.as_ref().is_some_and(editor::Window::show))
    }

    unsafe extern "C" fn gui_hide(plugin: *const clap_plugin) -> bool {
        Self::with_gui(plugin, || false, |gui| gui.window.as_ref().is_some_and(editor::Window::hide))
    }
}

fn port_type(channels: u32) -> *const c_char {
    if channels == 1 { CLAP_PORT_MONO.as_ptr() } else { CLAP_PORT_STEREO.as_ptr() }
}

#[cfg(target_os = "linux")]
const NATIVE_API: Option<&CStr> = Some(clap_sys::ext::gui::CLAP_WINDOW_API_X11);
#[cfg(target_os = "windows")]
const NATIVE_API: Option<&CStr> = Some(clap_sys::ext::gui::CLAP_WINDOW_API_WIN32);
#[cfg(target_os = "macos")]
const NATIVE_API: Option<&CStr> = Some(clap_sys::ext::gui::CLAP_WINDOW_API_COCOA);
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
const NATIVE_API: Option<&CStr> = None;

fn supported(api: *const c_char, floating: bool) -> bool {
    !floating && NATIVE_API.is_some_and(|native| same(api, native))
}

/// Only the main thread uses it, as CLAP requires of every gui call.
struct Gui {
    created: bool,
    window: Option<editor::Window>,
    geometry: editor::Geometry,
}

impl Gui {
    fn new<P: Plugin>(shared: &Arc<P::Shared>) -> Self {
        Gui { created: false, window: None, geometry: editor::Geometry::new::<P>(shared) }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use clap_sys::audio_buffer::clap_audio_buffer;

    use super::*;
    use crate::{Block, Info, Kind, ParamInfo};

    struct Fixed;

    impl Shared for Fixed {
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
        fn editor(&self, _: crate::GuiContext) -> Box<dyn FnMut(&mut egui::Ui) + Send> {
            Box::new(|_| {})
        }
    }

    static RESTARTS: AtomicUsize = AtomicUsize::new(0);

    struct Doubler {
        changed: bool,
        latencies: &'static [u32],
        activations: usize,
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
            sidechain: false,
        };
        const EDITOR_SIZE: (u32, u32) = (1, 1);
        type Shared = Fixed;
        fn new() -> (Self, Arc<Fixed>) {
            (Doubler { changed: false, latencies: &[5], activations: 0 }, Arc::new(Fixed))
        }
        fn activate(&mut self, _: f64, _: usize, _: usize) -> u32 {
            let latency = self.latencies[self.activations.min(self.latencies.len() - 1)];
            self.activations += 1;
            latency
        }
        fn deactivate(&mut self) {}
        fn latency(&self) -> u32 {
            0
        }
        fn process(&mut self, block: Block<'_>) {
            for s in block.channels[0].iter_mut() {
                *s *= 2.0;
            }
            self.changed = true;
        }
        fn take_latency_changed(&mut self) -> bool {
            std::mem::take(&mut self.changed)
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

    struct Shifter(Doubler);

    impl Plugin for Shifter {
        const INFO: Info = Doubler::INFO;
        const EDITOR_SIZE: (u32, u32) = (1, 1);
        type Shared = Fixed;
        fn new() -> (Self, Arc<Fixed>) {
            (Shifter(Doubler { changed: false, latencies: &[5, 5, 9], activations: 0 }), Arc::new(Fixed))
        }
        fn activate(&mut self, rate: f64, block: usize, inputs: usize) -> u32 {
            self.0.activate(rate, block, inputs)
        }
        fn deactivate(&mut self) {}
        fn latency(&self) -> u32 {
            0
        }
        fn process(&mut self, block: Block<'_>) {
            self.0.process(block)
        }
        fn take_latency_changed(&mut self) -> bool {
            self.0.take_latency_changed()
        }
    }

    static SHIFTER: std::sync::atomic::AtomicPtr<clap_plugin> = std::sync::atomic::AtomicPtr::new(ptr::null_mut());
    static CHANGES: AtomicUsize = AtomicUsize::new(0);
    static SEEN: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn latency_changed(_: *const clap_host) {
        CHANGES.fetch_add(1, Ordering::SeqCst);
        let plugin = SHIFTER.load(Ordering::SeqCst);
        unsafe {
            let latency = ((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_LATENCY.as_ptr()) as *const clap_plugin_latency;
            SEEN.store(((*latency).get.unwrap())(plugin), Ordering::SeqCst);
        }
    }

    static HOST_LATENCY: clap_host_latency = clap_host_latency { changed: Some(latency_changed) };

    unsafe extern "C" fn host_extension(_: *const clap_host, id: *const c_char) -> *const c_void {
        if same(id, CLAP_EXT_LATENCY) { &HOST_LATENCY as *const _ as *const c_void } else { ptr::null() }
    }

    #[test]
    fn activate_tells_the_host_when_the_latency_differs_from_the_last_one() {
        let host = clap_host {
            clap_version: CLAP_VERSION,
            host_data: ptr::null_mut(),
            name: c"".as_ptr(),
            vendor: c"".as_ptr(),
            url: c"".as_ptr(),
            version: c"".as_ptr(),
            get_extension: Some(host_extension),
            request_restart: None,
            request_process: None,
            request_callback: None,
        };
        unsafe {
            let factory = &Factory::<Shifter>::FACTORY;
            let plugin = (factory.create_plugin.unwrap())(factory, &host, descriptor::<Shifter>().id);
            SHIFTER.store(plugin as *mut _, Ordering::SeqCst);
            let p = &*plugin;
            assert!((p.init.unwrap())(plugin));
            let latency = (p.get_extension.unwrap())(plugin, CLAP_EXT_LATENCY.as_ptr()) as *const clap_plugin_latency;
            assert_eq!(((*latency).get.unwrap())(plugin), 0);

            assert!((p.activate.unwrap())(plugin, 48_000.0, 1, 16));
            assert_eq!((CHANGES.load(Ordering::SeqCst), SEEN.load(Ordering::SeqCst)), (1, 5));
            (p.deactivate.unwrap())(plugin);
            assert_eq!(((*latency).get.unwrap())(plugin), 5, "inactive keeps the last reported latency");

            assert!((p.activate.unwrap())(plugin, 48_000.0, 1, 16));
            assert_eq!(CHANGES.load(Ordering::SeqCst), 1, "unchanged latency is not announced");
            (p.deactivate.unwrap())(plugin);

            assert!((p.activate.unwrap())(plugin, 48_000.0, 1, 16));
            assert_eq!((CHANGES.load(Ordering::SeqCst), SEEN.load(Ordering::SeqCst)), (2, 9));
            (p.destroy.unwrap())(plugin);
        }
    }

    /// Keeps the editor's size, as a plugin would in its state.
    struct Remembers(Mutex<Option<(u32, u32)>>);

    impl Shared for Remembers {
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
        fn editor(&self, _: crate::GuiContext) -> Box<dyn FnMut(&mut egui::Ui) + Send> {
            Box::new(|_| {})
        }
        fn editor_size(&self) -> Option<(u32, u32)> {
            *self.0.lock().unwrap()
        }
        fn set_editor_size(&self, width: u32, height: u32) {
            *self.0.lock().unwrap() = Some((width, height));
        }
    }

    struct Wide;

    impl Plugin for Wide {
        const INFO: Info = Doubler::INFO;
        const EDITOR_SIZE: (u32, u32) = (1180, 680);
        const RESIZABLE: bool = true;
        const EDITOR_MIN: (u32, u32) = (760, 440);
        type Shared = Remembers;
        fn new() -> (Self, Arc<Remembers>) {
            (Wide, Arc::new(Remembers(Mutex::new(Some((1000, 500))))))
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

    static ASKED: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());
    static REFUSE: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn request_resize(_: *const clap_host, width: u32, height: u32) -> bool {
        ASKED.lock().unwrap().push((width, height));
        !REFUSE.load(Ordering::SeqCst)
    }

    static HOST_GUI: clap_host_gui =
        clap_host_gui { resize_hints_changed: None, request_resize: Some(request_resize), request_show: None, request_hide: None, closed: None };

    unsafe extern "C" fn host_gui(_: *const clap_host, id: *const c_char) -> *const c_void {
        if same(id, CLAP_EXT_GUI) { &HOST_GUI as *const _ as *const c_void } else { ptr::null() }
    }

    fn sized_host() -> clap_host {
        clap_host {
            clap_version: CLAP_VERSION,
            host_data: ptr::null_mut(),
            name: c"".as_ptr(),
            vendor: c"".as_ptr(),
            url: c"".as_ptr(),
            version: c"".as_ptr(),
            get_extension: Some(host_gui),
            request_restart: None,
            request_process: None,
            request_callback: None,
        }
    }

    /// # Safety
    /// `plugin` is a live plugin.
    unsafe fn gui_size_of(gui: &clap_plugin_gui, plugin: *const clap_plugin) -> (u32, u32) {
        let (mut width, mut height) = (0, 0);
        assert!(unsafe { (gui.get_size.unwrap())(plugin, &mut width, &mut height) });
        (width, height)
    }

    #[cfg_attr(target_os = "macos", ignore = "macOS takes no scale from the host")]
    #[test]
    fn adjust_size_respects_the_minimum() {
        let host = sized_host();
        unsafe {
            let factory = &Factory::<Wide>::FACTORY;
            let plugin = (factory.create_plugin.unwrap())(factory, &host, descriptor::<Wide>().id);
            let p = &*plugin;
            assert!((p.init.unwrap())(plugin));
            let gui = &*((p.get_extension.unwrap())(plugin, CLAP_EXT_GUI.as_ptr()) as *const clap_plugin_gui);
            assert!((gui.create.unwrap())(plugin, NATIVE_API.unwrap().as_ptr(), false));
            assert!((gui.can_resize.unwrap())(plugin));
            let mut hints: clap_gui_resize_hints = std::mem::zeroed();
            assert!((gui.get_resize_hints.unwrap())(plugin, &mut hints));
            assert!(hints.can_resize_horizontally && hints.can_resize_vertically && !hints.preserve_aspect_ratio);
            assert_eq!(gui_size_of(gui, plugin), (1000, 500), "the size the plugin kept");

            let (mut width, mut height) = (10, 10);
            assert!((gui.adjust_size.unwrap())(plugin, &mut width, &mut height));
            assert_eq!((width, height), Wide::EDITOR_MIN);
            let (mut width, mut height) = (2000, 300);
            assert!((gui.adjust_size.unwrap())(plugin, &mut width, &mut height));
            assert_eq!((width, height), (2000, 440));

            assert!((gui.set_scale.unwrap())(plugin, 2.0));
            let (mut width, mut height) = (10, 10);
            assert!((gui.adjust_size.unwrap())(plugin, &mut width, &mut height));
            assert_eq!((width, height), (1520, 880), "at the host's scale");
            assert!(!(gui.set_size.unwrap())(plugin, 1520, 879));
            assert!((gui.set_size.unwrap())(plugin, 2000, 1200));
            assert_eq!(gui_size_of(gui, plugin), (2000, 1200));
            let instance = Instance::<Wide>::from(plugin).unwrap();
            assert_eq!(instance.shared.editor_size(), Some((1000, 600)), "kept at a scale of 1.0");

            (gui.destroy.unwrap())(plugin);
            assert!((gui.create.unwrap())(plugin, NATIVE_API.unwrap().as_ptr(), false));
            assert_eq!(gui_size_of(gui, plugin), (1000, 600), "a new gui opens at the size it was left at");
            (gui.destroy.unwrap())(plugin);
            (p.destroy.unwrap())(plugin);
        }
    }

    #[test]
    fn an_editor_without_a_window_is_the_instances_own() {
        let host = sized_host();
        unsafe {
            assert!(editor::<Wide>(ptr::null()).is_none());
            let factory = &Factory::<Wide>::FACTORY;
            let plugin = (factory.create_plugin.unwrap())(factory, &host, descriptor::<Wide>().id);
            let p = &*plugin;
            assert!((p.init.unwrap())(plugin));
            let mut drawn = editor::<Wide>(plugin).expect("an editor for the instance");
            egui::__run_test_ui(|ui| drawn(ui));
            let stranger = clap_plugin { desc: ptr::null(), ..*p };
            assert!(editor::<Wide>(&stranger).is_none(), "not this library's");
            (p.destroy.unwrap())(plugin);
        }
    }

    #[cfg_attr(target_os = "macos", ignore = "macOS takes no scale from the host")]
    #[test]
    fn the_editor_asks_the_host_for_a_size_no_smaller_than_its_minimum() {
        let host = sized_host();
        unsafe {
            let factory = &Factory::<Wide>::FACTORY;
            let plugin = (factory.create_plugin.unwrap())(factory, &host, descriptor::<Wide>().id);
            let p = &*plugin;
            assert!((p.init.unwrap())(plugin));
            let gui = &*((p.get_extension.unwrap())(plugin, CLAP_EXT_GUI.as_ptr()) as *const clap_plugin_gui);
            assert!((gui.create.unwrap())(plugin, NATIVE_API.unwrap().as_ptr(), false));
            assert!((gui.set_scale.unwrap())(plugin, 1.5));
            let instance = Instance::<Wide>::from(plugin).unwrap();
            let cx = instance.cx.clone();
            assert!(!cx.request_resize(900, 900), "nothing to resize without a window");

            // What `set_parent` does once the window is open, which needs a display.
            instance.connect_resize(instance.gui.lock().unwrap().geometry.clone());
            assert!(cx.request_resize(100, 2000));
            assert_eq!(*ASKED.lock().unwrap(), [(1140, 3000)]);
            assert_eq!(gui_size_of(gui, plugin), (1140, 3000));
            assert_eq!(instance.shared.editor_size(), Some((760, 2000)));

            REFUSE.store(true, Ordering::SeqCst);
            assert!(!cx.request_resize(900, 900));
            assert_eq!(gui_size_of(gui, plugin), (1140, 3000), "a refused size is not taken");

            (gui.destroy.unwrap())(plugin);
            assert!(!cx.request_resize(900, 900), "nothing to resize once the gui is gone");
            assert_eq!(ASKED.lock().unwrap().len(), 2);
            (p.destroy.unwrap())(plugin);
        }
    }

    struct StereoFirst(Doubler);

    impl Plugin for StereoFirst {
        const INFO: Info = Doubler::INFO;
        const EDITOR_SIZE: (u32, u32) = (1, 1);
        const LAYOUTS: &'static [Layout] = &[Layout { name: "Stereo", main_inputs: 2 }, Layout { name: "Mono in, stereo out", main_inputs: 1 }];
        type Shared = Fixed;
        fn new() -> (Self, Arc<Fixed>) {
            (StereoFirst(Doubler { changed: false, latencies: &[0], activations: 0 }), Arc::new(Fixed))
        }
        fn activate(&mut self, rate: f64, block: usize, inputs: usize) -> u32 {
            self.0.activate(rate, block, inputs)
        }
        fn deactivate(&mut self) {}
        fn latency(&self) -> u32 {
            0
        }
        fn process(&mut self, block: Block<'_>) {
            self.0.process(block)
        }
        fn take_latency_changed(&mut self) -> bool {
            self.0.take_latency_changed()
        }
    }

    /// # Safety
    /// `P`'s CLAP id is `test.doubler`, which the factory looks the plugin up by.
    unsafe fn main_input_channels<P: Plugin>() -> u32 {
        unsafe {
            let host = clap_host {
                clap_version: CLAP_VERSION,
                host_data: ptr::null_mut(),
                name: c"".as_ptr(),
                vendor: c"".as_ptr(),
                url: c"".as_ptr(),
                version: c"".as_ptr(),
                get_extension: None,
                request_restart: None,
                request_process: None,
                request_callback: None,
            };
            let factory = &Factory::<P>::FACTORY;
            let plugin = (factory.create_plugin.unwrap())(factory, &host, c"test.doubler".as_ptr());
            let ports = ((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_AUDIO_PORTS.as_ptr()) as *const clap_plugin_audio_ports;
            let mut info: clap_audio_port_info = std::mem::zeroed();
            assert!(((*ports).get.unwrap())(plugin, 0, true, &mut info));
            let channels = info.channel_count;
            ((*plugin).destroy.unwrap())(plugin);
            channels
        }
    }

    #[test]
    fn clap_default_layout_is_the_first() {
        unsafe {
            assert_eq!(main_input_channels::<StereoFirst>(), 2);
            assert_eq!(main_input_channels::<Doubler>(), 1);

            let configs = |index| {
                let mut config: clap_audio_ports_config = std::mem::zeroed();
                assert!(Instance::<StereoFirst>::config(ptr::null(), index, &mut config));
                (CStr::from_ptr(config.name.as_ptr()).to_str().unwrap().to_owned(), config.main_input_channel_count, config.input_port_count)
            };
            assert_eq!(Instance::<StereoFirst>::config_count(ptr::null()), 2);
            assert_eq!(configs(0), ("Stereo".to_owned(), 2, 1));
            assert_eq!(configs(1), ("Mono in, stereo out".to_owned(), 1, 1));
        }
    }
}
