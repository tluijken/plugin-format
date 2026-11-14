//! Loads the example plugin's built CLAP library the way a host does and runs audio through it.

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_VALUE, clap_event_header, clap_event_param_gesture,
    clap_event_param_value, clap_input_events, clap_output_events,
};
use clap_sys::ext::audio_ports::{CLAP_EXT_AUDIO_PORTS, clap_audio_port_info, clap_plugin_audio_ports};
use clap_sys::ext::audio_ports_config::{CLAP_EXT_AUDIO_PORTS_CONFIG, clap_audio_ports_config, clap_plugin_audio_ports_config};
use clap_sys::ext::gui::{self as gui_ext, CLAP_EXT_GUI, CLAP_WINDOW_API_WAYLAND, clap_plugin_gui, clap_window, clap_window_handle};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use clap_sys::ext::params::{CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_STEPPED, CLAP_PARAM_RESCAN_VALUES, clap_host_params, clap_param_info, clap_param_rescan_flags, clap_plugin_params};
use clap_sys::ext::track_info::{CLAP_EXT_TRACK_INFO, CLAP_TRACK_INFO_HAS_TRACK_COLOR, CLAP_TRACK_INFO_HAS_TRACK_NAME, clap_host_track_info, clap_plugin_track_info, clap_track_info};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state, clap_plugin_state};
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::clap_process;
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::CLAP_VERSION;
use gain::{FREQUENCY, GAIN, TEST_GESTURE};

mod built;

fn plugin() -> &'static built::Built {
    static PLUGIN: OnceLock<built::Built> = OnceLock::new();
    PLUGIN.get_or_init(|| built::plugin("gain", "clap"))
}

#[cfg(windows)]
const NATIVE_API: &CStr = gui_ext::CLAP_WINDOW_API_WIN32;
#[cfg(target_os = "macos")]
const NATIVE_API: &CStr = gui_ext::CLAP_WINDOW_API_COCOA;
#[cfg(not(any(windows, target_os = "macos")))]
const NATIVE_API: &CStr = gui_ext::CLAP_WINDOW_API_X11;
#[cfg(windows)]
const OTHER_API: &CStr = gui_ext::CLAP_WINDOW_API_X11;
#[cfg(not(windows))]
const OTHER_API: &CStr = gui_ext::CLAP_WINDOW_API_WIN32;

fn no_window() -> clap_window {
    #[cfg(windows)]
    let specific = clap_window_handle { win32: ptr::null_mut() };
    #[cfg(target_os = "macos")]
    let specific = clap_window_handle { cocoa: ptr::null_mut() };
    #[cfg(not(any(windows, target_os = "macos")))]
    let specific = clap_window_handle { x11: 0 };
    clap_window { api: NATIVE_API.as_ptr(), specific }
}

/// The entry's init and deinit apply to the whole library, so tests take turns.
static SERIAL: Mutex<()> = Mutex::new(());

fn turn() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn library() -> &'static libloading::Library {
    static LIBRARY: OnceLock<libloading::Library> = OnceLock::new();
    LIBRARY.get_or_init(|| unsafe { libloading::Library::new(&plugin().binary).unwrap() })
}

unsafe fn entry() -> &'static clap_plugin_entry {
    unsafe {
        let entry: libloading::Symbol<*const clap_plugin_entry> = library().get(b"clap_entry\0").unwrap();
        &**entry
    }
}

unsafe extern "C" fn nothing(_: *const clap_host) {}
unsafe extern "C" fn drop_event(_: *const clap_output_events, _: *const clap_event_header) -> bool {
    true
}

static VALUE_RESCANS: AtomicU32 = AtomicU32::new(0);

unsafe extern "C" fn rescan(_: *const clap_host, flags: clap_param_rescan_flags) {
    if flags & CLAP_PARAM_RESCAN_VALUES != 0 {
        VALUE_RESCANS.fetch_add(1, Ordering::SeqCst);
    }
}
unsafe extern "C" fn clear(_: *const clap_host, _: u32, _: u32) {}

static FLUSH_REQUESTS: AtomicU32 = AtomicU32::new(0);
static CALLBACK_REQUESTS: AtomicU32 = AtomicU32::new(0);
static DIRTY_MARKS: AtomicU32 = AtomicU32::new(0);

