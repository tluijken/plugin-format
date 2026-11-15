//! Loads the example plugin's built VST3 library the way a host does and runs audio through it.

#![allow(non_snake_case)]
#![cfg_attr(windows, allow(clippy::unnecessary_cast, reason = "the vst3 enums are i32 on Windows and u32 elsewhere"))]

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use gain::{FREQUENCY, GAIN, TEST_GESTURE};
use vst3::Steinberg::Vst::ChannelContext::{IInfoListener, IInfoListenerTrait, kChannelColorKey, kChannelNameKey};
use vst3::Steinberg::Vst::ParameterInfo_::ParameterFlags_::{kCanAutomate, kIsList};
use vst3::Steinberg::Vst::RestartFlags_::kParamValuesChanged;
use vst3::Steinberg::Vst::SymbolicSampleSizes_::{kSample32, kSample64};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper};

mod built;
mod vst3_module;

use vst3_module::Instance;

#[cfg(windows)]
const NATIVE_VIEW: FIDString = kPlatformTypeHWND;
#[cfg(target_os = "macos")]
const NATIVE_VIEW: FIDString = kPlatformTypeNSView;
#[cfg(not(any(windows, target_os = "macos")))]
const NATIVE_VIEW: FIDString = kPlatformTypeX11EmbedWindowID;
#[cfg(windows)]
const OTHER_VIEW: FIDString = kPlatformTypeX11EmbedWindowID;
#[cfg(not(windows))]
const OTHER_VIEW: FIDString = kPlatformTypeHWND;

/// The module entry applies to the whole library, so tests take turns.
static SERIAL: Mutex<()> = Mutex::new(());

fn turn() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn library() -> &'static libloading::Library {
    static LIBRARY: OnceLock<libloading::Library> = OnceLock::new();
    LIBRARY.get_or_init(|| vst3_module::load(&built::plugin("gain", "vst3")))
}

fn factory() -> ComPtr<IPluginFactory> {
    vst3_module::factory(library())
}

fn text(chars: &[std::ffi::c_char]) -> String {
    unsafe { CStr::from_ptr(chars.as_ptr()) }.to_str().unwrap().to_owned()
}

fn wide(chars: &[TChar]) -> String {
    let len = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
    String::from_utf16(&chars[..len]).unwrap()
}

fn class_id() -> TUID {
    vst3_module::class_id(library())
}

fn create() -> Instance {
    Instance::create(library())
}

impl Instance {
    fn arrange(&self, input: SpeakerArrangement, output: SpeakerArrangement) -> tresult {
        let (mut input, mut output) = (input, output);
        unsafe { self.processor.setBusArrangements(&mut input, 1, &mut output, 1) }
    }

    fn start(&self, block: i32) {
        unsafe {
            let mut setup = ProcessSetup { processMode: 0, symbolicSampleSize: kSample32 as i32, maxSamplesPerBlock: block, sampleRate: 48_000.0 };
            assert_eq!(self.processor.setupProcessing(&mut setup), kResultOk);
            assert_eq!(self.component.setActive(1), kResultOk);
            assert_eq!(self.processor.setProcessing(1), kResultOk);
        }
    }

    /// Runs one block; `inputs` and `outputs` hold one vector per channel.
    fn run(&self, inputs: &mut [Vec<f32>], outputs: &mut [Vec<f32>], frames: i32, changes: Option<&ComPtr<IParameterChanges>>) -> tresult {
        unsafe {
            let mut in_ptrs: Vec<*mut f32> = inputs.iter_mut().map(|c| c.as_mut_ptr()).collect();
            let mut out_ptrs: Vec<*mut f32> = outputs.iter_mut().map(|c| c.as_mut_ptr()).collect();
            let mut input = AudioBusBuffers {
                numChannels: inputs.len() as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: in_ptrs.as_mut_ptr() },
            };
            let mut output = AudioBusBuffers {
                numChannels: outputs.len() as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: out_ptrs.as_mut_ptr() },
            };
            let mut data = ProcessData {
                processMode: 0,
                symbolicSampleSize: kSample32 as i32,
                numSamples: frames,
                numInputs: 1,
                numOutputs: 1,
                inputs: &mut input,
                outputs: &mut output,
                inputParameterChanges: changes.map_or(ptr::null_mut(), |c| c.as_ptr()),
                outputParameterChanges: ptr::null_mut(),
                inputEvents: ptr::null_mut(),
                outputEvents: ptr::null_mut(),
                processContext: ptr::null_mut(),
            };
            self.processor.process(&mut data)
        }
    }
}

