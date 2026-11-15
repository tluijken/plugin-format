//! Loads the sidechain example's VST3 library and checks what a host gives the plugin besides the main input.

#![allow(non_snake_case)]
#![cfg_attr(windows, allow(clippy::unnecessary_cast, reason = "the vst3 enums are i32 on Windows and u32 elsewhere"))]

use std::ptr;
use std::sync::{Mutex, MutexGuard, OnceLock};

use sidechain::POSITION;
use vst3::Steinberg::Vst::BusInfo_::BusFlags_::kDefaultActive;
use vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_::kPlaying;
use vst3::Steinberg::Vst::SymbolicSampleSizes_::kSample32;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;

mod built;
mod vst3_module;

use vst3_module::Instance;

static SERIAL: Mutex<()> = Mutex::new(());

fn turn() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn library() -> &'static libloading::Library {
    static LIBRARY: OnceLock<libloading::Library> = OnceLock::new();
    LIBRARY.get_or_init(|| vst3_module::load(&built::plugin("sidechain", "vst3")))
}

fn create() -> Instance {
    assert_eq!(&vst3_module::class_id(library()).map(|c| c as u8), b"plugfmtSidechain");
    Instance::create(library())
}

impl Instance {
    /// Stereo everywhere, the sidechain bus activated when `connected`, active with room for 128 frames.
    fn start(&self, connected: bool) {
        unsafe {
            let mut inputs = [SpeakerArr::kStereo, SpeakerArr::kStereo];
            let mut output = SpeakerArr::kStereo;
            assert_eq!(self.processor.setBusArrangements(inputs.as_mut_ptr(), 2, &mut output, 1), kResultTrue);
            if connected {
                assert_eq!(self.component.activateBus(MediaTypes_::kAudio as i32, BusDirections_::kInput as i32, 1, 1), kResultOk);
            }
            let mut setup = ProcessSetup { processMode: 0, symbolicSampleSize: kSample32 as i32, maxSamplesPerBlock: 128, sampleRate: 48_000.0 };
            assert_eq!(self.processor.setupProcessing(&mut setup), kResultOk);
            assert_eq!(self.component.setActive(1), kResultOk);
            assert_eq!(self.processor.setProcessing(1), kResultOk);
        }
    }

    /// One block at `main` on both channels, with the sidechain bus at `side` or empty. Returns the first left output sample.
    unsafe fn play(&self, frames: usize, main: f32, side: Option<f32>, context: *mut ProcessContext) -> Option<f32> {
        unsafe {
            let mut main_in = [vec![main; frames], vec![main; frames]];
            let mut side_in = [vec![side.unwrap_or(0.0); frames], vec![side.unwrap_or(0.0); frames]];
            let mut out = [vec![0.0f32; frames], vec![0.0f32; frames]];
            let mut main_ptrs = [main_in[0].as_mut_ptr(), main_in[1].as_mut_ptr()];
            let mut side_ptrs = [side_in[0].as_mut_ptr(), side_in[1].as_mut_ptr()];
            let mut out_ptrs = [out[0].as_mut_ptr(), out[1].as_mut_ptr()];
            let bus = |pointers: &mut [*mut f32; 2], channels: i32| AudioBusBuffers {
                numChannels: channels,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: pointers.as_mut_ptr() },
            };
            let mut inputs = [bus(&mut main_ptrs, 2), bus(&mut side_ptrs, if side.is_some() { 2 } else { 0 })];
            let mut output = bus(&mut out_ptrs, 2);
            let mut data = ProcessData {
                processMode: 0,
                symbolicSampleSize: kSample32 as i32,
                numSamples: frames as i32,
                numInputs: 2,
                numOutputs: 1,
                inputs: inputs.as_mut_ptr(),
                outputs: &mut output,
                inputParameterChanges: ptr::null_mut(),
                outputParameterChanges: ptr::null_mut(),
                inputEvents: ptr::null_mut(),
                outputEvents: ptr::null_mut(),
                processContext: context,
            };
            assert_eq!(self.processor.process(&mut data), kResultOk);
            out[0].first().copied()
        }
    }

    fn position(&self) -> f64 {
        unsafe { self.controller.normalizedParamToPlain(POSITION, self.controller.getParamNormalized(POSITION)) }
    }
}

fn context(position: i64, playing: bool) -> ProcessContext {
    let mut context: ProcessContext = unsafe { std::mem::zeroed() };
    context.state = if playing { kPlaying as uint32 } else { 0 };
    context.sampleRate = 48_000.0;
    context.projectTimeSamples = position;
    context
}