unsafe extern "C" fn request_flush(_: *const clap_host) {
    FLUSH_REQUESTS.fetch_add(1, Ordering::SeqCst);
}
unsafe extern "C" fn request_callback(_: *const clap_host) {
    CALLBACK_REQUESTS.fetch_add(1, Ordering::SeqCst);
}
unsafe extern "C" fn mark_dirty(_: *const clap_host) {
    DIRTY_MARKS.fetch_add(1, Ordering::SeqCst);
}

static HOST_PARAMS: clap_host_params = clap_host_params { rescan: Some(rescan), clear: Some(clear), request_flush: Some(request_flush) };
static HOST_STATE: clap_host_state = clap_host_state { mark_dirty: Some(mark_dirty) };

/// What the test host answers `clap_host_track_info::get` with: the flags, the name and the colour.
static TRACK: Mutex<(u64, &str, [u8; 3])> = Mutex::new((CLAP_TRACK_INFO_HAS_TRACK_NAME | CLAP_TRACK_INFO_HAS_TRACK_COLOR, "Bass", [255, 0, 0]));

unsafe extern "C" fn track_info(_: *const clap_host, out: *mut clap_track_info) -> bool {
    let (flags, name, [red, green, blue]) = *TRACK.lock().unwrap();
    let out = unsafe { &mut *out };
    out.flags = flags;
    for (slot, byte) in out.name.iter_mut().zip(name.bytes()) {
        *slot = byte as c_char;
    }
    out.color.red = red;
    out.color.green = green;
    out.color.blue = blue;
    true
}

static HOST_TRACK_INFO: clap_host_track_info = clap_host_track_info { get: Some(track_info) };

unsafe extern "C" fn host_extension(_: *const clap_host, id: *const c_char) -> *const c_void {
    match unsafe { CStr::from_ptr(id) } {
        id if id == CLAP_EXT_PARAMS => &HOST_PARAMS as *const _ as *const c_void,
        id if id == CLAP_EXT_STATE => &HOST_STATE as *const _ as *const c_void,
        id if id == CLAP_EXT_TRACK_INFO => &HOST_TRACK_INFO as *const _ as *const c_void,
        _ => ptr::null(),
    }
}

static HOST: clap_host = clap_host {
    clap_version: CLAP_VERSION,
    host_data: ptr::null_mut(),
    name: c"plugin-format test host".as_ptr(),
    vendor: c"".as_ptr(),
    url: c"".as_ptr(),
    version: c"0".as_ptr(),
    get_extension: Some(host_extension),
    request_restart: Some(nothing),
    request_process: Some(nothing),
    request_callback: Some(request_callback),
};

/// An initialised plugin instance from the shared library.
unsafe fn create() -> *const clap_plugin {
    unsafe {
        let entry = entry();
        let path = CString::new(plugin().path.to_str().unwrap()).unwrap();
        assert!((entry.init.unwrap())(path.as_ptr()));
        let factory = (entry.get_factory.unwrap())(CLAP_PLUGIN_FACTORY_ID.as_ptr()) as *const clap_plugin_factory;
        let desc = ((*factory).get_plugin_descriptor.unwrap())(factory, 0);
        let plugin = ((*factory).create_plugin.unwrap())(factory, &HOST, (*desc).id);
        assert!(!plugin.is_null());
        assert!(((*plugin).init.unwrap())(plugin));
        plugin
    }
}

unsafe fn extension<T>(plugin: *const clap_plugin, id: &CStr) -> &'static T {
    unsafe {
        let ext = ((*plugin).get_extension.unwrap())(plugin, id.as_ptr()) as *const T;
        assert!(!ext.is_null(), "{id:?} missing");
        &*ext
    }
}

unsafe fn listed<'a>(list: *const clap_input_events) -> &'a Vec<clap_event_param_value> {
    unsafe { &*((*list).ctx as *const Vec<clap_event_param_value>) }
}
unsafe extern "C" fn listed_events(list: *const clap_input_events) -> u32 {
    unsafe { listed(list).len() as u32 }
}
unsafe extern "C" fn listed_event(list: *const clap_input_events, index: u32) -> *const clap_event_header {
    unsafe { &listed(list)[index as usize].header }
}

fn param_event(param_id: u32, value: f64) -> clap_event_param_value {
    clap_event_param_value {
        header: clap_event_header {
            size: size_of::<clap_event_param_value>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        },
        param_id,
        cookie: ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value,
    }
}