struct Queue {
    id: ParamID,
    points: Vec<(i32, f64)>,
}

impl Class for Queue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for Queue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id
    }
    unsafe fn getPointCount(&self) -> int32 {
        self.points.len() as int32
    }
    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        let Some(&(at, v)) = self.points.get(index as usize) else { return kInvalidArgument };
        unsafe {
            *offset = at;
            *value = v;
        }
        kResultOk
    }
    unsafe fn addPoint(&self, _: int32, _: ParamValue, _: *mut int32) -> tresult {
        kResultFalse
    }
}

struct Changes(Vec<ComWrapper<Queue>>);

impl Class for Changes {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for Changes {
    unsafe fn getParameterCount(&self) -> int32 {
        self.0.len() as int32
    }
    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        self.0.get(index as usize).and_then(|q| q.as_com_ref::<IParamValueQueue>()).map_or(ptr::null_mut(), |q| q.as_ptr())
    }
    unsafe fn addParameterData(&self, _: *const ParamID, _: *mut int32) -> *mut IParamValueQueue {
        ptr::null_mut()
    }
}

fn changes(queues: &[(ParamID, &[f64])]) -> ComPtr<IParameterChanges> {
    let queues = queues
        .iter()
        .map(|(id, values)| ComWrapper::new(Queue { id: *id, points: values.iter().enumerate().map(|(i, &v)| (i as i32, v)).collect() }))
        .collect();
    ComWrapper::new(Changes(queues)).to_com_ptr().unwrap()
}

/// Takes at most 5 bytes per write and hands out at most 7 per read, as a host's stream may.
struct Memory {
    bytes: RefCell<Vec<u8>>,
    at: Cell<usize>,
}

impl Class for Memory {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for Memory {
    unsafe fn read(&self, buffer: *mut c_void, size: int32, read: *mut int32) -> tresult {
        let bytes = self.bytes.borrow();
        let at = self.at.get();
        let n = (bytes.len() - at).min(size.max(0) as usize).min(7);
        unsafe {
            ptr::copy_nonoverlapping(bytes[at..].as_ptr(), buffer as *mut u8, n);
            if !read.is_null() {
                *read = n as int32;
            }
        }
        self.at.set(at + n);
        kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, size: int32, written: *mut int32) -> tresult {
        let n = size.clamp(0, 5) as usize;
        unsafe {
            self.bytes.borrow_mut().extend_from_slice(std::slice::from_raw_parts(buffer as *const u8, n));
            if !written.is_null() {
                *written = n as int32;
            }
        }
        kResultOk
    }
    unsafe fn seek(&self, _: int64, _: int32, _: *mut int64) -> tresult {
        kNotImplemented
    }
    unsafe fn tell(&self, at: *mut int64) -> tresult {
        unsafe { *at = self.at.get() as int64 };
        kResultOk
    }
}

fn memory(bytes: Vec<u8>) -> ComWrapper<Memory> {
    ComWrapper::new(Memory { bytes: RefCell::new(bytes), at: Cell::new(0) })
}

static RESTARTS: AtomicI32 = AtomicI32::new(0);

struct Handler;

impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, _: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn performEdit(&self, _: ParamID, _: ParamValue) -> tresult {
        kResultOk
    }
    unsafe fn endEdit(&self, _: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        RESTARTS.fetch_or(flags, Ordering::SeqCst);
        kResultOk
    }
}

fn normalised(db: f64) -> f64 {
    (db + 60.0) / 84.0
}

#[derive(Debug, PartialEq)]
enum Edit {
    Begin(ParamID),
    Perform(ParamID, ParamValue),
    End(ParamID),
    Restart(int32),
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Edit>>);

impl Class for Recorder {
    type Interfaces = (IComponentHandler,);
}

impl Recorder {
    fn record(&self, edit: Edit) -> tresult {
        self.0.lock().unwrap().push(edit);
        kResultOk
    }