#[test]
fn the_sidechain_is_a_second_aux_bus_that_starts_inactive() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let (audio, input, output) = (MediaTypes_::kAudio as i32, BusDirections_::kInput as i32, BusDirections_::kOutput as i32);
        assert_eq!((plugin.component.getBusCount(audio, input), plugin.component.getBusCount(audio, output)), (2, 1));
        let mut bus: BusInfo = std::mem::zeroed();
        assert_eq!(plugin.component.getBusInfo(audio, input, 1, &mut bus), kResultOk);
        assert_eq!((bus.channelCount, bus.busType, bus.flags & kDefaultActive as uint32), (2, BusTypes_::kAux as i32, 0));
        assert_eq!(plugin.component.getBusInfo(audio, input, 0, &mut bus), kResultOk);
        assert_eq!((bus.busType, bus.flags & kDefaultActive as uint32 != 0), (BusTypes_::kMain as i32, true));
        assert_eq!(plugin.component.getBusInfo(audio, output, 1, &mut bus), kInvalidArgument);
        assert_eq!(plugin.component.getBusInfo(audio, input, 2, &mut bus), kInvalidArgument);
        let mut arrangement = 0;
        assert_eq!(plugin.processor.getBusArrangement(input, 1, &mut arrangement), kResultOk);
        assert_eq!(arrangement, SpeakerArr::kStereo);
        let mut mono_side = [SpeakerArr::kStereo, SpeakerArr::kMono];
        let mut stereo = SpeakerArr::kStereo;
        assert_eq!(plugin.processor.setBusArrangements(mono_side.as_mut_ptr(), 2, &mut stereo, 1), kResultFalse);
        assert_eq!(plugin.processor.setBusArrangements(mono_side.as_mut_ptr(), 1, &mut stereo, 1), kResultFalse);
        plugin.stop();
    }
}

#[test]
fn sidechain_reaches_the_plugin() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        plugin.start(true);
        assert_eq!(plugin.play(128, 0.25, Some(0.5), ptr::null_mut()), Some(0.75));
        assert_eq!(plugin.play(128, 0.25, None, ptr::null_mut()), Some(0.25), "no channels on the bus");
        plugin.stop();

        let plugin = create();
        plugin.start(false);
        assert_eq!(plugin.play(128, 0.25, Some(0.5), ptr::null_mut()), Some(0.25), "bus left inactive");
        plugin.stop();
    }
}

#[test]
fn position_reaches_the_plugin() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        plugin.start(false);
        assert_eq!(plugin.position(), -1.0);
        plugin.play(128, 0.0, None, &mut context(48_000, true));
        assert_eq!(plugin.position(), 48_000.0);
        plugin.play(128, 0.0, None, &mut context(0, false));
        assert_eq!(plugin.position(), 0.0);
        plugin.play(128, 0.0, None, ptr::null_mut());
        assert_eq!(plugin.position(), -1.0, "no context");
        plugin.stop();
    }
}

#[test]
fn zero_frame_block_is_harmless() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        plugin.start(true);
        assert_eq!(plugin.play(0, 0.25, Some(0.5), ptr::null_mut()), None);
        assert_eq!(plugin.play(128, 0.25, Some(0.5), ptr::null_mut()), Some(0.75));
        plugin.stop();
    }
}

#[test]
fn a_sidechain_sharing_the_output_buffer_is_treated_as_absent() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        plugin.start(true);
        let mut main_in = [vec![0.25f32; 128], vec![0.25f32; 128]];
        let mut shared = [vec![0.5f32; 128], vec![0.5f32; 128]];
        let mut main_ptrs = [main_in[0].as_mut_ptr(), main_in[1].as_mut_ptr()];
        let mut shared_ptrs = [shared[0].as_mut_ptr(), shared[1].as_mut_ptr()];
        let bus = |pointers: &mut [*mut f32; 2]| AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 { channelBuffers32: pointers.as_mut_ptr() },
        };
        let mut inputs = [bus(&mut main_ptrs), bus(&mut shared_ptrs)];
        let mut output = bus(&mut shared_ptrs);
        let mut data = ProcessData {
            processMode: 0,
            symbolicSampleSize: kSample32 as i32,
            numSamples: 128,
            numInputs: 2,
            numOutputs: 1,
            inputs: inputs.as_mut_ptr(),
            outputs: &mut output,
            inputParameterChanges: ptr::null_mut(),
            outputParameterChanges: ptr::null_mut(),
            inputEvents: ptr::null_mut(),
            outputEvents: ptr::null_mut(),
            processContext: ptr::null_mut(),
        };
        assert_eq!(plugin.processor.process(&mut data), kResultOk);
        assert!(shared[0].iter().all(|&s| s == 0.25), "{:?}", &shared[0][..4]);
        plugin.stop();
    }
}