fn events(list: &Vec<clap_event_param_value>) -> clap_input_events {
    clap_input_events { ctx: list as *const _ as *mut c_void, size: Some(listed_events), get: Some(listed_event) }
}

const OUT_EVENTS: clap_output_events = clap_output_events { ctx: ptr::null_mut(), try_push: Some(drop_event) };

/// What the plugin sent, as the host's output event list received it.
#[derive(Debug, PartialEq)]
enum Sent {
    Begin(u32),
    Value(u32, f64),
    End(u32),
}

unsafe extern "C" fn record_event(list: *const clap_output_events, header: *const clap_event_header) -> bool {
    unsafe {
        let sent = &mut *((*list).ctx as *mut Vec<Sent>);
        assert_eq!((*header).space_id, CLAP_CORE_EVENT_SPACE_ID);
        match (*header).type_ {
            CLAP_EVENT_PARAM_GESTURE_BEGIN => sent.push(Sent::Begin((*(header as *const clap_event_param_gesture)).param_id)),
            CLAP_EVENT_PARAM_GESTURE_END => sent.push(Sent::End((*(header as *const clap_event_param_gesture)).param_id)),
            CLAP_EVENT_PARAM_VALUE => {
                let event = &*(header as *const clap_event_param_value);
                sent.push(Sent::Value(event.param_id, event.value));
            }
            other => panic!("unexpected event type {other}"),
        }
        true
    }
}

unsafe extern "C" fn refuse_event(_: *const clap_output_events, _: *const clap_event_header) -> bool {
    false
}

fn recorder(sent: &mut Vec<Sent>) -> clap_output_events {
    clap_output_events { ctx: sent as *mut _ as *mut c_void, try_push: Some(record_event) }
}

/// Runs one block; `inputs` and `outputs` hold one slice per channel.
unsafe fn run(plugin: *const clap_plugin, inputs: &mut [Vec<f32>], outputs: &mut [Vec<f32>], frames: u32, in_events: &clap_input_events) {
    unsafe { run_with(plugin, inputs, outputs, frames, in_events, &OUT_EVENTS) }
}

unsafe fn run_with(plugin: *const clap_plugin, inputs: &mut [Vec<f32>], outputs: &mut [Vec<f32>], frames: u32, in_events: &clap_input_events, out_events: &clap_output_events) {
    unsafe {
        let mut in_ptrs: Vec<*mut f32> = inputs.iter_mut().map(|c| c.as_mut_ptr()).collect();
        let mut out_ptrs: Vec<*mut f32> = outputs.iter_mut().map(|c| c.as_mut_ptr()).collect();
        let input = clap_audio_buffer { data32: in_ptrs.as_mut_ptr(), data64: ptr::null_mut(), channel_count: inputs.len() as u32, latency: 0, constant_mask: 0 };
        let mut output = clap_audio_buffer { data32: out_ptrs.as_mut_ptr(), data64: ptr::null_mut(), channel_count: outputs.len() as u32, latency: 0, constant_mask: 0 };
        let process = clap_process {
            steady_time: 0,
            frames_count: frames,
            transport: ptr::null(),
            audio_inputs: &input,
            audio_outputs: &mut output,
            audio_inputs_count: 1,
            audio_outputs_count: 1,
            in_events,
            out_events,
        };
        ((*plugin).process.unwrap())(plugin, &process);
    }
}

fn db(gain: f32) -> f32 {
    10f32.powf(gain / 20.0)
}