    fn take(&self) -> Vec<Edit> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

impl IComponentHandlerTrait for Recorder {
    unsafe fn beginEdit(&self, id: ParamID) -> tresult {
        self.record(Edit::Begin(id))
    }
    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        self.record(Edit::Perform(id, value))
    }
    unsafe fn endEdit(&self, id: ParamID) -> tresult {
        self.record(Edit::End(id))
    }
    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        self.record(Edit::Restart(flags))
    }
}

/// An instance whose host records what it is told, and the recording.
fn recorded() -> (Instance, ComWrapper<Recorder>) {
    let plugin = create();
    let recorder = ComWrapper::new(Recorder::default());
    assert_eq!(unsafe { plugin.controller.setComponentHandler(recorder.to_com_ptr::<IComponentHandler>().unwrap().as_ptr()) }, kResultOk);
    (plugin, recorder)
}

#[test]
fn loads_and_scans_the_factory() {
    let _turn = turn();
    unsafe {
        let factory = factory();
        assert_eq!(factory.countClasses(), 1);
        let mut info: PFactoryInfo = std::mem::zeroed();
        assert_eq!(factory.getFactoryInfo(&mut info), kResultOk);
        assert_eq!(text(&info.vendor), "plugin-format");
        assert_eq!(class_id().map(|b| b as u8), *b"plugfmtGainExamp");
        let factory2 = factory.cast::<IPluginFactory2>().expect("IPluginFactory2");
        let mut info2: PClassInfo2 = std::mem::zeroed();
        assert_eq!(factory2.getClassInfo2(0, &mut info2), kResultOk);
        assert_eq!(text(&info2.category), "Audio Module Class");
        assert_eq!(text(&info2.subCategories), "Fx");
        assert_eq!(text(&info2.name), "Gain");
        assert_eq!(text(&info2.version), "0.1.0");
        assert!(text(&info2.sdkVersion).starts_with("VST 3"));
        let factory3 = factory.cast::<IPluginFactory3>().expect("IPluginFactory3");
        let mut info3: PClassInfoW = std::mem::zeroed();
        assert_eq!(factory3.getClassInfoUnicode(0, &mut info3), kResultOk);
        assert_eq!(wide(&info3.name), "Gain");

        let plugin = create();
        let mut controller_cid: TUID = [0; 16];
        assert_ne!(plugin.component.getControllerClassId(&mut controller_cid), kResultOk, "single component");
        assert_eq!(plugin.controller.getParameterCount(), 3);
        let mut param: ParameterInfo = std::mem::zeroed();
        assert_eq!(plugin.controller.getParameterInfo(0, &mut param), kResultOk);
        assert_eq!((param.id, wide(&param.title)), (GAIN, "Gain".to_owned()));
        assert_ne!(param.flags & kCanAutomate, 0);
        assert_eq!(param.flags & kIsList, 0);
        assert_eq!((param.stepCount, param.defaultNormalizedValue), (84, normalised(0.0)));
        let mut frequency: ParameterInfo = std::mem::zeroed();
        assert_eq!(plugin.controller.getParameterInfo(1, &mut frequency), kResultOk);
        assert_eq!((frequency.id, frequency.stepCount), (FREQUENCY, 0));
        let mut text = [0 as TChar; 128];
        let normalised_1k = plugin.controller.plainParamToNormalized(FREQUENCY, 1000.0);
        assert_eq!(plugin.controller.getParamStringByValue(FREQUENCY, normalised_1k, &mut text), kResultOk);
        assert_eq!(wide(&text), "1000 Hz");
        let mut typed: Vec<TChar> = "1000 Hz".encode_utf16().chain([0]).collect();
        let mut parsed = 0.0;
        assert_eq!(plugin.controller.getParamValueByString(FREQUENCY, typed.as_mut_ptr(), &mut parsed), kResultOk);
        assert!((plugin.controller.normalizedParamToPlain(FREQUENCY, parsed) - 1000.0).abs() < 1e-9);

        assert_eq!(plugin.processor.canProcessSampleSize(kSample32 as i32), kResultTrue);
        assert_ne!(plugin.processor.canProcessSampleSize(kSample64 as i32), kResultTrue);
        assert_eq!(plugin.arrange(SpeakerArr::kMono, SpeakerArr::kStereo), kResultTrue);
        plugin.start(128);
        assert_eq!(plugin.processor.getLatencySamples(), 0);
        plugin.stop();
    }
}

