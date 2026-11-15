//! Loads the sidechain example's CLAP library and checks what a host gives the plugin besides the main input.

use std::ffi::CStr;
use std::ptr;
use std::sync::{Mutex, MutexGuard, OnceLock};

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, CLAP_TRANSPORT_IS_PLAYING, clap_event_transport};
use clap_sys::ext::audio_ports::{CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, clap_audio_port_info, clap_plugin_audio_ports};
use clap_sys::ext::audio_ports_config::{CLAP_EXT_AUDIO_PORTS_CONFIG, clap_audio_ports_config, clap_plugin_audio_ports_config};
use clap_sys::ext::params::{CLAP_EXT_PARAMS, clap_plugin_params};
use clap_sys::fixedpoint::CLAP_SECTIME_FACTOR;
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{CLAP_PROCESS_CONTINUE, clap_process};
use sidechain::POSITION;

mod built;
mod clap_module;

fn plugin() -> &'static built::Built {
    static PLUGIN: OnceLock<built::Built> = OnceLock::new();
    PLUGIN.get_or_init(|| built::plugin("sidechain", "clap"))
}

static SERIAL: Mutex<()> = Mutex::new(());

fn turn() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

unsafe extern "C" fn nothing(_: *const clap_host) {}

static HOST: clap_host = clap_module::host(None, Some(nothing));

unsafe fn entry() -> &'static clap_plugin_entry {
    unsafe { clap_module::load(&plugin().binary) }
}