#[test]
fn loads_scans_and_processes() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let p = &*plugin;
        let factory = (entry().get_factory.unwrap())(CLAP_PLUGIN_FACTORY_ID.as_ptr()) as *const clap_plugin_factory;
        assert_eq!(((*factory).get_plugin_count.unwrap())(factory), 1);
        let desc = ((*factory).get_plugin_descriptor.unwrap())(factory, 0);
        assert_eq!(CStr::from_ptr((*desc).id).to_str().unwrap(), "plugin-format.gain");

        let params: &clap_plugin_params = extension(plugin, CLAP_EXT_PARAMS);
        assert_eq!((params.count.unwrap())(plugin), 3);
        let mut info: clap_param_info = std::mem::zeroed();
        assert!((params.get_info.unwrap())(plugin, 0, &mut info));
        assert_eq!(info.id, GAIN);
        assert_eq!(CStr::from_ptr(info.name.as_ptr()).to_str().unwrap(), "Gain");
        assert_ne!(info.flags & CLAP_PARAM_IS_AUTOMATABLE, 0);
        assert_eq!((info.min_value, info.max_value, info.default_value), (-60.0, 24.0, 0.0));
        assert_ne!(info.flags & CLAP_PARAM_IS_STEPPED, 0);
        let mut frequency: clap_param_info = std::mem::zeroed();
        assert!((params.get_info.unwrap())(plugin, 1, &mut frequency));
        assert_eq!(frequency.id, FREQUENCY);
        assert_eq!(frequency.flags & CLAP_PARAM_IS_STEPPED, 0);
        assert_eq!((frequency.min_value, frequency.max_value, frequency.default_value), (20.0, 20000.0, 1000.0));
        let mut text = [0 as c_char; 32];
        assert!((params.value_to_text.unwrap())(plugin, FREQUENCY, 1000.0, text.as_mut_ptr(), 32));
        assert_eq!(CStr::from_ptr(text.as_ptr()).to_str().unwrap(), "1000 Hz");
        let mut parsed = 0.0;
        let hz = CString::new("1000 Hz").unwrap();
        assert!((params.text_to_value.unwrap())(plugin, FREQUENCY, hz.as_ptr(), &mut parsed));
        assert_eq!(parsed, 1000.0);
        let latency: &clap_plugin_latency = extension(plugin, CLAP_EXT_LATENCY);
        assert_eq!((latency.get.unwrap())(plugin), 0);

        assert!((p.activate.unwrap())(plugin, 48_000.0, 1, 128));
        assert!((p.start_processing.unwrap())(plugin));
        let none = Vec::new();
        let mut outputs = [vec![0.0; 128], vec![0.0; 128]];
        run(plugin, &mut [vec![0.25; 128]], &mut outputs, 128, &events(&none));
        assert!(outputs.iter().flatten().all(|&s| s == 0.25), "{:?}", &outputs[1][..4]);

        let list = vec![param_event(GAIN, -6.0)];
        run(plugin, &mut [vec![0.25; 128]], &mut outputs, 128, &events(&list));
        let want = 0.25 * db(-6.0);
        assert!(outputs.iter().flatten().all(|&s| (s - want).abs() < 1e-6), "{:?}", &outputs[1][..4]);
        let mut value = 0.0;
        assert!((params.get_value.unwrap())(plugin, GAIN, &mut value));
        assert_eq!(value, -6.0);

        (p.stop_processing.unwrap())(plugin);
        (p.deactivate.unwrap())(plugin);
        (p.destroy.unwrap())(plugin);
        (entry().deinit.unwrap())();
    }
}

#[test]
fn a_parameter_event_in_process_reaches_the_plugin() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let p = &*plugin;
        let params: &clap_plugin_params = extension(plugin, CLAP_EXT_PARAMS);
        assert!((p.activate.unwrap())(plugin, 48_000.0, 1, 64));
        assert!((p.start_processing.unwrap())(plugin));
        let list = vec![param_event(GAIN, -6.0)];
        run(plugin, &mut [vec![0.0; 64]], &mut [vec![0.0; 64], vec![0.0; 64]], 64, &events(&list));
        let mut value = 0.0;
        assert!((params.get_value.unwrap())(plugin, GAIN, &mut value));
        assert_eq!(value, -6.0);
        (p.stop_processing.unwrap())(plugin);
        (p.deactivate.unwrap())(plugin);
        (p.destroy.unwrap())(plugin);
        (entry().deinit.unwrap())();
    }
}

/// Takes at most 5 bytes per call, as a host's stream may.
unsafe extern "C" fn write_some(stream: *const clap_ostream, buffer: *const c_void, size: u64) -> i64 {
    unsafe {
        let out = &mut *((*stream).ctx as *mut Vec<u8>);
        let n = size.min(5) as usize;
        out.extend_from_slice(std::slice::from_raw_parts(buffer as *const u8, n));
        n as i64
    }
}

/// Hands out at most 7 bytes per call.
unsafe extern "C" fn read_some(stream: *const clap_istream, buffer: *mut c_void, size: u64) -> i64 {
    unsafe {
        let (bytes, at) = &mut *((*stream).ctx as *mut (Vec<u8>, usize));
        let n = (bytes.len() - *at).min(size as usize).min(7);
        ptr::copy_nonoverlapping(bytes[*at..].as_ptr(), buffer as *mut u8, n);
        *at += n;
        n as i64
    }
}