#[test]
fn processes_and_restores_state() {
    let _turn = turn();
    unsafe {
        let a = create();
        assert_eq!(a.arrange(SpeakerArr::kStereo, SpeakerArr::kStereo), kResultTrue);
        a.start(128);
        let mut outputs = [vec![0.0; 128], vec![0.0; 128]];
        assert_eq!(a.run(&mut [vec![0.25; 128], vec![0.5; 128]], &mut outputs, 128, None), kResultOk);
        assert!(outputs[0].iter().all(|&s| s == 0.25), "{:?}", &outputs[0][..4]);
        assert!(outputs[1].iter().all(|&s| s == 0.5), "{:?}", &outputs[1][..4]);

        let list = changes(&[(GAIN, &[normalised(-6.0)])]);
        assert_eq!(a.run(&mut [vec![0.25; 128], vec![0.5; 128]], &mut outputs, 128, Some(&list)), kResultOk);
        let factor = 10f32.powf(-6.0 / 20.0);
        assert!(outputs[0].iter().all(|&s| (s - 0.25 * factor).abs() < 1e-6), "{:?}", &outputs[0][..4]);
        assert!(outputs[1].iter().all(|&s| (s - 0.5 * factor).abs() < 1e-6), "{:?}", &outputs[1][..4]);
        assert!((a.controller.getParamNormalized(GAIN) - normalised(-6.0)).abs() < 1e-9);
        let mut text = [0 as TChar; 128];
        assert_eq!(a.controller.getParamStringByValue(GAIN, normalised(-6.0), &mut text), kResultOk);
        assert_eq!(wide(&text), "-6 dB");

        let saved = memory(Vec::new());
        assert_eq!(a.component.getState(saved.to_com_ptr::<IBStream>().unwrap().as_ptr()), kResultOk);
        let bytes = saved.bytes.borrow().clone();
        assert!(bytes.len() > 5, "more than one write");

        let b = create();
        let handler = ComWrapper::new(Handler);
        assert_eq!(b.controller.setComponentHandler(handler.to_com_ptr::<IComponentHandler>().unwrap().as_ptr()), kResultOk);
        RESTARTS.store(0, Ordering::SeqCst);
        let source = memory(bytes);
        let stream = source.to_com_ptr::<IBStream>().unwrap();
        assert_eq!(b.component.setState(stream.as_ptr()), kResultOk);
        assert_eq!(b.controller.setComponentState(stream.as_ptr()), kResultOk);
        assert_ne!(RESTARTS.load(Ordering::SeqCst) & kParamValuesChanged, 0, "host told to re-read values");
        assert!((b.controller.getParamNormalized(GAIN) - normalised(-6.0)).abs() < 1e-9);

        let garbage = memory(b"not a state".to_vec());
        assert_ne!(b.component.setState(garbage.to_com_ptr::<IBStream>().unwrap().as_ptr()), kResultOk);
        assert_ne!(b.component.setState(ptr::null_mut()), kResultOk);
        assert!((b.controller.getParamNormalized(GAIN) - normalised(-6.0)).abs() < 1e-9, "a refused state changes nothing");
        assert_eq!(b.controller.setComponentHandler(ptr::null_mut()), kResultOk);
        a.stop();
        b.component.terminate();
    }
}

#[test]
fn stereo_arrangement_keeps_left_and_right_apart() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        assert_eq!(plugin.arrange(SpeakerArr::kStereo, SpeakerArr::kStereo), kResultTrue);
        let mut arrangement = 0;
        assert_eq!(plugin.processor.getBusArrangement(BusDirections_::kInput as i32, 0, &mut arrangement), kResultOk);
        assert_eq!(arrangement, SpeakerArr::kStereo);
        let mut bus: BusInfo = std::mem::zeroed();
        assert_eq!(plugin.component.getBusInfo(MediaTypes_::kAudio as i32, BusDirections_::kInput as i32, 0, &mut bus), kResultOk);
        assert_eq!(bus.channelCount, 2);

        plugin.start(128);
        assert_ne!(plugin.arrange(SpeakerArr::kMono, SpeakerArr::kStereo), kResultTrue, "arranged while active");
        let mut outputs = [vec![0.0; 128], vec![0.0; 128]];
        plugin.run(&mut [vec![0.25; 128], vec![0.5; 128]], &mut outputs, 128, None);
        assert!(outputs[0].iter().all(|&s| s == 0.25), "{:?}", &outputs[0][..4]);
        assert!(outputs[1].iter().all(|&s| s == 0.5), "{:?}", &outputs[1][..4]);
        plugin.stop();
    }
}