/// An initialised plugin set to the stereo main input, active with room for 128 frames.
unsafe fn active() -> *const clap_plugin {
    unsafe {
        let plugin = clap_module::create_at(entry(), &plugin().path, &HOST);
        assert_eq!(CStr::from_ptr((*(*plugin).desc).id), c"plugin-format.sidechain");
        let configs = ((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_AUDIO_PORTS_CONFIG.as_ptr()) as *const clap_plugin_audio_ports_config;
        assert!(((*configs).select.unwrap())(plugin, 1));
        assert!(((*plugin).activate.unwrap())(plugin, 48_000.0, 1, 128));
        assert!(((*plugin).start_processing.unwrap())(plugin));
        plugin
    }
}

unsafe fn finish(plugin: *const clap_plugin) {
    unsafe {
        ((*plugin).stop_processing.unwrap())(plugin);
        ((*plugin).deactivate.unwrap())(plugin);
        ((*plugin).destroy.unwrap())(plugin);
    }
}

fn buffer(pointers: &mut [*mut f32], channels: u32) -> clap_audio_buffer {
    clap_audio_buffer { data32: pointers.as_mut_ptr(), data64: ptr::null_mut(), channel_count: channels, latency: 0, constant_mask: 0 }
}

/// One block of `frames` frames at `main` on both channels; `side` is the sidechain port's value, if connected.
/// Returns the first left output sample, or `None` for a zero-frame block.
unsafe fn play(plugin: *const clap_plugin, frames: u32, main: f32, side: Option<f32>, transport: *const clap_event_transport) -> Option<f32> {
    unsafe {
        let len = frames as usize;
        let mut main_in = [vec![main; len], vec![main; len]];
        let mut side_in = [vec![side.unwrap_or(0.0); len], vec![side.unwrap_or(0.0); len]];
        let mut out = [vec![0.0f32; len], vec![0.0f32; len]];
        let mut main_ptrs = [main_in[0].as_mut_ptr(), main_in[1].as_mut_ptr()];
        let mut side_ptrs = [side_in[0].as_mut_ptr(), side_in[1].as_mut_ptr()];
        let mut out_ptrs = [out[0].as_mut_ptr(), out[1].as_mut_ptr()];
        let inputs = [buffer(&mut main_ptrs, 2), if side.is_some() { buffer(&mut side_ptrs, 2) } else { buffer(&mut [], 0) }];
        let mut output = buffer(&mut out_ptrs, 2);
        let process = clap_process {
            steady_time: 0,
            frames_count: frames,
            transport,
            audio_inputs: inputs.as_ptr(),
            audio_outputs: &mut output,
            audio_inputs_count: 2,
            audio_outputs_count: 1,
            in_events: ptr::null(),
            out_events: ptr::null(),
        };
        ((*plugin).process.unwrap())(plugin, &process);
        out[0].first().copied()
    }
}

unsafe fn position(plugin: *const clap_plugin) -> f64 {
    unsafe {
        let params = ((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_PARAMS.as_ptr()) as *const clap_plugin_params;
        let mut value = 0.0;
        assert!(((*params).get_value.unwrap())(plugin, POSITION, &mut value));
        value
    }
}

fn transport(flags: u32, seconds: f64) -> clap_event_transport {
    let mut transport: clap_event_transport = unsafe { std::mem::zeroed() };
    transport.flags = flags;
    transport.song_pos_seconds = (seconds * CLAP_SECTIME_FACTOR as f64) as i64;
    transport
}

#[test]
fn the_sidechain_is_a_second_optional_input_port() {
    let _turn = turn();
    unsafe {
        let plugin = active();
        let ports = &*(((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_AUDIO_PORTS.as_ptr()) as *const clap_plugin_audio_ports);
        assert_eq!((ports.count.unwrap())(plugin, true), 2);
        assert_eq!((ports.count.unwrap())(plugin, false), 1);
        let configs = &*(((*plugin).get_extension.unwrap())(plugin, CLAP_EXT_AUDIO_PORTS_CONFIG.as_ptr()) as *const clap_plugin_audio_ports_config);
        let mut config: clap_audio_ports_config = std::mem::zeroed();
        assert!((configs.get.unwrap())(plugin, 1, &mut config));
        assert_eq!((config.input_port_count, config.output_port_count), (2, 1));
        let mut port: clap_audio_port_info = std::mem::zeroed();
        assert!((ports.get.unwrap())(plugin, 1, true, &mut port));
        assert_eq!((CStr::from_ptr(port.name.as_ptr()), port.channel_count, port.flags & CLAP_AUDIO_PORT_IS_MAIN), (c"Sidechain", 2, 0));
        assert!(!(ports.get.unwrap())(plugin, 1, false, &mut port));
        assert!(!(ports.get.unwrap())(plugin, 2, true, &mut port));
        finish(plugin);
        (entry().deinit.unwrap())();
    }
}

#[test]
fn sidechain_reaches_the_plugin() {
    let _turn = turn();
    unsafe {
        let plugin = active();
        assert_eq!(play(plugin, 128, 0.25, Some(0.5), ptr::null()), Some(0.75));
        assert_eq!(play(plugin, 128, 0.25, None, ptr::null()), Some(0.25), "unconnected");
        finish(plugin);
        (entry().deinit.unwrap())();
    }
}

#[test]
fn position_reaches_the_plugin() {
    let _turn = turn();
    unsafe {
        let plugin = active();
        assert_eq!(position(plugin), -1.0);
        play(plugin, 128, 0.0, None, &transport(CLAP_TRANSPORT_HAS_SECONDS_TIMELINE | CLAP_TRANSPORT_IS_PLAYING, 1.0));
        assert_eq!(position(plugin), 48_000.0);

        play(plugin, 128, 0.0, None, &transport(CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, 0.5));
        assert_eq!(position(plugin), 24_000.0);

        play(plugin, 128, 0.0, None, &transport(CLAP_TRANSPORT_IS_PLAYING, 1.0));
        assert_eq!(position(plugin), -1.0, "no seconds timeline");
        play(plugin, 128, 0.0, None, &transport(CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, 2.0));
        play(plugin, 128, 0.0, None, ptr::null());
        assert_eq!(position(plugin), -1.0, "no transport");
        finish(plugin);
        (entry().deinit.unwrap())();
    }
}

#[test]
fn zero_frame_block_is_harmless() {
    let _turn = turn();
    unsafe {
        let plugin = active();
        assert_eq!(play(plugin, 0, 0.25, Some(0.5), ptr::null()), None);
        assert_eq!(play(plugin, 128, 0.25, Some(0.5), ptr::null()), Some(0.75));
        finish(plugin);
        (entry().deinit.unwrap())();
    }
}

#[test]
fn a_sidechain_sharing_the_output_buffer_is_treated_as_absent() {
    let _turn = turn();
    unsafe {
        let plugin = active();
        let mut main_in = [vec![0.25f32; 128], vec![0.25f32; 128]];
        let mut shared = [vec![0.5f32; 128], vec![0.5f32; 128]];
        let mut main_ptrs = [main_in[0].as_mut_ptr(), main_in[1].as_mut_ptr()];
        let mut shared_ptrs = [shared[0].as_mut_ptr(), shared[1].as_mut_ptr()];
        let inputs = [buffer(&mut main_ptrs, 2), buffer(&mut shared_ptrs, 2)];
        let mut output = buffer(&mut shared_ptrs, 2);
        let process = clap_process {
            steady_time: 0,
            frames_count: 128,
            transport: ptr::null(),
            audio_inputs: inputs.as_ptr(),
            audio_outputs: &mut output,
            audio_inputs_count: 2,
            audio_outputs_count: 1,
            in_events: ptr::null(),
            out_events: ptr::null(),
        };
        assert_eq!(((*plugin).process.unwrap())(plugin, &process), CLAP_PROCESS_CONTINUE);
        assert!(shared.iter().all(|channel| channel.iter().all(|&s| s == 0.25)), "{:?}", &shared[0][..4]);
        finish(plugin);
        (entry().deinit.unwrap())();
    }
}