#[test]
fn state_saved_by_one_instance_loads_into_another() {
    let _turn = turn();
    unsafe {
        let a = create();
        let params: &clap_plugin_params = extension(a, CLAP_EXT_PARAMS);
        let state: &clap_plugin_state = extension(a, CLAP_EXT_STATE);
        let list = vec![param_event(GAIN, -12.0)];
        (params.flush.unwrap())(a, &events(&list), &OUT_EVENTS);
        let mut saved = Vec::<u8>::new();
        let out = clap_ostream { ctx: &mut saved as *mut _ as *mut c_void, write: Some(write_some) };
        assert!((state.save.unwrap())(a, &out));
        assert!(saved.len() > 5, "more than one write");

        let b = create();
        let mut source = (saved, 0usize);
        let input = clap_istream { ctx: &mut source as *mut _ as *mut c_void, read: Some(read_some) };
        let rescans = VALUE_RESCANS.load(Ordering::SeqCst);
        assert!((state.load.unwrap())(b, &input));
        assert_eq!(VALUE_RESCANS.load(Ordering::SeqCst), rescans + 1, "host told to rescan values");
        let mut value = f64::NAN;
        assert!((params.get_value.unwrap())(b, GAIN, &mut value));
        assert_eq!(value, -12.0);
        ((*a).destroy.unwrap())(a);
        ((*b).destroy.unwrap())(b);
        (entry().deinit.unwrap())();
        (entry().deinit.unwrap())();
    }
}

#[test]
fn stereo_config_keeps_left_and_right_apart() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let p = &*plugin;
        let configs: &clap_plugin_audio_ports_config = extension(plugin, CLAP_EXT_AUDIO_PORTS_CONFIG);
        assert_eq!((configs.count.unwrap())(plugin), 2);
        for (index, channels) in [(0, 1), (1, 2)] {
            let mut config: clap_audio_ports_config = std::mem::zeroed();
            assert!((configs.get.unwrap())(plugin, index, &mut config));
            assert_eq!((config.id, config.main_input_channel_count, config.main_output_channel_count), (index, channels, 2));
        }
        assert!((configs.select.unwrap())(plugin, 1));
        let ports: &clap_plugin_audio_ports = extension(plugin, CLAP_EXT_AUDIO_PORTS);
        let mut port: clap_audio_port_info = std::mem::zeroed();
        assert!((ports.get.unwrap())(plugin, 0, true, &mut port));
        assert_eq!(port.channel_count, 2);

        assert!((p.activate.unwrap())(plugin, 48_000.0, 1, 128));
        assert!(!(configs.select.unwrap())(plugin, 0), "select while active");
        assert!((p.start_processing.unwrap())(plugin));
        let mut outputs = [vec![0.0; 128], vec![0.0; 128]];
        let none = Vec::new();
        run(plugin, &mut [vec![0.25; 128], vec![0.5; 128]], &mut outputs, 128, &events(&none));
        assert!(outputs[0].iter().all(|&s| s == 0.25), "{:?}", &outputs[0][..4]);
        assert!(outputs[1].iter().all(|&s| s == 0.5), "{:?}", &outputs[1][..4]);
        (p.stop_processing.unwrap())(plugin);
        (p.deactivate.unwrap())(plugin);
        (p.destroy.unwrap())(plugin);
        (entry().deinit.unwrap())();
    }
}

#[test]
fn zero_frames_and_destroy_without_deactivate_are_survived() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let p = &*plugin;
        assert!((p.activate.unwrap())(plugin, 48_000.0, 1, 128));
        assert!((p.start_processing.unwrap())(plugin));
        let none = Vec::new();
        run(plugin, &mut [vec![]], &mut [vec![], vec![]], 0, &events(&none));
        (p.destroy.unwrap())(plugin);

        let plugin = create();
        let p = &*plugin;
        assert!((p.activate.unwrap())(plugin, 48_000.0, 1, 128));
        assert!((p.start_processing.unwrap())(plugin));
        let mut outputs = [vec![0.0; 128], vec![0.0; 128]];
        run(plugin, &mut [vec![0.25; 128]], &mut outputs, 128, &events(&none));
        assert!(outputs.iter().flatten().all(|&s| s == 0.25));
        (p.stop_processing.unwrap())(plugin);
        (p.deactivate.unwrap())(plugin);
        (p.destroy.unwrap())(plugin);
        (entry().deinit.unwrap())();
        (entry().deinit.unwrap())();
    }
}