#[test]
fn a_parameter_change_in_process_reaches_the_controller() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        plugin.start(64);
        let list = changes(&[(GAIN, &[normalised(0.0), normalised(12.0)]), (99, &[1.0])]);
        plugin.run(&mut [vec![0.0; 64]], &mut [vec![0.0; 64], vec![0.0; 64]], 64, Some(&list));
        assert!((plugin.controller.getParamNormalized(GAIN) - normalised(12.0)).abs() < 1e-9);

        let list = changes(&[(GAIN, &[normalised(24.0)])]);
        assert_eq!(plugin.run(&mut [vec![]], &mut [vec![], vec![]], 0, Some(&list)), kResultOk, "a flush without audio");
        assert_eq!(plugin.controller.getParamNormalized(GAIN), 1.0);
        plugin.stop();
    }
}

#[test]
fn a_surround_arrangement_is_refused() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        assert_ne!(plugin.arrange(SpeakerArr::k51, SpeakerArr::kStereo), kResultTrue);
        assert_ne!(plugin.arrange(SpeakerArr::kStereo, SpeakerArr::kMono), kResultTrue);
        let mut arrangement = 0;
        assert_eq!(plugin.processor.getBusArrangement(BusDirections_::kInput as i32, 0, &mut arrangement), kResultOk);
        assert_eq!(arrangement, SpeakerArr::kMono, "a refused arrangement changes nothing");
        let mut setup = ProcessSetup { processMode: 0, symbolicSampleSize: kSample64 as i32, maxSamplesPerBlock: 64, sampleRate: 48_000.0 };
        assert_ne!(plugin.processor.setupProcessing(&mut setup), kResultOk, "64-bit samples");
        plugin.component.terminate();
    }
}

#[test]
fn released_while_active_and_empty_blocks_are_survived() {
    let _turn = turn();
    let plugin = create();
    plugin.start(128);
    assert_eq!(plugin.run(&mut [vec![]], &mut [vec![], vec![]], 0, None), kResultOk);
    drop(plugin);

    let plugin = create();
    plugin.start(128);
    let mut outputs = [vec![0.0; 128], vec![0.0; 128]];
    plugin.run(&mut [vec![0.25; 128]], &mut outputs, 128, None);
    assert!(outputs.iter().flatten().all(|&s| s == 0.25));
    plugin.stop();
}

fn view(controller: &ComPtr<IEditController>, name: &CStr) -> Option<ComPtr<IPlugView>> {
    unsafe { ComPtr::from_raw(controller.createView(name.as_ptr())) }
}

/// Opening a real window needs a display, so this stops short of a parent the plugin would embed in.
#[test]
fn the_view_offers_an_embedded_native_window_of_fixed_size_and_outlives_the_component() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        assert!(view(&plugin.controller, c"other").is_none());
        assert!(plugin.controller.createView(ptr::null()).is_null());
        let editor = view(&plugin.controller, c"editor").expect("an editor view");
        assert_eq!(editor.isPlatformTypeSupported(NATIVE_VIEW), kResultTrue);
        assert_eq!(editor.isPlatformTypeSupported(OTHER_VIEW), kResultFalse, "only the platform's own window system");
        assert_eq!(editor.isPlatformTypeSupported(kPlatformTypeWaylandSurfaceID), kResultFalse);
        assert_eq!(editor.isPlatformTypeSupported(ptr::null()), kResultFalse);
        let mut rect = ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
        assert_eq!(editor.getSize(&mut rect), kResultOk);
        assert_eq!((rect.right - rect.left, rect.bottom - rect.top), (200, 100));
        assert_eq!(editor.getSize(ptr::null_mut()), kInvalidArgument);
        assert_eq!(editor.canResize(), kResultFalse);
        let mut wanted = ViewRect { left: 10, top: 20, right: 900, bottom: 900 };
        assert_eq!(editor.checkSizeConstraint(&mut wanted), kResultTrue);
        assert_eq!((wanted.left, wanted.top, wanted.right, wanted.bottom), (10, 20, 210, 120));
        assert_eq!(editor.attached(ptr::null_mut(), NATIVE_VIEW), kResultFalse);
        assert_eq!(editor.attached(ptr::dangling_mut(), OTHER_VIEW), kResultFalse);
        assert_eq!(editor.removed(), kResultOk);
        assert_eq!(editor.setFrame(ptr::null_mut()), kResultOk);

        plugin.component.terminate();
        drop(plugin);
        assert_eq!(editor.getSize(&mut rect), kResultOk);
        assert_eq!(editor.removed(), kResultOk);
    }
}

