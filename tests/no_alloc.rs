//! Counts what the CLAP wrapper allocates on the audio thread while it applies the host's parameter
//! events, runs the plugin and sends the editor's changes to the host. The wrapper runs in this
//! process, since a loaded library has an allocator of its own.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::events::{CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE, clap_event_header, clap_event_param_value, clap_input_events, clap_output_events};
use clap_sys::process::{CLAP_PROCESS_CONTINUE, clap_process};
use gain::{FREQUENCY, GAIN, TEST_GESTURE};

mod clap_module;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn note() {
    if COUNTING.get() {
        ALLOCATIONS.set(ALLOCATIONS.get() + 1);
    }
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

static SENT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn take(_: *const clap_output_events, _: *const clap_event_header) -> bool {
    SENT.fetch_add(1, Ordering::SeqCst);
    true
}

unsafe extern "C" fn three(_: *const clap_input_events) -> u32 {
    3
}

unsafe extern "C" fn nth(list: *const clap_input_events, index: u32) -> *const clap_event_header {
    unsafe { &(*((*list).ctx as *const [clap_event_param_value; 3]))[index as usize].header }
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

unsafe extern "C" fn request(_: *const clap_sys::host::clap_host) {}

static HOST_PARAMS: clap_sys::ext::params::clap_host_params =
    clap_sys::ext::params::clap_host_params { rescan: None, clear: None, request_flush: Some(request) };
static HOST_STATE: clap_sys::ext::state::clap_host_state = clap_sys::ext::state::clap_host_state { mark_dirty: Some(request) };

unsafe extern "C" fn host_extension(_: *const clap_sys::host::clap_host, id: *const std::ffi::c_char) -> *const c_void {
    match unsafe { std::ffi::CStr::from_ptr(id) } {
        id if id == clap_sys::ext::params::CLAP_EXT_PARAMS => &HOST_PARAMS as *const _ as *const c_void,
        id if id == clap_sys::ext::state::CLAP_EXT_STATE => &HOST_STATE as *const _ as *const c_void,
        _ => ptr::null(),
    }
}

#[test]
fn processing_allocates_nothing_while_applying_and_sending() {
    let host = clap_module::host(Some(host_extension), Some(request));
    let entry = plugin_format::clap::entry::<gain::Gain>();
    unsafe {
        let plugin = clap_module::create(&entry, c"", &host);
        let p = &*plugin;
        assert!((p.activate.unwrap())(plugin, 48_000.0, 1, 64));
        assert!((p.start_processing.unwrap())(plugin));

        // The gesture parameter makes the plugin do what an editor does: begin, set, end and mark dirty.
        let list = [param_event(GAIN, -6.0), param_event(FREQUENCY, 440.0), param_event(TEST_GESTURE, 1.0)];
        let in_events = clap_input_events { ctx: &list as *const _ as *mut c_void, size: Some(three), get: Some(nth) };
        let out_events = clap_output_events { ctx: ptr::null_mut(), try_push: Some(take) };
        let mut input = vec![0.25f32; 64];
        let mut outputs = [vec![0.0f32; 64], vec![0.0f32; 64]];
        let mut in_ptrs = [input.as_mut_ptr()];
        let mut out_ptrs = [outputs[0].as_mut_ptr(), outputs[1].as_mut_ptr()];
        let in_buffer = clap_audio_buffer { data32: in_ptrs.as_mut_ptr(), data64: ptr::null_mut(), channel_count: 1, latency: 0, constant_mask: 0 };
        let mut out_buffer = clap_audio_buffer { data32: out_ptrs.as_mut_ptr(), data64: ptr::null_mut(), channel_count: 2, latency: 0, constant_mask: 0 };
        let process = clap_process {
            steady_time: 0,
            frames_count: 64,
            transport: ptr::null(),
            audio_inputs: &in_buffer,
            audio_outputs: &mut out_buffer,
            audio_inputs_count: 1,
            audio_outputs_count: 1,
            in_events: &in_events,
            out_events: &out_events,
        };

        assert_eq!((p.process.unwrap())(plugin, &process), CLAP_PROCESS_CONTINUE, "a first block, for what is set up once");
        let sent = SENT.load(Ordering::SeqCst);
        COUNTING.set(true);
        for _ in 0..200 {
            assert_eq!((p.process.unwrap())(plugin, &process), CLAP_PROCESS_CONTINUE);
        }
        COUNTING.set(false);
        assert_eq!(ALLOCATIONS.get(), 0, "allocations on the audio thread");
        assert_eq!(SENT.load(Ordering::SeqCst) - sent, 200 * 3, "the gesture of each block reached the host");

        (p.stop_processing.unwrap())(plugin);
        (p.deactivate.unwrap())(plugin);
        (p.destroy.unwrap())(plugin);
        (entry.deinit.unwrap())();
    }
}