#[test]
fn activation_without_room_for_a_single_frame_is_refused() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let p = &*plugin;
        assert!(!(p.activate.unwrap())(plugin, 48_000.0, 0, 0));
        assert!((p.activate.unwrap())(plugin, 48_000.0, 1, 128));
        (p.deactivate.unwrap())(plugin);
        (p.destroy.unwrap())(plugin);
        (entry().deinit.unwrap())();
    }
}

#[test]
fn a_non_finite_parameter_value_leaves_a_loadable_state() {
    let _turn = turn();
    unsafe {
        let a = create();
        let p = &*a;
        let params: &clap_plugin_params = extension(a, CLAP_EXT_PARAMS);
        let state: &clap_plugin_state = extension(a, CLAP_EXT_STATE);
        assert!((p.activate.unwrap())(a, 48_000.0, 1, 64));
        assert!((p.start_processing.unwrap())(a));
        let list = vec![param_event(GAIN, 6.0)];
        (params.flush.unwrap())(a, &events(&list), &OUT_EVENTS);
        let list = vec![param_event(GAIN, f64::NAN)];
        run(a, &mut [vec![0.0; 64]], &mut [vec![0.0; 64], vec![0.0; 64]], 64, &events(&list));
        (p.stop_processing.unwrap())(a);
        (p.deactivate.unwrap())(a);
        let list = vec![param_event(GAIN, 6.0), param_event(GAIN, f64::NEG_INFINITY)];
        (params.flush.unwrap())(a, &events(&list), &OUT_EVENTS);

        let mut saved = Vec::<u8>::new();
        let out = clap_ostream { ctx: &mut saved as *mut _ as *mut c_void, write: Some(write_some) };
        assert!((state.save.unwrap())(a, &out));
        let b = create();
        let mut source = (saved, 0usize);
        let input = clap_istream { ctx: &mut source as *mut _ as *mut c_void, read: Some(read_some) };
        assert!((state.load.unwrap())(b, &input), "state after a NaN must load");
        let mut value = f64::NAN;
        assert!((params.get_value.unwrap())(b, GAIN, &mut value));
        assert_eq!(value, 0.0, "a non-finite value becomes the default");
        ((*a).destroy.unwrap())(a);
        ((*b).destroy.unwrap())(b);
        (entry().deinit.unwrap())();
        (entry().deinit.unwrap())();
    }
}

/// Opening a real window needs a display, so this stops short of a parent the plugin would embed in.
#[test]
fn the_gui_offers_an_embedded_native_window_of_fixed_size() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let gui: &clap_plugin_gui = extension(plugin, CLAP_EXT_GUI);
        let supported = gui.is_api_supported.unwrap();
        assert!(supported(plugin, NATIVE_API.as_ptr(), false));
        assert!(!supported(plugin, NATIVE_API.as_ptr(), true), "embedded only");
        assert!(!supported(plugin, OTHER_API.as_ptr(), false), "only the platform's own window system");
        assert!(!supported(plugin, CLAP_WINDOW_API_WAYLAND.as_ptr(), false));
        assert!(!supported(plugin, ptr::null(), false));
        let (mut api, mut floating) = (ptr::null(), true);
        assert!((gui.get_preferred_api.unwrap())(plugin, &mut api, &mut floating));
        assert_eq!((CStr::from_ptr(api), floating), (NATIVE_API, false));

        assert!(!(gui.create.unwrap())(plugin, NATIVE_API.as_ptr(), true));
        assert!((gui.create.unwrap())(plugin, NATIVE_API.as_ptr(), false));
        let (mut width, mut height) = (0, 0);
        assert!((gui.get_size.unwrap())(plugin, &mut width, &mut height));
        assert_eq!((width, height), (200, 100));
        assert!(!(gui.can_resize.unwrap())(plugin));
        assert!(!(gui.set_size.unwrap())(plugin, 400, 300));
        assert!((gui.set_size.unwrap())(plugin, 200, 100));
        let wayland = clap_window { api: CLAP_WINDOW_API_WAYLAND.as_ptr(), specific: clap_window_handle { ptr: ptr::null_mut() } };
        assert!(!(gui.set_parent.unwrap())(plugin, &wayland));
        let no_window = no_window();
        assert!(!(gui.set_parent.unwrap())(plugin, &no_window));
        assert!(!(gui.set_parent.unwrap())(plugin, ptr::null()));
        assert!(!(gui.set_transient.unwrap())(plugin, &no_window));
        assert!(!(gui.show.unwrap())(plugin), "nothing to show without a parent");
        (gui.destroy.unwrap())(plugin);
        (gui.destroy.unwrap())(plugin);

        assert!((gui.create.unwrap())(plugin, NATIVE_API.as_ptr(), false));
        ((*plugin).destroy.unwrap())(plugin);
        (entry().deinit.unwrap())();
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn the_gui_takes_the_hosts_scale_and_reports_its_size_in_it() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let gui: &clap_plugin_gui = extension(plugin, CLAP_EXT_GUI);
        let size = || {
            let (mut width, mut height) = (0, 0);
            assert!((gui.get_size.unwrap())(plugin, &mut width, &mut height));
            (width, height)
        };
        assert!((gui.create.unwrap())(plugin, NATIVE_API.as_ptr(), false));
        assert!((gui.set_scale.unwrap())(plugin, 2.0));
        assert_eq!(size(), (400, 200));
        assert!(!(gui.set_size.unwrap())(plugin, 200, 100));
        assert!((gui.set_size.unwrap())(plugin, 400, 200));
        assert!((gui.set_scale.unwrap())(plugin, 1.5));
        assert_eq!(size(), (300, 150));
        assert!(!(gui.set_scale.unwrap())(plugin, 0.0));
        assert!(!(gui.set_scale.unwrap())(plugin, f64::NAN));
        assert_eq!(size(), (300, 150), "a scale that cannot be applied is ignored");

        (gui.destroy.unwrap())(plugin);
        assert!((gui.create.unwrap())(plugin, NATIVE_API.as_ptr(), false));
        assert_eq!(size(), (200, 100), "a new gui starts at 1.0 until the host says otherwise");
        ((*plugin).destroy.unwrap())(plugin);
        (entry().deinit.unwrap())();
    }
}