fn size(editor: &ComPtr<IPlugView>) -> (i32, i32) {
    let mut rect = ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
    assert_eq!(unsafe { editor.getSize(&mut rect) }, kResultOk);
    (rect.right - rect.left, rect.bottom - rect.top)
}

#[cfg(not(target_os = "macos"))]
#[test]
fn the_view_takes_the_hosts_content_scale_and_reports_its_size_in_it() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let editor = view(&plugin.controller, c"editor").expect("an editor view");
        assert_eq!(size(&editor), (200, 100));
        let scaling = editor.cast::<IPlugViewContentScaleSupport>().expect("content scale support");
        assert_eq!(scaling.setContentScaleFactor(1.5), kResultTrue);
        assert_eq!(size(&editor), (300, 150));
        let mut wanted = ViewRect { left: 10, top: 20, right: 900, bottom: 900 };
        assert_eq!(editor.checkSizeConstraint(&mut wanted), kResultTrue);
        assert_eq!((wanted.right, wanted.bottom), (310, 170));
        let mut exact = ViewRect { left: 0, top: 0, right: 300, bottom: 150 };
        assert_eq!(editor.onSize(&mut exact), kResultOk);
        assert_eq!(scaling.setContentScaleFactor(0.0), kResultFalse);
        assert_eq!(scaling.setContentScaleFactor(f32::INFINITY), kResultFalse);
        assert_eq!(size(&editor), (300, 150), "a factor that cannot be applied is ignored");
        assert_eq!(scaling.setContentScaleFactor(1.0), kResultTrue);
        assert_eq!(size(&editor), (200, 100));
        drop(plugin);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn the_view_measures_in_points_and_takes_no_content_scale_from_the_host() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let editor = view(&plugin.controller, c"editor").expect("an editor view");
        let scaling = editor.cast::<IPlugViewContentScaleSupport>().expect("content scale support");
        assert_eq!(scaling.setContentScaleFactor(2.0), kResultFalse);
        assert_eq!(size(&editor), (200, 100));
        drop(plugin);
    }
}

#[test]
fn gesture_reaches_the_vst3_host() {
    let _turn = turn();
    unsafe {
        let (plugin, recorder) = recorded();
        let mut info: ParameterInfo = std::mem::zeroed();
        assert_eq!(plugin.controller.getParameterInfo(2, &mut info), kResultOk);
        assert_eq!((info.id, info.flags & kCanAutomate), (TEST_GESTURE, 0));

        assert_eq!(plugin.controller.setParamNormalized(TEST_GESTURE, 1.0), kResultOk);
        let edits: Vec<Edit> = recorder.take().into_iter().filter(|e| !matches!(e, Edit::Restart(_))).collect();
        assert_eq!(edits, [Edit::Begin(GAIN), Edit::Perform(GAIN, normalised(-6.0)), Edit::End(GAIN)]);
        assert!((plugin.controller.getParamNormalized(GAIN) - normalised(-6.0)).abs() < 1e-9);
        plugin.component.terminate();
    }
}

#[test]
fn mark_dirty_reaches_the_host() {
    let _turn = turn();
    unsafe {
        let (plugin, recorder) = recorded();
        assert_eq!(plugin.controller.setParamNormalized(TEST_GESTURE, 1.0), kResultOk);
        let restarts: Vec<Edit> = recorder.take().into_iter().filter(|e| matches!(e, Edit::Restart(_))).collect();
        assert_eq!(restarts, [Edit::Restart(kParamValuesChanged)]);
        plugin.component.terminate();
    }
}

struct Channel {
    name: Option<&'static str>,
    argb: Option<i64>,
}

impl Class for Channel {
    type Interfaces = (IAttributeList,);
}