#[cfg(target_os = "macos")]
#[test]
fn the_gui_measures_in_points_and_takes_no_scale_from_the_host() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let gui: &clap_plugin_gui = extension(plugin, CLAP_EXT_GUI);
        assert!((gui.create.unwrap())(plugin, NATIVE_API.as_ptr(), false));
        assert!(!(gui.set_scale.unwrap())(plugin, 2.0));
        let (mut width, mut height) = (0, 0);
        assert!((gui.get_size.unwrap())(plugin, &mut width, &mut height));
        assert_eq!((width, height), (200, 100));
        ((*plugin).destroy.unwrap())(plugin);
        (entry().deinit.unwrap())();
    }
}

#[test]
fn gesture_reaches_the_clap_host_in_order() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let params: &clap_plugin_params = extension(plugin, CLAP_EXT_PARAMS);
        let mut info: clap_param_info = std::mem::zeroed();
        assert!((params.get_info.unwrap())(plugin, 2, &mut info));
        assert_eq!((info.id, info.flags & CLAP_PARAM_IS_AUTOMATABLE), (TEST_GESTURE, 0));

        let requests = FLUSH_REQUESTS.load(Ordering::SeqCst);
        let mut sent = Vec::new();
        let trigger = vec![param_event(TEST_GESTURE, 1.0)];
        (params.flush.unwrap())(plugin, &events(&trigger), &recorder(&mut sent));
        assert_eq!(FLUSH_REQUESTS.load(Ordering::SeqCst), requests, "no flush request from inside a flush, which sends the events itself");
        let none = Vec::new();
        (params.flush.unwrap())(plugin, &events(&none), &recorder(&mut sent));
        assert_eq!(sent, [Sent::Begin(GAIN), Sent::Value(GAIN, -6.0), Sent::End(GAIN)]);
        let mut value = 0.0;
        assert!((params.get_value.unwrap())(plugin, GAIN, &mut value));
        assert_eq!(value, -6.0, "the shared value changes at once");
        ((*plugin).destroy.unwrap())(plugin);
        (entry().deinit.unwrap())();
    }
}

#[test]
fn a_gesture_the_host_refused_goes_out_with_the_next_block() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let p = &*plugin;
        let params: &clap_plugin_params = extension(plugin, CLAP_EXT_PARAMS);
        let trigger = vec![param_event(TEST_GESTURE, 1.0)];
        let full = clap_output_events { ctx: ptr::null_mut(), try_push: Some(refuse_event) };
        (params.flush.unwrap())(plugin, &events(&trigger), &full);

        assert!((p.activate.unwrap())(plugin, 48_000.0, 1, 64));
        assert!((p.start_processing.unwrap())(plugin));
        let mut sent = Vec::new();
        let none = Vec::new();
        run_with(plugin, &mut [vec![0.0; 64]], &mut [vec![0.0; 64], vec![0.0; 64]], 64, &events(&none), &recorder(&mut sent));
        assert_eq!(sent, [Sent::Begin(GAIN), Sent::Value(GAIN, -6.0), Sent::End(GAIN)]);
        run_with(plugin, &mut [vec![0.0; 64]], &mut [vec![0.0; 64], vec![0.0; 64]], 64, &events(&none), &recorder(&mut sent));
        assert_eq!(sent.len(), 3, "sent once");
        (p.stop_processing.unwrap())(plugin);
        (p.deactivate.unwrap())(plugin);
        (p.destroy.unwrap())(plugin);
        (entry().deinit.unwrap())();
    }
}

#[test]
fn mark_dirty_reaches_the_host() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let params: &clap_plugin_params = extension(plugin, CLAP_EXT_PARAMS);
        let (callbacks, marks) = (CALLBACK_REQUESTS.load(Ordering::SeqCst), DIRTY_MARKS.load(Ordering::SeqCst));
        let trigger = vec![param_event(TEST_GESTURE, 1.0)];
        (params.flush.unwrap())(plugin, &events(&trigger), &OUT_EVENTS);
        assert!(CALLBACK_REQUESTS.load(Ordering::SeqCst) > callbacks, "the host is asked for a main-thread callback");
        assert_eq!(DIRTY_MARKS.load(Ordering::SeqCst), marks, "marked from that callback only");
        ((*plugin).on_main_thread.unwrap())(plugin);
        assert_eq!(DIRTY_MARKS.load(Ordering::SeqCst), marks + 1);
        ((*plugin).on_main_thread.unwrap())(plugin);
        assert_eq!(DIRTY_MARKS.load(Ordering::SeqCst), marks + 1, "once per mark");
        ((*plugin).destroy.unwrap())(plugin);
        (entry().deinit.unwrap())();
    }
}

fn saved_state(plugin: *const clap_plugin) -> String {
    unsafe {
        let state: &clap_plugin_state = extension(plugin, CLAP_EXT_STATE);
        let mut saved = Vec::<u8>::new();
        let out = clap_ostream { ctx: &mut saved as *mut _ as *mut c_void, write: Some(write_some) };
        assert!((state.save.unwrap())(plugin, &out));
        String::from_utf8(saved).unwrap()
    }
}

#[test]
fn clap_track_info_reaches_the_plugin() {
    let _turn = turn();
    unsafe {
        *TRACK.lock().unwrap() = (CLAP_TRACK_INFO_HAS_TRACK_NAME | CLAP_TRACK_INFO_HAS_TRACK_COLOR, "Bass", [255, 0, 0]);
        let plugin = create();
        assert!(saved_state(plugin).ends_with(";name=Bass;color=ff0000"), "read on init");

        let track: &clap_plugin_track_info = extension(plugin, CLAP_EXT_TRACK_INFO);
        *TRACK.lock().unwrap() = (CLAP_TRACK_INFO_HAS_TRACK_NAME | CLAP_TRACK_INFO_HAS_TRACK_COLOR, "Keys", [0, 128, 255]);
        assert!(saved_state(plugin).ends_with(";name=Bass;color=ff0000"), "not before the host says it changed");
        (track.changed.unwrap())(plugin);
        assert!(saved_state(plugin).ends_with(";name=Keys;color=0080ff"));

        *TRACK.lock().unwrap() = (CLAP_TRACK_INFO_HAS_TRACK_NAME, "Keys", [9, 9, 9]);
        (track.changed.unwrap())(plugin);
        assert!(saved_state(plugin).ends_with(";name=Keys"), "a colour only when the host has one");

        *TRACK.lock().unwrap() = (CLAP_TRACK_INFO_HAS_TRACK_COLOR, "ignored", [1, 2, 3]);
        (track.changed.unwrap())(plugin);
        assert!(saved_state(plugin).ends_with(";color=010203"), "a name only when the host has one");
        ((*plugin).destroy.unwrap())(plugin);
        (entry().deinit.unwrap())();
    }
}