impl IAttributeListTrait for Channel {
    unsafe fn setInt(&self, _: IAttributeList_::AttrID, _: int64) -> tresult {
        kNotImplemented
    }
    unsafe fn getInt(&self, id: IAttributeList_::AttrID, value: *mut int64) -> tresult {
        match self.argb {
            Some(argb) if unsafe { CStr::from_ptr(id) } == unsafe { CStr::from_ptr(kChannelColorKey) } => {
                unsafe { *value = argb };
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setFloat(&self, _: IAttributeList_::AttrID, _: f64) -> tresult {
        kNotImplemented
    }
    unsafe fn getFloat(&self, _: IAttributeList_::AttrID, _: *mut f64) -> tresult {
        kResultFalse
    }
    unsafe fn setString(&self, _: IAttributeList_::AttrID, _: *const TChar) -> tresult {
        kNotImplemented
    }
    unsafe fn getString(&self, id: IAttributeList_::AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        match self.name {
            Some(name) if unsafe { CStr::from_ptr(id) } == unsafe { CStr::from_ptr(kChannelNameKey) } => {
                let mut units: Vec<TChar> = name.encode_utf16().map(|u| u as TChar).collect();
                units.truncate((size_in_bytes as usize / 2).saturating_sub(1));
                units.push(0);
                unsafe { ptr::copy_nonoverlapping(units.as_ptr(), string, units.len()) };
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setBinary(&self, _: IAttributeList_::AttrID, _: *const c_void, _: uint32) -> tresult {
        kNotImplemented
    }
    unsafe fn getBinary(&self, _: IAttributeList_::AttrID, _: *mut *const c_void, _: *mut uint32) -> tresult {
        kResultFalse
    }
}

fn saved_state(plugin: &Instance) -> String {
    unsafe {
        let saved = memory(Vec::new());
        assert_eq!(plugin.component.getState(saved.to_com_ptr::<IBStream>().unwrap().as_ptr()), kResultOk);
        let bytes = saved.bytes.borrow().clone();
        String::from_utf8(bytes).unwrap()
    }
}

#[test]
fn vst3_channel_context_reaches_the_plugin() {
    let _turn = turn();
    unsafe {
        let plugin = create();
        let listener = plugin.controller.cast::<IInfoListener>().expect("the controller listens to the channel context");

        let both = ComWrapper::new(Channel { name: Some("Bass"), argb: Some(0xFFFF_0000) });
        assert_eq!(listener.setChannelContextInfos(both.to_com_ptr::<IAttributeList>().unwrap().as_ptr()), kResultOk);
        assert!(saved_state(&plugin).ends_with(";name=Bass;color=ff0000"));

        let colour_only = ComWrapper::new(Channel { name: None, argb: Some(0xFF00_80FF) });
        assert_eq!(listener.setChannelContextInfos(colour_only.to_com_ptr::<IAttributeList>().unwrap().as_ptr()), kResultOk);
        assert!(saved_state(&plugin).ends_with(";color=0080ff"), "a name only when the host has one");

        let name_only = ComWrapper::new(Channel { name: Some("Keys"), argb: None });
        assert_eq!(listener.setChannelContextInfos(name_only.to_com_ptr::<IAttributeList>().unwrap().as_ptr()), kResultOk);
        assert!(saved_state(&plugin).ends_with(";name=Keys"), "a colour only when the host has one");
        assert_ne!(listener.setChannelContextInfos(ptr::null_mut()), kResultOk);
        plugin.component.terminate();
    }
}

#[test]
fn processing_flushes_subnormals_and_leaves_the_hosts_flags_alone() {
    let _turn = turn();
    let host_product = || std::hint::black_box(1e-36f32) * std::hint::black_box(1e-3f32);
    let plugin = create();
    assert_eq!(plugin.arrange(SpeakerArr::kStereo, SpeakerArr::kStereo), kResultTrue);
    plugin.start(128);
    let list = changes(&[(GAIN, &[normalised(-60.0)])]);
    let mut outputs = [vec![1.0; 128], vec![1.0; 128]];
    assert_eq!(plugin.run(&mut [vec![1e-36; 128], vec![1e-36; 128]], &mut outputs, 128, Some(&list)), kResultOk);
    assert!(outputs.iter().flatten().all(|&s| s == 0.0), "1e-36 at -60 dB is subnormal: {:?}", &outputs[0][..4]);
    assert!(host_product().is_subnormal(), "the host's floating-point state is back");
    plugin.stop();
}
