use std::cell::UnsafeCell;
use std::ffi::{CStr, c_char, c_void};
use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::ThreadId;

#[cfg(target_os = "linux")]
use vst3::Steinberg::Linux::IRunLoopTrait;
use vst3::Steinberg::Vst::BusDirections_::{kInput, kOutput};
use vst3::Steinberg::Vst::BusInfo_::BusFlags_::kDefaultActive;
use vst3::Steinberg::Vst::BusTypes_::{kAux, kMain};
use vst3::Steinberg::Vst::IProcessContextRequirements_::Flags_::kNeedTransportState;
use vst3::Steinberg::Vst::MediaTypes_::kAudio;
use vst3::Steinberg::Vst::ParameterInfo_::ParameterFlags_::{kCanAutomate, kIsList};
use vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_::kPlaying;
use vst3::Steinberg::Vst::RestartFlags_::{kLatencyChanged, kParamValuesChanged};
use vst3::Steinberg::Vst::SymbolicSampleSizes_::kSample32;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComRef, ComWrapper};

use crate::denormals::FlushDenormals;
use crate::editor::{self, Parent};
use crate::gui_context::{Event, ResizeId};
use crate::{Extras, GuiContext, Kind, ParamInfo, Plugin, Shared, TrackInfo, copy_str, guard, process_block};

const CATEGORY: &str = "Audio Module Class";

fn copy_wide(text: &str, out: &mut [TChar]) {
    let Some(room) = out.len().checked_sub(1) else { return };
    let mut len = 0;
    for (o, unit) in out.iter_mut().zip(text.encode_utf16()).take(room) {
        *o = unit;
        len += 1;
    }
    if len > 0 && (0xD800..0xDC00).contains(&out[len - 1]) {
        len -= 1;
    }
    out[len] = 0;
}

/// Reads a NUL-terminated string the host hands over, giving up after 1024 units.
///
/// # Safety
/// `text` is valid for reads up to and including its terminator, or for 1024 units.
unsafe fn read_wide(text: *const TChar) -> Option<String> {
    let mut units = Vec::new();
    for i in 0..1024 {
        let unit = unsafe { *text.add(i) };
        if unit == 0 {
            return String::from_utf16(&units).ok();
        }
        units.push(unit);
    }
    None
}

fn class_id<P: Plugin>() -> TUID {
    P::INFO.vst3_class_id.map(|b| b as c_char)
}

/// The library's `GetPluginFactory`: a new reference the host releases.
pub fn factory<P: Plugin>() -> *mut c_void {
    guard(ptr::null_mut(), || {
        ComWrapper::new(Factory::<P>(PhantomData)).to_com_ptr::<IPluginFactory>().map_or(ptr::null_mut(), |f| f.into_raw() as *mut c_void)
    })
}

struct Factory<P>(PhantomData<P>);

impl<P: Plugin> Class for Factory<P> {
    type Interfaces = (IPluginFactory3,);
}

impl<P: Plugin> IPluginFactoryTrait for Factory<P> {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        guard(kInternalError, || {
            if info.is_null() {
                return kInvalidArgument;
            }
            let mut out: PFactoryInfo = unsafe { std::mem::zeroed() };
            copy_str(P::INFO.vendor, &mut out.vendor);
            out.flags = PFactoryInfo_::FactoryFlags_::kUnicode as int32;
            unsafe { info.write(out) };
            kResultOk
        })
    }

    unsafe fn countClasses(&self) -> int32 {
        1
    }

    unsafe fn getClassInfo(&self, index: int32, info: *mut PClassInfo) -> tresult {
        guard(kInternalError, || {
            if index != 0 || info.is_null() {
                return kInvalidArgument;
            }
            let mut out: PClassInfo = unsafe { std::mem::zeroed() };
            out.cid = class_id::<P>();
            out.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
            copy_str(CATEGORY, &mut out.category);
            copy_str(P::INFO.name, &mut out.name);
            unsafe { info.write(out) };
            kResultOk
        })
    }

    unsafe fn createInstance(&self, cid: FIDString, iid: FIDString, obj: *mut *mut c_void) -> tresult {
        guard(kInternalError, || unsafe {
            if obj.is_null() {
                return kInvalidArgument;
            }
            obj.write(ptr::null_mut());
            if cid.is_null() || iid.is_null() {
                return kInvalidArgument;
            }
            if ptr::read_unaligned(cid as *const TUID) != class_id::<P>() {
                return kNoInterface;
            }
            let instance = ComWrapper::new(Component::<P>::new());
            let Some(unknown) = instance.to_com_ptr::<FUnknown>() else { return kNoInterface };
            let unknown = unknown.as_ptr();
            ((*(*unknown).vtbl).queryInterface)(unknown, iid as *const TUID, obj)
        })
    }
}

impl<P: Plugin> IPluginFactory2Trait for Factory<P> {
    unsafe fn getClassInfo2(&self, index: int32, info: *mut PClassInfo2) -> tresult {
        guard(kInternalError, || {
            if index != 0 || info.is_null() {
                return kInvalidArgument;
            }
            let mut out: PClassInfo2 = unsafe { std::mem::zeroed() };
            out.cid = class_id::<P>();
            out.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
            copy_str(CATEGORY, &mut out.category);
            copy_str(P::INFO.name, &mut out.name);
            copy_str(P::INFO.vst3_subcategories, &mut out.subCategories);
            copy_str(P::INFO.vendor, &mut out.vendor);
            copy_str(P::INFO.version, &mut out.version);
            copy_str(sdk_version(), &mut out.sdkVersion);
            unsafe { info.write(out) };
            kResultOk
        })
    }
}

impl<P: Plugin> IPluginFactory3Trait for Factory<P> {
    unsafe fn getClassInfoUnicode(&self, index: int32, info: *mut PClassInfoW) -> tresult {
        guard(kInternalError, || {
            if index != 0 || info.is_null() {
                return kInvalidArgument;
            }
            let mut out: PClassInfoW = unsafe { std::mem::zeroed() };
            out.cid = class_id::<P>();
            out.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
            copy_str(CATEGORY, &mut out.category);
            copy_wide(P::INFO.name, &mut out.name);
            copy_str(P::INFO.vst3_subcategories, &mut out.subCategories);
            copy_wide(P::INFO.vendor, &mut out.vendor);
            copy_wide(P::INFO.version, &mut out.version);
            copy_wide(sdk_version(), &mut out.sdkVersion);
            unsafe { info.write(out) };
            kResultOk
        })
    }

    unsafe fn setHostContext(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
}

fn sdk_version() -> &'static str {
    unsafe { CStr::from_ptr(SDKVersionString) }.to_str().unwrap_or_default()
}

/// One object is component, processor and controller, as in the SDK's single-component effects.
///
/// `plugin` is reached only through `claim`, which hands it to one caller at a time: the main thread
/// in `setActive`, the audio thread in `process`, and the last release. VST3 never runs `setActive`
/// alongside `process`; should a host do so anyway, the loser of the claim skips its work rather
/// than alias the processor. Everything else goes through `shared` and atomics.
struct Component<P: Plugin> {
    plugin: UnsafeCell<P>,
    claimed: AtomicBool,
    shared: Arc<P::Shared>,
    /// Changed only while `plugin` is claimed, so a claimant sees it consistent with `plugin`.
    active: AtomicBool,
    inputs: AtomicU32,
    sidechain_active: AtomicBool,
    sample_rate: AtomicU64,
    /// 0 until `setupProcessing`.
    max_block: AtomicU32,
    host: Arc<Host>,
}

/// What the component tells the host, shared with its views so that a view the host keeps after
/// releasing the component still has something valid to announce through.
struct Host {
    /// What `getLatencySamples` reports. It never reads `plugin`, so a host may ask at any time.
    latency: AtomicU32,
    /// Left by `process` for a call on the host's UI thread to announce, since Steinberg requires
    /// `restartComponent` there: `setActive(false)`, the open editor's timer, or a parameter or state
    /// call.
    new_latency: AtomicU32,
    latency_changed: AtomicBool,
    handler: Mutex<Option<ComPtr<IComponentHandler>>>,
    /// The thread that set the handler, which Steinberg makes the UI thread.
    ui_thread: Mutex<Option<ThreadId>>,
    params: &'static [ParamInfo],
    cx: GuiContext,
}

impl Host {
    fn new(latency: u32, shared: Weak<dyn Shared>) -> Arc<Self> {
        let host = Arc::new(Host {
            latency: AtomicU32::new(latency),
            new_latency: AtomicU32::new(latency),
            latency_changed: AtomicBool::new(false),
            handler: Mutex::new(None),
            ui_thread: Mutex::new(None),
            params: shared.upgrade().map_or(&[], |shared| shared.params()),
            cx: GuiContext::new(shared),
        });
        let weak = Arc::downgrade(&host);
        // Steinberg wants the handler called on the UI thread, while the context may be used from any
        // thread, on Linux from the editor's own. So a call from the UI thread is delivered at once and
        // any other waits for the next delivery there: any open view's run-loop timer on Linux (also
        // another instance's, whose editor may be the one writing), the editor's frame on Windows
        // (where the window runs on the UI thread), or a new handler.
        host.cx.connect(Box::new(move |_| {
            let Some(host) = weak.upgrade() else { return };
            if host.on_ui_thread() {
                host.deliver();
            } else {
                let mut waiting = WAITING.lock().unwrap_or_else(|e| e.into_inner());
                if !waiting.iter().any(|w| w.ptr_eq(&weak)) {
                    waiting.push(weak.clone());
                }
            }
        }));
        host
    }

    fn on_ui_thread(&self) -> bool {
        *self.ui_thread.lock().unwrap_or_else(|e| e.into_inner()) == Some(std::thread::current().id())
    }

    fn handler(&self) -> Option<ComPtr<IComponentHandler>> {
        self.handler.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn restart(&self, flags: RestartFlags) {
        if let Some(handler) = self.handler() {
            unsafe { handler.restartComponent(flags) };
        }
    }

    fn announce_latency(&self) {
        if self.latency_changed.swap(false, Ordering::AcqRel) {
            self.latency.store(self.new_latency.load(Ordering::Acquire), Ordering::Release);
            self.restart(kLatencyChanged);
        }
    }

    /// For the calls a host makes on the UI thread whether or not the editor is open, so a change
    /// waits for at most the next one.
    fn announce_latency_here(&self) {
        if self.on_ui_thread() {
            self.announce_latency();
        }
    }

    /// On the UI thread only. Without a handler everything waits for one.
    fn deliver(&self) {
        let Some(handler) = self.handler() else { return };
        self.cx.drain(|event| {
            unsafe {
                match event {
                    Event::Begin(id) => handler.beginEdit(id),
                    Event::Value(id, plain) => {
                        let normalised = self.params.iter().find(|p| p.id == id).map_or(0.0, |info| info.normalise(plain));
                        handler.performEdit(id, normalised)
                    }
                    Event::End(id) => handler.endEdit(id),
                }
            };
            true
        });
        if self.cx.take_dirty() {
            unsafe { handler.restartComponent(kParamValuesChanged) };
        }
    }

    /// What waits for the UI thread.
    fn tick(&self) {
        self.announce_latency();
        self.deliver();
        deliver_waiting();
    }
}

/// The instances in the library with changes made off the UI thread, for whichever instance ticks
/// there first.
static WAITING: Mutex<Vec<Weak<Host>>> = Mutex::new(Vec::new());

/// On the UI thread: delivers for every waiting instance whose UI thread it is.
fn deliver_waiting() {
    let waiting = std::mem::take(&mut *WAITING.lock().unwrap_or_else(|e| e.into_inner()));
    let mut elsewhere = Vec::new();
    for weak in waiting {
        let Some(host) = weak.upgrade() else { continue };
        if host.on_ui_thread() {
            host.deliver();
        } else {
            elsewhere.push(weak);
        }
    }
    if !elsewhere.is_empty() {
        WAITING.lock().unwrap_or_else(|e| e.into_inner()).extend(elsewhere);
    }
}

impl<P: Plugin> Class for Component<P> {
    type Interfaces = (IComponent, IAudioProcessor, IEditController, IProcessContextRequirements, ChannelContext::IInfoListener);
}

impl<P: Plugin> Drop for Component<P> {
    fn drop(&mut self) {
        guard((), || {
            if *self.active.get_mut() {
                self.plugin.get_mut().deactivate();
            }
        })
    }
}

impl<P: Plugin> Component<P> {
    fn new() -> Self {
        let (plugin, shared) = P::new();
        let latency = plugin.latency();
        let weak: Weak<dyn Shared> = Arc::downgrade(&shared) as Weak<P::Shared>;
        Component {
            plugin: UnsafeCell::new(plugin),
            claimed: AtomicBool::new(false),
            shared,
            active: AtomicBool::new(false),
            inputs: AtomicU32::new(crate::default_inputs::<P>()),
            sidechain_active: AtomicBool::new(false),
            sample_rate: AtomicU64::new(0),
            max_block: AtomicU32::new(0),
            host: Host::new(latency, weak),
        }
    }

    /// `None` when another thread holds the processor.
    fn claim<R>(&self, body: impl FnOnce(&mut P) -> R) -> Option<R> {
        struct Release<'a>(&'a AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        self.claimed.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).ok()?;
        let _release = Release(&self.claimed);
        Some(body(unsafe { &mut *self.plugin.get() }))
    }

    fn info(&self, id: ParamID) -> Option<&'static ParamInfo> {
        self.shared.params().iter().find(|p| p.id == id)
    }

    fn set_active(&self, active: bool) -> tresult {
        if active == self.active.load(Ordering::Acquire) {
            return kResultOk;
        }
        if active {
            let max_block = self.max_block.load(Ordering::Acquire) as usize;
            if max_block == 0 {
                return kResultFalse;
            }
            let sample_rate = f64::from_bits(self.sample_rate.load(Ordering::Acquire));
            let inputs = self.inputs.load(Ordering::Acquire) as usize;
            let Some(latency) = self.claim(|plugin| {
                let latency = plugin.activate(sample_rate, max_block, inputs);
                self.active.store(true, Ordering::Release);
                latency
            }) else {
                return kResultFalse;
            };
            if self.host.latency.swap(latency, Ordering::AcqRel) != latency {
                self.host.restart(kLatencyChanged);
            }
        } else {
            let deactivated = self.claim(|plugin| {
                plugin.deactivate();
                self.active.store(false, Ordering::Release);
            });
            if deactivated.is_none() {
                return kResultFalse;
            }
            self.host.announce_latency();
        }
        kResultOk
    }

    fn apply(&self, changes: *mut IParameterChanges) {
        unsafe {
            let Some(changes) = ComRef::from_raw(changes) else { return };
            for i in 0..changes.getParameterCount() {
                let Some(queue) = ComRef::from_raw(changes.getParameterData(i)) else { continue };
                let id = queue.getParameterId();
                let points = queue.getPointCount();
                let Some(info) = self.info(id) else { continue };
                let (mut offset, mut value) = (0, 0.0);
                if points > 0 && queue.getPoint(points - 1, &mut offset, &mut value) == kResultOk {
                    self.shared.set_param(id, info.plain(value));
                }
            }
        }
    }

    /// The aux input bus when the plugin has one, the host activated it and gave both its channels.
    ///
    /// # Safety
    /// `data.inputs` holds `data.numInputs` valid buses, each with `numChannels` pointers in `channelBuffers32`.
    unsafe fn sidechain(&self, data: &ProcessData) -> Option<[*const f32; 2]> {
        unsafe {
            if !P::INFO.sidechain || !self.sidechain_active.load(Ordering::Acquire) || data.numInputs < 2 || data.inputs.is_null() {
                return None;
            }
            let bus = &*data.inputs.add(1);
            let buffers = bus.__field0.channelBuffers32;
            if bus.numChannels < 2 || buffers.is_null() {
                return None;
            }
            let (left, right) = (*buffers as *const f32, *buffers.add(1) as *const f32);
            (!left.is_null() && !right.is_null()).then_some([left, right])
        }
    }

    /// `projectTimeSamples` has no validity flag of its own: a host that gives a context fills it.
    fn position(data: &ProcessData) -> Option<i64> {
        unsafe { data.processContext.as_ref() }.map(|context| context.projectTimeSamples)
    }

    fn playing(data: &ProcessData) -> bool {
        unsafe { data.processContext.as_ref() }.is_some_and(|context| context.state & kPlaying as uint32 != 0)
    }

    /// # Safety
    /// Called from `process` with the host's data: its buses hold `numSamples` valid frames for the call.
    unsafe fn run(&self, data: &ProcessData) -> tresult {
        unsafe {
            if data.numSamples <= 0 {
                return kResultOk;
            }
            if data.symbolicSampleSize != kSample32 as int32 {
                return kInvalidArgument;
            }
            let frames = data.numSamples as usize;
            if data.numOutputs < 1 || data.outputs.is_null() {
                return kResultOk;
            }
            let output = data.outputs;
            let count = ((*output).numChannels.max(0) as usize).min(2);
            if count == 0 {
                return kResultOk;
            }
            let buffers = (*output).__field0.channelBuffers32;
            if buffers.is_null() {
                return kInvalidArgument;
            }
            let mut outs = [ptr::null_mut::<f32>(); 2];
            for (c, out) in outs.iter_mut().enumerate().take(count) {
                *out = *buffers.add(c);
                if out.is_null() {
                    return kInvalidArgument;
                }
            }
            if count == 2 && outs[0] == outs[1] {
                return kInvalidArgument;
            }
            (*output).silenceFlags = 0;

            let mut ins = [ptr::null::<f32>(); 2];
            let mut inputs = 0;
            if data.numInputs > 0 && !data.inputs.is_null() && !(*data.inputs).__field0.channelBuffers32.is_null() {
                let input = data.inputs;
                inputs = ((*input).numChannels.max(0) as usize).min(2);
                for (c, source) in ins.iter_mut().enumerate().take(inputs) {
                    *source = *(*input).__field0.channelBuffers32.add(c);
                }
            }
            let configured = self.inputs.load(Ordering::Acquire) as usize;
            let extras = Extras { sidechain: self.sidechain(data), position: Self::position(data), playing: Self::playing(data) };
            let played = self.claim(|plugin| {
                if !self.active.load(Ordering::Acquire) {
                    return kResultFalse;
                }
                process_block(plugin, frames, &outs[..count], &ins[..inputs], configured, extras);
                if plugin.take_latency_changed() {
                    self.host.new_latency.store(plugin.latency(), Ordering::Release);
                    self.host.latency_changed.store(true, Ordering::Release);
                }
                kResultOk
            });
            played.unwrap_or(kResultFalse)
        }
    }

    fn save_state(&self, state: *mut IBStream) -> tresult {
        unsafe {
            let Some(stream) = ComRef::from_raw(state) else { return kInvalidArgument };
            let bytes = self.shared.save();
            let mut rest = &bytes[..];
            while !rest.is_empty() {
                let mut written = 0;
                let size = rest.len().min(int32::MAX as usize) as int32;
                if stream.write(rest.as_ptr() as *mut c_void, size, &mut written) != kResultOk || written <= 0 {
                    return kResultFalse;
                }
                rest = &rest[(written as usize).min(rest.len())..];
            }
            kResultOk
        }
    }

    fn load_state(&self, state: *mut IBStream) -> tresult {
        unsafe {
            let Some(stream) = ComRef::from_raw(state) else { return kInvalidArgument };
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let mut got = 0;
                let result = stream.read(chunk.as_mut_ptr() as *mut c_void, chunk.len() as int32, &mut got);
                let got = (got.max(0) as usize).min(chunk.len());
                bytes.extend_from_slice(&chunk[..got]);
                if result != kResultOk || got == 0 {
                    break;
                }
            }
            if !self.shared.load(&bytes) {
                return kResultFalse;
            }
        }
        self.host.restart(kParamValuesChanged);
        kResultOk
    }
}

impl<P: Plugin> IPluginBaseTrait for Component<P> {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        guard(kInternalError, || {
            *self.host.handler.lock().unwrap_or_else(|e| e.into_inner()) = None;
            kResultOk
        })
    }
}

impl<P: Plugin> IComponentTrait for Component<P> {
    /// No separate controller: the host finds `IEditController` on this object.
    unsafe fn getControllerClassId(&self, _class_id: *mut TUID) -> tresult {
        kNotImplemented
    }

    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kNotImplemented
    }

    unsafe fn getBusCount(&self, media: MediaType, dir: BusDirection) -> int32 {
        if media != kAudio as MediaType {
            return 0;
        }
        match dir {
            d if d == kInput as BusDirection => 1 + P::INFO.sidechain as int32,
            d if d == kOutput as BusDirection => 1,
            _ => 0,
        }
    }

    unsafe fn getBusInfo(&self, media: MediaType, dir: BusDirection, index: int32, bus: *mut BusInfo) -> tresult {
        guard(kInternalError, || {
            let input = dir == kInput as BusDirection;
            if media != kAudio as MediaType || bus.is_null() || index < 0 || index > (input && P::INFO.sidechain) as int32 {
                return kInvalidArgument;
            }
            let (channels, name) = match dir {
                _ if input && index == 1 => (2, "Sidechain"),
                _ if input => (self.inputs.load(Ordering::Acquire) as int32, "Input"),
                d if d == kOutput as BusDirection => (2, "Output"),
                _ => return kInvalidArgument,
            };
            let mut info: BusInfo = unsafe { std::mem::zeroed() };
            info.mediaType = media;
            info.direction = dir;
            info.channelCount = channels;
            copy_wide(name, &mut info.name);
            let aux = input && index == 1;
            info.busType = if aux { kAux } else { kMain } as BusType;
            info.flags = if aux { 0 } else { kDefaultActive as uint32 };
            unsafe { bus.write(info) };
            kResultOk
        })
    }

    unsafe fn getRoutingInfo(&self, _in_info: *mut RoutingInfo, _out_info: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }

    unsafe fn activateBus(&self, media: MediaType, dir: BusDirection, index: int32, state: TBool) -> tresult {
        if media != kAudio as MediaType {
            return kInvalidArgument;
        }
        let input = dir == kInput as BusDirection;
        match index {
            0 if input || dir == kOutput as BusDirection => kResultOk,
            1 if input && P::INFO.sidechain => {
                self.sidechain_active.store(state != 0, Ordering::Release);
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn setActive(&self, state: TBool) -> tresult {
        guard(kInternalError, || self.set_active(state != 0))
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        guard(kInternalError, || {
            self.host.announce_latency_here();
            self.load_state(state)
        })
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        guard(kInternalError, || self.save_state(state))
    }
}

impl<P: Plugin> IAudioProcessorTrait for Component<P> {
    unsafe fn setBusArrangements(&self, inputs: *mut SpeakerArrangement, num_ins: int32, outputs: *mut SpeakerArrangement, num_outs: int32) -> tresult {
        guard(kInternalError, || unsafe {
            let buses = 1 + P::INFO.sidechain as int32;
            if num_ins != buses || num_outs != 1 || inputs.is_null() || outputs.is_null() || *outputs != SpeakerArr::kStereo {
                return kResultFalse;
            }
            if P::INFO.sidechain && *inputs.add(1) != SpeakerArr::kStereo {
                return kResultFalse;
            }
            let channels = match *inputs {
                SpeakerArr::kMono => 1,
                SpeakerArr::kStereo => 2,
                _ => return kResultFalse,
            };
            if self.active.load(Ordering::Acquire) {
                return kResultFalse;
            }
            self.inputs.store(channels, Ordering::Release);
            kResultTrue
        })
    }

    unsafe fn getBusArrangement(&self, dir: BusDirection, index: int32, arr: *mut SpeakerArrangement) -> tresult {
        guard(kInternalError, || {
            if index < 0 || index > (dir == kInput as BusDirection && P::INFO.sidechain) as int32 || arr.is_null() {
                return kInvalidArgument;
            }
            let arrangement = match dir {
                d if d == kInput as BusDirection && index == 1 => SpeakerArr::kStereo,
                d if d == kInput as BusDirection && self.inputs.load(Ordering::Acquire) == 1 => SpeakerArr::kMono,
                d if d == kInput as BusDirection || d == kOutput as BusDirection => SpeakerArr::kStereo,
                _ => return kInvalidArgument,
            };
            unsafe { arr.write(arrangement) };
            kResultOk
        })
    }

    unsafe fn canProcessSampleSize(&self, size: int32) -> tresult {
        if size == kSample32 as int32 { kResultTrue } else { kResultFalse }
    }

    unsafe fn getLatencySamples(&self) -> uint32 {
        self.host.latency.load(Ordering::Acquire)
    }

    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        guard(kInternalError, || {
            let Some(setup) = (unsafe { setup.as_ref() }) else { return kInvalidArgument };
            if setup.symbolicSampleSize != kSample32 as int32 || self.active.load(Ordering::Acquire) {
                return kResultFalse;
            }
            if !(setup.sampleRate.is_finite() && setup.sampleRate > 0.0) || setup.maxSamplesPerBlock <= 0 {
                return kInvalidArgument;
            }
            self.sample_rate.store(setup.sampleRate.to_bits(), Ordering::Release);
            self.max_block.store(setup.maxSamplesPerBlock as u32, Ordering::Release);
            kResultOk
        })
    }

    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        guard(kInternalError, || unsafe {
            let Some(data) = data.as_ref() else { return kInvalidArgument };
            let _flushed = FlushDenormals::new();
            self.apply(data.inputParameterChanges);
            self.run(data)
        })
    }

    unsafe fn getTailSamples(&self) -> uint32 {
        0
    }
}

impl<P: Plugin> IProcessContextRequirementsTrait for Component<P> {
    unsafe fn getProcessContextRequirements(&self) -> uint32 {
        kNeedTransportState as uint32
    }
}

impl<P: Plugin> ChannelContext::IInfoListenerTrait for Component<P> {
    unsafe fn setChannelContextInfos(&self, list: *mut IAttributeList) -> tresult {
        guard(kInternalError, || {
            let Some(list) = (unsafe { ComRef::from_raw(list) }) else { return kInvalidArgument };
            let mut buffer = [0 as TChar; 256];
            let name = (unsafe { list.getString(ChannelContext::kChannelNameKey, buffer.as_mut_ptr(), size_of_val(&buffer) as u32) } == kResultOk).then(|| {
                let units: Vec<u16> = buffer.iter().copied().take_while(|&c| c != 0).collect();
                String::from_utf16_lossy(&units)
            });
            let mut argb = 0i64;
            let color = (unsafe { list.getInt(ChannelContext::kChannelColorKey, &mut argb) } == kResultOk).then_some([
                (argb >> 16) as u8,
                (argb >> 8) as u8,
                argb as u8,
            ]);
            self.host.cx.set_track(TrackInfo { name, color });
            kResultOk
        })
    }
}

impl<P: Plugin> IEditControllerTrait for Component<P> {
    /// The component's own `setState` already loaded it.
    unsafe fn setComponentState(&self, _state: *mut IBStream) -> tresult {
        guard(kInternalError, || {
            self.host.announce_latency_here();
            kResultOk
        })
    }

    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        guard(kInternalError, || {
            self.host.announce_latency_here();
            kResultOk
        })
    }

    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getParameterCount(&self) -> int32 {
        guard(0, || self.shared.params().len() as int32)
    }

    unsafe fn getParameterInfo(&self, index: int32, info: *mut ParameterInfo) -> tresult {
        guard(kInternalError, || {
            let Some(param) = usize::try_from(index).ok().and_then(|i| self.shared.params().get(i)) else { return kInvalidArgument };
            if info.is_null() {
                return kInvalidArgument;
            }
            let mut out: ParameterInfo = unsafe { std::mem::zeroed() };
            out.id = param.id;
            copy_wide(param.name, &mut out.title);
            copy_wide(param.name, &mut out.shortTitle);
            out.stepCount = param.step_count();
            out.defaultNormalizedValue = param.normalise(param.default);
            out.unitId = kRootUnitId;
            if param.automatable {
                out.flags |= kCanAutomate;
            }
            if matches!(param.kind, Kind::List { .. }) {
                out.flags |= kIsList;
            }
            unsafe { info.write(out) };
            kResultOk
        })
    }

    unsafe fn getParamStringByValue(&self, id: ParamID, value: ParamValue, string: *mut String128) -> tresult {
        guard(kInternalError, || {
            let Some(info) = self.info(id) else { return kInvalidArgument };
            let Some(string) = (unsafe { string.as_mut() }) else { return kInvalidArgument };
            copy_wide(&self.shared.param_text(id, info.plain(value)), string);
            kResultOk
        })
    }

    unsafe fn getParamValueByString(&self, id: ParamID, string: *mut TChar, value: *mut ParamValue) -> tresult {
        guard(kInternalError, || {
            let Some(info) = self.info(id) else { return kInvalidArgument };
            if string.is_null() || value.is_null() {
                return kInvalidArgument;
            }
            let Some(text) = (unsafe { read_wide(string) }) else { return kInvalidArgument };
            let Some(plain) = self.shared.param_from_text(id, &text) else { return kResultFalse };
            unsafe { value.write(info.normalise(plain)) };
            kResultOk
        })
    }

    unsafe fn normalizedParamToPlain(&self, id: ParamID, value: ParamValue) -> ParamValue {
        guard(0.0, || self.info(id).map_or(0.0, |info| info.plain(value)))
    }

    unsafe fn plainParamToNormalized(&self, id: ParamID, plain: ParamValue) -> ParamValue {
        guard(0.0, || self.info(id).map_or(0.0, |info| info.normalise(plain)))
    }

    unsafe fn getParamNormalized(&self, id: ParamID) -> ParamValue {
        guard(0.0, || {
            self.host.announce_latency_here();
            self.info(id).map_or(0.0, |info| info.normalise(self.shared.param(id)))
        })
    }

    unsafe fn setParamNormalized(&self, id: ParamID, value: ParamValue) -> tresult {
        guard(kInternalError, || {
            self.host.announce_latency_here();
            let Some(info) = self.info(id) else { return kInvalidArgument };
            self.shared.set_param(id, info.plain(value));
            kResultOk
        })
    }

    unsafe fn setComponentHandler(&self, handler: *mut IComponentHandler) -> tresult {
        guard(kInternalError, || {
            let handler = unsafe { ComRef::from_raw(handler) }.map(|h| h.to_com_ptr());
            let known = handler.is_some();
            *self.host.handler.lock().unwrap_or_else(|e| e.into_inner()) = handler;
            if known {
                *self.host.ui_thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::thread::current().id());
                self.shared.attach(self.host.cx.clone());
                self.host.deliver();
            }
            kResultOk
        })
    }

    unsafe fn createView(&self, name: FIDString) -> *mut IPlugView {
        guard(ptr::null_mut(), || {
            if name.is_null() || unsafe { CStr::from_ptr(name) } != c"editor" {
                return ptr::null_mut();
            }
            let view = ComWrapper::new(View::<P>::new(self));
            let Some(plug_view) = view.to_com_ptr::<IPlugView>() else { return ptr::null_mut() };
            view.sizing.this.store(plug_view.as_ptr(), Ordering::Release);
            plug_view.into_raw()
        })
    }
}

#[cfg(target_os = "linux")]
const PLATFORM_TYPE: &CStr = c"X11EmbedWindowID";
#[cfg(target_os = "windows")]
const PLATFORM_TYPE: &CStr = c"HWND";
#[cfg(target_os = "macos")]
const PLATFORM_TYPE: &CStr = c"NSView";
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
const PLATFORM_TYPE: &CStr = c"";

fn platform_type_supported(kind: FIDString) -> bool {
    !kind.is_null() && !PLATFORM_TYPE.is_empty() && unsafe { CStr::from_ptr(kind) } == PLATFORM_TYPE
}

/// The editor window. It holds no reference to the component, only to what the editor and the
/// latency announcement need, so the host may release the two in either order. Steinberg calls every
/// `IPlugView` method on the host's UI thread, which is also where `window` opens and closes.
struct View<P: Plugin> {
    shared: Arc<P::Shared>,
    host: Arc<Host>,
    #[cfg(target_os = "linux")]
    timer: Mutex<Option<Timer>>,
    window: Mutex<Option<editor::Window>>,
    sizing: Arc<Sizing>,
    /// What this view connected to the context, which may hold other views' too.
    resizer: Mutex<Option<ResizeId>>,
}

impl<P: Plugin> Class for View<P> {
    type Interfaces = (IPlugView, IPlugViewContentScaleSupport);
}

impl<P: Plugin> View<P> {
    fn new(component: &Component<P>) -> Self {
        View {
            shared: component.shared.clone(),
            host: component.host.clone(),
            #[cfg(target_os = "linux")]
            timer: Mutex::new(None),
            window: Mutex::new(None),
            sizing: Arc::new(Sizing {
                geometry: editor::Geometry::new::<P>(&component.shared),
                frame: Mutex::new(None),
                this: AtomicPtr::new(ptr::null_mut()),
                requested: Mutex::new(None),
            }),
            resizer: Mutex::new(None),
        }
    }

    fn open(&self, parent: *mut c_void, kind: FIDString) -> tresult {
        if !platform_type_supported(kind) {
            return kResultFalse;
        }
        let Ok(mut window) = self.window.try_lock() else { return kResultFalse };
        *window = None;
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let (parent, on_frame): (_, Box<dyn FnMut() + Send>) = {
            let (host, sizing) = (self.host.clone(), self.sizing.clone());
            #[cfg(target_os = "windows")]
            let parent = Parent::Win32(parent);
            #[cfg(target_os = "macos")]
            let parent = Parent::AppKit(parent);
            (
                parent,
                Box::new(move || {
                    host.tick();
                    sizing.tick();
                }),
            )
        };
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let (parent, on_frame): (_, Box<dyn FnMut() + Send>) = (Parent::X11(parent as usize as std::ffi::c_ulong), Box::new(|| {}));
        *window = editor::open(parent, self.sizing.geometry.clone(), self.shared.editor(self.host.cx.clone()), on_frame);
        if window.is_none() {
            return kResultFalse;
        }
        #[cfg(target_os = "linux")]
        self.start_timer();
        self.connect_resize();
        kResultOk
    }

    /// The window's own thread on Linux is not the host's UI thread, so the latency, the editor's
    /// changes and the sizes it asks for go out from a timer on the host's run loop instead. Without
    /// one the latency waits for the next `setActive(false)`, and the editor cannot ask for a size.
    #[cfg(target_os = "linux")]
    fn start_timer(&self) {
        let frame = self.sizing.frame.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(run_loop) = frame.and_then(|frame| frame.cast::<Linux::IRunLoop>()) else { return };
        let ticker = Ticker { host: self.host.clone(), sizing: self.sizing.clone() };
        let Some(handler) = ComWrapper::new(ticker).to_com_ptr::<Linux::ITimerHandler>() else { return };
        if unsafe { run_loop.registerTimer(handler.as_ptr(), TIMER_MS) } == kResultOk {
            *self.timer.lock().unwrap_or_else(|e| e.into_inner()) = Some(Timer { run_loop, handler });
        }
    }

    /// The editor may ask from any thread, and `resizeView` must be called on the UI thread, so the
    /// size waits there for the next tick.
    fn connect_resize(&self) {
        if !P::RESIZABLE {
            return;
        }
        #[cfg(target_os = "linux")]
        if self.timer.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
            return;
        }
        let sizing = self.sizing.clone();
        let id = self.host.cx.connect_resize(Box::new(move |size| {
            *sizing.requested.lock().unwrap_or_else(|e| e.into_inner()) = Some(sizing.geometry.fit(size));
            true
        }));
        self.disconnect_resize();
        *self.resizer.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
    }

    fn disconnect_resize(&self) {
        if let Some(id) = self.resizer.lock().unwrap_or_else(|e| e.into_inner()).take() {
            self.host.cx.disconnect_resize(id);
        }
    }

    /// What the editor did last reaches the host before the view is gone, e.g. the end of a gesture
    /// that would otherwise leave the host writing automation.
    fn close(&self) {
        #[cfg(target_os = "linux")]
        self.timer.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Ok(mut window) = self.window.try_lock() {
            window.take();
        }
        self.disconnect_resize();
        self.sizing.requested.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.host.tick();
    }
}

impl<P: Plugin> Drop for View<P> {
    fn drop(&mut self) {
        guard((), || self.close())
    }
}

/// What resizing the view needs on the UI thread, shared with what runs there for the open editor.
struct Sizing {
    geometry: editor::Geometry,
    frame: Mutex<Option<ComPtr<IPlugFrame>>>,
    /// This view as the host holds it, for `resizeView`; not a reference, so it does not keep itself alive.
    this: AtomicPtr<IPlugView>,
    /// What the editor asked for, at a scale of 1.0.
    requested: Mutex<Option<(u32, u32)>>,
}

impl Sizing {
    /// On the UI thread only. True when the host resized the view to `size`, in the platform's
    /// pixels; it may have told the view with `onSize` already.
    fn ask(&self, (width, height): (u32, u32)) -> bool {
        let frame = self.frame.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let this = self.this.load(Ordering::Acquire);
        let Some(frame) = frame.filter(|_| !this.is_null()) else { return false };
        let mut rect = ViewRect { left: 0, top: 0, right: width as int32, bottom: height as int32 };
        unsafe { frame.resizeView(this, &mut rect) == kResultOk }
    }

    /// What waits for the UI thread.
    fn tick(&self) {
        let Some(size) = self.requested.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
        if self.ask(self.geometry.of(size)) {
            self.geometry.resize(size);
        }
    }
}

/// About a frame, so a drag reaches the host's automation as smoothly as the editor draws it.
#[cfg(target_os = "linux")]
const TIMER_MS: u64 = 16;

#[cfg(target_os = "linux")]
struct Timer {
    run_loop: ComPtr<Linux::IRunLoop>,
    handler: ComPtr<Linux::ITimerHandler>,
}

#[cfg(target_os = "linux")]
impl Drop for Timer {
    fn drop(&mut self) {
        unsafe { self.run_loop.unregisterTimer(self.handler.as_ptr()) };
    }
}

#[cfg(target_os = "linux")]
struct Ticker {
    host: Arc<Host>,
    sizing: Arc<Sizing>,
}

#[cfg(target_os = "linux")]
impl Class for Ticker {
    type Interfaces = (Linux::ITimerHandler,);
}

#[cfg(target_os = "linux")]
impl Linux::ITimerHandlerTrait for Ticker {
    unsafe fn onTimer(&self) {
        // The host may unregister and release this handler from inside `restartComponent`.
        let (host, sizing) = (self.host.clone(), self.sizing.clone());
        guard((), || {
            host.tick();
            sizing.tick();
        })
    }
}

fn extent(rect: &ViewRect) -> (u32, u32) {
    (u32::try_from(rect.right - rect.left).unwrap_or(0), u32::try_from(rect.bottom - rect.top).unwrap_or(0))
}

impl<P: Plugin> IPlugViewTrait for View<P> {
    unsafe fn isPlatformTypeSupported(&self, kind: FIDString) -> tresult {
        guard(kInternalError, || if platform_type_supported(kind) { kResultTrue } else { kResultFalse })
    }

    unsafe fn attached(&self, parent: *mut c_void, kind: FIDString) -> tresult {
        guard(kInternalError, || self.open(parent, kind))
    }

    unsafe fn removed(&self) -> tresult {
        guard(kInternalError, || {
            self.close();
            kResultOk
        })
    }

    unsafe fn onWheel(&self, _distance: f32) -> tresult {
        kResultFalse
    }

    unsafe fn onKeyDown(&self, _key: char16, _key_code: int16, _modifiers: int16) -> tresult {
        kResultFalse
    }

    unsafe fn onKeyUp(&self, _key: char16, _key_code: int16, _modifiers: int16) -> tresult {
        kResultFalse
    }

    unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
        guard(kInternalError, || {
            if size.is_null() {
                return kInvalidArgument;
            }
            let (width, height) = self.sizing.geometry.physical();
            unsafe { size.write(ViewRect { left: 0, top: 0, right: width as int32, bottom: height as int32 }) };
            kResultOk
        })
    }

    /// Takes a size `checkSizeConstraint` would leave as it is, and refuses any other.
    unsafe fn onSize(&self, new_size: *mut ViewRect) -> tresult {
        guard(kInternalError, || {
            let Some(rect) = (unsafe { new_size.as_ref() }) else { return kInvalidArgument };
            let size = extent(rect);
            if self.sizing.geometry.fit_physical(size) != size {
                return kResultFalse;
            }
            self.sizing.geometry.resize_physical(size);
            kResultOk
        })
    }

    unsafe fn onFocus(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn setFrame(&self, frame: *mut IPlugFrame) -> tresult {
        guard(kInternalError, || {
            let frame = unsafe { ComRef::from_raw(frame) }.map(|f| f.to_com_ptr());
            *self.sizing.frame.lock().unwrap_or_else(|e| e.into_inner()) = frame;
            kResultOk
        })
    }

    unsafe fn canResize(&self) -> tresult {
        if P::RESIZABLE { kResultTrue } else { kResultFalse }
    }

    unsafe fn checkSizeConstraint(&self, rect: *mut ViewRect) -> tresult {
        guard(kInternalError, || {
            let Some(rect) = (unsafe { rect.as_mut() }) else { return kInvalidArgument };
            let (width, height) = self.sizing.geometry.fit_physical(extent(rect));
            rect.right = rect.left + width as int32;
            rect.bottom = rect.top + height as int32;
            kResultTrue
        })
    }
}

impl<P: Plugin> IPlugViewContentScaleSupportTrait for View<P> {
    unsafe fn setContentScaleFactor(&self, factor: IPlugViewContentScaleSupport_::ScaleFactor) -> tresult {
        guard(kInternalError, || {
            let geometry = &self.sizing.geometry;
            let before = geometry.physical();
            if !editor::HOST_SCALE || !geometry.set_scale(factor as f64) {
                return kResultFalse;
            }
            let open = self.window.try_lock().is_ok_and(|window| window.is_some());
            if open && geometry.physical() != before {
                self.sizing.ask(geometry.physical());
            }
            kResultTrue
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicI32;

    use vst3::Interface;

    use super::*;
    use crate::{Block, Info, Shared};

    struct Fixed;

    impl Shared for Fixed {
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
    }

    /// Doubles channel 0; activation `n` reports `latencies[n]`, and every block moves the latency to 7.
    struct Doubler {
        latencies: &'static [u32],
        activations: usize,
        latency: u32,
        changed: bool,
    }

    impl Plugin for Doubler {
        const INFO: Info = Info {
            name: "Doubler",
            vendor: "test",
            version: "0",
            clap_id: "test.doubler",
            description: "",
            clap_features: &[],
            vst3_class_id: *b"testDoublerClass",
            vst3_subcategories: "Fx",
            sidechain: false,
        };
        const EDITOR_SIZE: (u32, u32) = (1, 1);
        type Shared = Fixed;
        fn new() -> (Self, Arc<Fixed>) {
            (Doubler { latencies: &[5, 5, 9], activations: 0, latency: 0, changed: false }, Arc::new(Fixed))
        }
        fn activate(&mut self, _: f64, _: usize, _: usize) -> u32 {
            self.latency = self.latencies[self.activations.min(self.latencies.len() - 1)];
            self.activations += 1;
            self.latency
        }
        fn deactivate(&mut self) {}
        fn latency(&self) -> u32 {
            self.latency
        }
        fn process(&mut self, block: Block<'_>) {
            for s in block.channels[0].iter_mut() {
                *s *= 2.0;
            }
            self.latency = 7;
            self.changed = true;
        }
        fn take_latency_changed(&mut self) -> bool {
            std::mem::take(&mut self.changed)
        }
    }

    struct Handler(Arc<AtomicI32>);

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
            assert_eq!(flags, kLatencyChanged);
            self.0.fetch_add(1, Ordering::SeqCst);
            kResultOk
        }
    }

    fn create() -> (ComPtr<IComponent>, ComPtr<IAudioProcessor>, Arc<AtomicI32>) {
        unsafe {
            let factory = ComPtr::from_raw(factory::<Doubler>() as *mut IPluginFactory).unwrap();
            let mut obj = ptr::null_mut();
            let cid = class_id::<Doubler>();
            assert_eq!(factory.createInstance(cid.as_ptr(), IComponent::IID.as_ptr() as FIDString, &mut obj), kResultOk);
            let component = ComPtr::from_raw(obj as *mut IComponent).unwrap();
            let processor = component.cast::<IAudioProcessor>().unwrap();
            let controller = component.cast::<IEditController>().unwrap();
            let restarts = Arc::new(AtomicI32::new(0));
            let handler = ComWrapper::new(Handler(restarts.clone())).to_com_ptr::<IComponentHandler>().unwrap();
            assert_eq!(controller.setComponentHandler(handler.as_ptr()), kResultOk);
            (component, processor, restarts)
        }
    }

    fn setup(processor: &ComPtr<IAudioProcessor>) {
        let mut setup = ProcessSetup { processMode: 0, symbolicSampleSize: kSample32 as int32, maxSamplesPerBlock: 16, sampleRate: 48_000.0 };
        assert_eq!(unsafe { processor.setupProcessing(&mut setup) }, kResultOk);
    }

    /// One block with mono input in place on output 0.
    unsafe fn play(processor: &ComPtr<IAudioProcessor>, left: &mut [f32], right: &mut [f32]) -> tresult {
        unsafe {
            let mut in_ptrs = [left.as_mut_ptr()];
            let mut out_ptrs = [left.as_mut_ptr(), right.as_mut_ptr()];
            let mut input = AudioBusBuffers { numChannels: 1, silenceFlags: 0, __field0: AudioBusBuffers__type0 { channelBuffers32: in_ptrs.as_mut_ptr() } };
            let mut output = AudioBusBuffers { numChannels: 2, silenceFlags: 1, __field0: AudioBusBuffers__type0 { channelBuffers32: out_ptrs.as_mut_ptr() } };
            let mut data = ProcessData {
                processMode: 0,
                symbolicSampleSize: kSample32 as int32,
                numSamples: left.len() as int32,
                numInputs: 1,
                numOutputs: 1,
                inputs: &mut input,
                outputs: &mut output,
                inputParameterChanges: ptr::null_mut(),
                outputParameterChanges: ptr::null_mut(),
                inputEvents: ptr::null_mut(),
                outputEvents: ptr::null_mut(),
                processContext: ptr::null_mut(),
            };
            processor.process(&mut data)
        }
    }

    #[test]
    fn in_place_mono_is_processed_once_and_latency_changes_are_announced_from_the_ui_thread() {
        unsafe {
            let (component, processor, restarts) = create();
            let mut left = vec![0.5f32; 16];
            let mut right = vec![9.0f32; 16];
            assert_ne!(component.setActive(1), kResultOk, "not set up");
            setup(&processor);
            assert_ne!(play(&processor, &mut left, &mut right), kResultOk, "not active");
            assert_eq!(left[0], 0.5);

            assert_eq!(component.setActive(1), kResultOk);
            assert_eq!(processor.getLatencySamples(), 5);
            assert_eq!(restarts.load(Ordering::SeqCst), 1, "activation moved the latency from 0 to 5");

            assert_eq!(play(&processor, &mut left, &mut right), kResultOk);
            assert!(left.iter().all(|&s| s == 1.0), "{left:?}");
            assert!(right.iter().all(|&s| s == 9.0), "{right:?}");
            assert_eq!(processor.getLatencySamples(), 5, "process leaves the announcement to the main thread");
            assert_eq!(restarts.load(Ordering::SeqCst), 1);

            assert_eq!(component.getState(ptr::null_mut()), kInvalidArgument);
            assert_eq!(restarts.load(Ordering::SeqCst), 1, "saving may run off the UI thread, so it does not announce");
            assert_eq!(processor.getLatencySamples(), 5);

            assert_eq!(play(&processor, &mut left, &mut right), kResultOk);
            assert_eq!(component.setActive(0), kResultOk);
            assert_eq!(restarts.load(Ordering::SeqCst), 2, "deactivation announces the change from the last block");
            assert_eq!(processor.getLatencySamples(), 7);
            assert_eq!(component.setActive(0), kResultOk);
            assert_eq!(restarts.load(Ordering::SeqCst), 2);

            assert_eq!(component.setActive(1), kResultOk);
            assert_eq!((processor.getLatencySamples(), restarts.load(Ordering::SeqCst)), (5, 3));
            assert_eq!(component.setActive(0), kResultOk);
            assert_eq!(component.setActive(1), kResultOk);
            assert_eq!((processor.getLatencySamples(), restarts.load(Ordering::SeqCst)), (9, 4));
        }
    }

    /// A host without the editor open hears of a latency change at its next call on the UI thread
    /// that edits or reads the parameters or the state.
    #[test]
    fn without_a_view_the_next_ui_thread_call_announces_latency_changes() {
        unsafe {
            let (component, processor, restarts) = create();
            let controller = component.cast::<IEditController>().unwrap();
            setup(&processor);
            assert_eq!(component.setActive(1), kResultOk);
            let (mut left, mut right) = (vec![0.5f32; 16], vec![0.0f32; 16]);
            assert_eq!(play(&processor, &mut left, &mut right), kResultOk);
            assert_eq!((processor.getLatencySamples(), restarts.load(Ordering::SeqCst)), (5, 1));
            controller.getParamNormalized(0);
            assert_eq!((processor.getLatencySamples(), restarts.load(Ordering::SeqCst)), (7, 2));
            controller.getParamNormalized(0);
            assert_eq!(restarts.load(Ordering::SeqCst), 2, "nothing new to announce");

            assert_eq!(play(&processor, &mut left, &mut right), kResultOk);
            assert_eq!(controller.setParamNormalized(0, 0.5), kInvalidArgument);
            assert_eq!(restarts.load(Ordering::SeqCst), 3);

            assert_eq!(play(&processor, &mut left, &mut right), kResultOk);
            let elsewhere = std::thread::spawn({
                let controller = controller.clone();
                move || controller.getParamNormalized(0)
            });
            elsewhere.join().unwrap();
            assert_eq!(restarts.load(Ordering::SeqCst), 3, "off the UI thread it waits");
            assert_eq!(controller.setComponentState(ptr::null_mut()), kResultOk);
            assert_eq!(restarts.load(Ordering::SeqCst), 4);

            assert_eq!(play(&processor, &mut left, &mut right), kResultOk);
            assert_eq!(component.setState(ptr::null_mut()), kInvalidArgument);
            assert_eq!(restarts.load(Ordering::SeqCst), 5);
        }
    }

    #[cfg(target_os = "linux")]
    #[derive(Default)]
    struct RunLoop {
        timer: Mutex<Option<ComPtr<Linux::ITimerHandler>>>,
        unregistered: AtomicI32,
        /// Resizes the view as asked, as a host does, rather than refuse.
        resizes: AtomicBool,
        asked: Mutex<Vec<(i32, i32)>>,
    }

    #[cfg(target_os = "linux")]
    impl Class for RunLoop {
        type Interfaces = (IPlugFrame, Linux::IRunLoop);
    }

    #[cfg(target_os = "linux")]
    impl IPlugFrameTrait for RunLoop {
        unsafe fn resizeView(&self, view: *mut IPlugView, rect: *mut ViewRect) -> tresult {
            let rect = unsafe { &mut *rect };
            self.asked.lock().unwrap().push((rect.right - rect.left, rect.bottom - rect.top));
            if !self.resizes.load(Ordering::SeqCst) {
                return kResultFalse;
            }
            unsafe { ComRef::<IPlugView>::from_raw(view).unwrap().onSize(rect) }
        }
    }

    #[cfg(target_os = "linux")]
    impl Linux::IRunLoopTrait for RunLoop {
        unsafe fn registerEventHandler(&self, _: *mut Linux::IEventHandler, _: Linux::FileDescriptor) -> tresult {
            kNotImplemented
        }
        unsafe fn unregisterEventHandler(&self, _: *mut Linux::IEventHandler) -> tresult {
            kNotImplemented
        }
        unsafe fn registerTimer(&self, handler: *mut Linux::ITimerHandler, milliseconds: Linux::TimerInterval) -> tresult {
            assert!((1..=100).contains(&milliseconds), "{milliseconds}");
            *self.timer.lock().unwrap() = unsafe { ComRef::from_raw(handler) }.map(|h| h.to_com_ptr());
            kResultOk
        }
        unsafe fn unregisterTimer(&self, handler: *mut Linux::ITimerHandler) -> tresult {
            let mut timer = self.timer.lock().unwrap();
            assert_eq!(timer.as_ref().map(|t| t.as_ptr()), Some(handler));
            *timer = None;
            self.unregistered.fetch_add(1, Ordering::SeqCst);
            kResultOk
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_open_view_announces_latency_changes_from_a_timer_on_the_hosts_run_loop() {
        use Linux::ITimerHandlerTrait;
        unsafe {
            let wrapper = ComWrapper::new(Component::<Doubler>::new());
            let component = wrapper.to_com_ptr::<IComponent>().unwrap();
            let processor = wrapper.to_com_ptr::<IAudioProcessor>().unwrap();
            let restarts = Arc::new(AtomicI32::new(0));
            let handler = ComWrapper::new(Handler(restarts.clone())).to_com_ptr::<IComponentHandler>().unwrap();
            assert_eq!(wrapper.to_com_ptr::<IEditController>().unwrap().setComponentHandler(handler.as_ptr()), kResultOk);
            setup(&processor);
            assert_eq!(component.setActive(1), kResultOk);
            let (mut left, mut right) = (vec![0.5f32; 16], vec![0.0f32; 16]);
            assert_eq!(play(&processor, &mut left, &mut right), kResultOk);
            assert_eq!((processor.getLatencySamples(), restarts.load(Ordering::SeqCst)), (5, 1));

            let view = ComWrapper::new(View::<Doubler>::new(&wrapper));
            let plug_view = view.to_com_ptr::<IPlugView>().unwrap();
            view.start_timer();
            let run_loop = ComWrapper::new(RunLoop::default());
            assert_eq!(plug_view.setFrame(run_loop.to_com_ptr::<IPlugFrame>().unwrap().as_ptr()), kResultOk);
            view.start_timer();
            let timer = run_loop.timer.lock().unwrap().clone().expect("a timer on the host's run loop");
            timer.onTimer();
            assert_eq!((processor.getLatencySamples(), restarts.load(Ordering::SeqCst)), (7, 2));
            timer.onTimer();
            assert_eq!(restarts.load(Ordering::SeqCst), 2, "nothing new to announce");

            drop((component, processor, wrapper));
            timer.onTimer();
            assert_eq!(plug_view.removed(), kResultOk);
            assert_eq!(run_loop.unregistered.load(Ordering::SeqCst), 1);
            assert!(run_loop.timer.lock().unwrap().is_none());
            assert_eq!(plug_view.setFrame(ptr::null_mut()), kResultOk);
        }
    }

    #[test]
    fn a_host_cannot_change_buses_or_setup_while_active() {
        unsafe {
            let (component, processor, _) = create();
            setup(&processor);
            assert_eq!(component.setActive(1), kResultOk);
            let (mut stereo, mut out) = (SpeakerArr::kStereo, SpeakerArr::kStereo);
            assert_eq!(processor.setBusArrangements(&mut stereo, 1, &mut out, 1), kResultFalse);
            let mut setup = ProcessSetup { processMode: 0, symbolicSampleSize: kSample32 as int32, maxSamplesPerBlock: 32, sampleRate: 44_100.0 };
            assert_eq!(processor.setupProcessing(&mut setup), kResultFalse);
            assert_eq!(processor.process(ptr::null_mut()), kInvalidArgument);
            assert_eq!(processor.setBusArrangements(ptr::null_mut(), 1, ptr::null_mut(), 1), kResultFalse);
        }
    }

    /// One parameter from 0 to 10; keeps the context it is attached to.
    #[derive(Default)]
    struct Attached(Mutex<Option<GuiContext>>);

    impl Shared for Attached {
        fn params(&self) -> &'static [ParamInfo] {
            &[ParamInfo { id: 3, name: "Level", kind: Kind::Float { min: 0.0, max: 10.0, map: crate::Map::Linear }, default: 0.0, automatable: true }]
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
        fn editor(&self, _: GuiContext) -> Box<dyn FnMut(&mut egui::Ui) + Send> {
            Box::new(|_| {})
        }
        fn attach(&self, cx: GuiContext) {
            *self.0.lock().unwrap() = Some(cx);
        }
    }

    struct Leveller;

    impl Plugin for Leveller {
        const INFO: Info = Doubler::INFO;
        const EDITOR_SIZE: (u32, u32) = (1, 1);
        type Shared = Attached;
        fn new() -> (Self, Arc<Attached>) {
            (Leveller, Arc::default())
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

    #[derive(Default)]
    struct Edits(Mutex<Vec<String>>);

    impl Class for Edits {
        type Interfaces = (IComponentHandler,);
    }

    impl IComponentHandlerTrait for Edits {
        unsafe fn beginEdit(&self, id: ParamID) -> tresult {
            self.0.lock().unwrap().push(format!("begin {id}"));
            kResultOk
        }
        unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
            self.0.lock().unwrap().push(format!("perform {id} {value}"));
            kResultOk
        }
        unsafe fn endEdit(&self, id: ParamID) -> tresult {
            self.0.lock().unwrap().push(format!("end {id}"));
            kResultOk
        }
        unsafe fn restartComponent(&self, flags: int32) -> tresult {
            self.0.lock().unwrap().push(format!("restart {flags}"));
            kResultOk
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_gesture_from_another_thread_waits_for_the_ui_thread() {
        use Linux::ITimerHandlerTrait;
        unsafe {
            let wrapper = ComWrapper::new(Component::<Leveller>::new());
            let edits = ComWrapper::new(Edits::default());
            let controller = wrapper.to_com_ptr::<IEditController>().unwrap();
            assert_eq!(controller.setComponentHandler(edits.to_com_ptr::<IComponentHandler>().unwrap().as_ptr()), kResultOk);
            let cx = wrapper.shared.0.lock().unwrap().clone().expect("attached once the handler is known");
            std::thread::spawn(move || {
                cx.begin(3);
                cx.set(3, 2.5);
                cx.end(3);
                cx.mark_dirty();
            })
            .join()
            .unwrap();
            assert!(edits.0.lock().unwrap().is_empty(), "nothing reaches the handler off the UI thread");

            let view = ComWrapper::new(View::<Leveller>::new(&wrapper));
            let plug_view = view.to_com_ptr::<IPlugView>().unwrap();
            let run_loop = ComWrapper::new(RunLoop::default());
            assert_eq!(plug_view.setFrame(run_loop.to_com_ptr::<IPlugFrame>().unwrap().as_ptr()), kResultOk);
            view.start_timer();
            let timer = run_loop.timer.lock().unwrap().clone().expect("a timer on the host's run loop");
            timer.onTimer();
            assert_eq!(*edits.0.lock().unwrap(), ["begin 3", "perform 3 0.25", "end 3", &format!("restart {kParamValuesChanged}")]);
            assert_eq!(plug_view.removed(), kResultOk);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn another_instances_open_view_delivers_a_gesture_made_off_the_ui_thread() {
        use Linux::ITimerHandlerTrait;
        unsafe {
            let (shown, written) = (ComWrapper::new(Component::<Leveller>::new()), ComWrapper::new(Component::<Leveller>::new()));
            let (unused, edits) = (ComWrapper::new(Edits::default()), ComWrapper::new(Edits::default()));
            let handler = |edits: &ComWrapper<Edits>| edits.to_com_ptr::<IComponentHandler>().unwrap();
            assert_eq!(shown.to_com_ptr::<IEditController>().unwrap().setComponentHandler(handler(&unused).as_ptr()), kResultOk);
            assert_eq!(written.to_com_ptr::<IEditController>().unwrap().setComponentHandler(handler(&edits).as_ptr()), kResultOk);
            let cx = written.shared.0.lock().unwrap().clone().expect("attached once the handler is known");

            let view = ComWrapper::new(View::<Leveller>::new(&shown));
            let plug_view = view.to_com_ptr::<IPlugView>().unwrap();
            let run_loop = ComWrapper::new(RunLoop::default());
            assert_eq!(plug_view.setFrame(run_loop.to_com_ptr::<IPlugFrame>().unwrap().as_ptr()), kResultOk);
            view.start_timer();
            std::thread::spawn(move || {
                cx.begin(3);
                cx.set(3, 2.5);
                cx.end(3);
            })
            .join()
            .unwrap();
            assert!(edits.0.lock().unwrap().is_empty(), "nothing reaches the handler off the UI thread");
            let timer = run_loop.timer.lock().unwrap().clone().expect("a timer on the host's run loop");
            timer.onTimer();
            assert_eq!(*edits.0.lock().unwrap(), ["begin 3", "perform 3 0.25", "end 3"]);
            assert!(unused.0.lock().unwrap().is_empty());
            assert_eq!(plug_view.removed(), kResultOk);
        }
    }

    #[test]
    fn removing_the_view_delivers_what_the_editor_did_last() {
        unsafe {
            let wrapper = ComWrapper::new(Component::<Leveller>::new());
            let edits = ComWrapper::new(Edits::default());
            let controller = wrapper.to_com_ptr::<IEditController>().unwrap();
            assert_eq!(controller.setComponentHandler(edits.to_com_ptr::<IComponentHandler>().unwrap().as_ptr()), kResultOk);
            let cx = wrapper.shared.0.lock().unwrap().clone().expect("attached once the handler is known");
            let view = ComWrapper::new(View::<Leveller>::new(&wrapper));
            let plug_view = view.to_com_ptr::<IPlugView>().unwrap();
            std::thread::spawn(move || {
                cx.begin(3);
                cx.set(3, 2.5);
                cx.end(3);
            })
            .join()
            .unwrap();
            assert!(edits.0.lock().unwrap().is_empty(), "nothing reaches the handler off the UI thread");
            assert_eq!(plug_view.removed(), kResultOk);
            assert_eq!(*edits.0.lock().unwrap(), ["begin 3", "perform 3 0.25", "end 3"], "the gesture is not left open");
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
        fn editor(&self, _: GuiContext) -> Box<dyn FnMut(&mut egui::Ui) + Send> {
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

    fn rect(view: &ComPtr<IPlugView>) -> (i32, i32) {
        let mut rect = ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
        assert_eq!(unsafe { view.getSize(&mut rect) }, kResultOk);
        (rect.right - rect.left, rect.bottom - rect.top)
    }

    #[cfg_attr(target_os = "macos", ignore = "macOS takes no scale from the host")]
    #[test]
    fn vst3_check_size_constraint_clamps() {
        unsafe {
            let wrapper = ComWrapper::new(Component::<Wide>::new());
            let controller = wrapper.to_com_ptr::<IEditController>().unwrap();
            let view = ComPtr::<IPlugView>::from_raw(controller.createView(c"editor".as_ptr())).expect("an editor view");
            assert_eq!(view.canResize(), kResultTrue);
            assert_eq!(rect(&view), (1000, 500), "the size the plugin kept");

            let mut wanted = ViewRect { left: 10, top: 20, right: 30, bottom: 40 };
            assert_eq!(view.checkSizeConstraint(&mut wanted), kResultTrue);
            assert_eq!((wanted.left, wanted.top, wanted.right, wanted.bottom), (10, 20, 770, 460));
            let mut wanted = ViewRect { left: 0, top: 0, right: 2000, bottom: 300 };
            assert_eq!(view.checkSizeConstraint(&mut wanted), kResultTrue);
            assert_eq!((wanted.right, wanted.bottom), (2000, 440));

            let scaling = view.cast::<IPlugViewContentScaleSupport>().unwrap();
            assert_eq!(scaling.setContentScaleFactor(2.0), kResultTrue);
            let mut wanted = ViewRect { left: 0, top: 0, right: 10, bottom: 10 };
            assert_eq!(view.checkSizeConstraint(&mut wanted), kResultTrue);
            assert_eq!((wanted.right, wanted.bottom), (1520, 880), "at the host's scale");
            let mut small = ViewRect { left: 0, top: 0, right: 1520, bottom: 879 };
            assert_eq!(view.onSize(&mut small), kResultFalse);
            let mut large = ViewRect { left: 5, top: 5, right: 2005, bottom: 1205 };
            assert_eq!(view.onSize(&mut large), kResultOk);
            assert_eq!(rect(&view), (2000, 1200));
            assert_eq!(wrapper.shared.editor_size(), Some((1000, 600)), "kept at a scale of 1.0");

            let again = ComPtr::<IPlugView>::from_raw(controller.createView(c"editor".as_ptr())).unwrap();
            assert_eq!(rect(&again), (1000, 600), "a new view opens at the size the last was left at");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_editor_asks_for_a_size_from_the_hosts_ui_thread() {
        use Linux::ITimerHandlerTrait;
        unsafe {
            let wrapper = ComWrapper::new(Component::<Wide>::new());
            let view = ComWrapper::new(View::<Wide>::new(&wrapper));
            let plug_view = view.to_com_ptr::<IPlugView>().unwrap();
            view.sizing.this.store(plug_view.as_ptr(), Ordering::Release);
            let run_loop = ComWrapper::new(RunLoop::default());
            run_loop.resizes.store(true, Ordering::SeqCst);
            assert_eq!(plug_view.setFrame(run_loop.to_com_ptr::<IPlugFrame>().unwrap().as_ptr()), kResultOk);
            let scaling = plug_view.cast::<IPlugViewContentScaleSupport>().unwrap();
            assert_eq!(scaling.setContentScaleFactor(1.5), kResultTrue);
            let cx = wrapper.host.cx.clone();
            assert!(!cx.request_resize(900, 900), "nothing to resize without a window");

            // What `attached` does once the window is open, which needs a display.
            view.start_timer();
            view.connect_resize();
            let timer = run_loop.timer.lock().unwrap().clone().expect("a timer on the host's run loop");
            let asking = cx.clone();
            assert!(std::thread::spawn(move || asking.request_resize(100, 2000)).join().unwrap());
            assert!(run_loop.asked.lock().unwrap().is_empty(), "the host is asked on its UI thread only");
            timer.onTimer();
            assert_eq!(*run_loop.asked.lock().unwrap(), [(1140, 3000)]);
            assert_eq!(rect(&plug_view), (1140, 3000));
            assert_eq!(wrapper.shared.editor_size(), Some((760, 2000)));
            timer.onTimer();
            assert_eq!(run_loop.asked.lock().unwrap().len(), 1, "asked once");

            run_loop.resizes.store(false, Ordering::SeqCst);
            assert!(cx.request_resize(900, 900));
            timer.onTimer();
            assert_eq!(rect(&plug_view), (1140, 3000), "a refused size is not taken");

            assert_eq!(plug_view.removed(), kResultOk);
            assert!(!cx.request_resize(900, 900), "nothing to resize once the view is removed");
            assert_eq!(plug_view.setFrame(ptr::null_mut()), kResultOk);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn closing_one_view_leaves_another_views_resize_request_connected() {
        use Linux::ITimerHandlerTrait;
        unsafe {
            let wrapper = ComWrapper::new(Component::<Wide>::new());
            let open = || {
                let view = ComWrapper::new(View::<Wide>::new(&wrapper));
                let plug_view = view.to_com_ptr::<IPlugView>().unwrap();
                view.sizing.this.store(plug_view.as_ptr(), Ordering::Release);
                let run_loop = ComWrapper::new(RunLoop::default());
                run_loop.resizes.store(true, Ordering::SeqCst);
                assert_eq!(plug_view.setFrame(run_loop.to_com_ptr::<IPlugFrame>().unwrap().as_ptr()), kResultOk);
                view.start_timer();
                view.connect_resize();
                (view, plug_view, run_loop)
            };
            let (_first, first_view, first_loop) = open();
            let (_second, second_view, second_loop) = open();
            let cx = wrapper.host.cx.clone();

            assert_eq!(second_view.removed(), kResultOk);
            assert!(cx.request_resize(900, 900), "the first view still takes the request");
            first_loop.timer.lock().unwrap().clone().expect("the first view's timer").onTimer();
            assert_eq!(first_loop.asked.lock().unwrap().len(), 1);
            assert!(second_loop.asked.lock().unwrap().is_empty());

            assert_eq!(first_view.removed(), kResultOk);
            assert!(!cx.request_resize(900, 900), "nothing to resize once every view is removed");
        }
    }

    struct StereoFirst(Doubler);

    impl Plugin for StereoFirst {
        const INFO: Info = Doubler::INFO;
        const EDITOR_SIZE: (u32, u32) = (1, 1);
        const LAYOUTS: &'static [crate::Layout] =
            &[crate::Layout { name: "Stereo", main_inputs: 2 }, crate::Layout { name: "Mono in, stereo out", main_inputs: 1 }];
        type Shared = Fixed;
        fn new() -> (Self, Arc<Fixed>) {
            (StereoFirst(Doubler { latencies: &[0], activations: 0, latency: 0, changed: false }), Arc::new(Fixed))
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

    fn default_input_arrangement<P: Plugin>() -> SpeakerArrangement {
        unsafe {
            let factory = ComPtr::from_raw(factory::<P>() as *mut IPluginFactory).unwrap();
            let mut obj = ptr::null_mut();
            let cid = class_id::<P>();
            assert_eq!(factory.createInstance(cid.as_ptr(), IComponent::IID.as_ptr() as FIDString, &mut obj), kResultOk);
            let component = ComPtr::from_raw(obj as *mut IComponent).unwrap();
            let processor = component.cast::<IAudioProcessor>().unwrap();
            let mut arrangement = 0;
            assert_eq!(processor.getBusArrangement(kInput as BusDirection, 0, &mut arrangement), kResultOk);
            let mut info: BusInfo = std::mem::zeroed();
            assert_eq!(component.getBusInfo(kAudio as MediaType, kInput as BusDirection, 0, &mut info), kResultOk);
            assert_eq!(info.channelCount, arrangement.count_ones() as int32);
            arrangement
        }
    }

    #[test]
    fn vst3_default_layout_is_the_first() {
        assert_eq!(default_input_arrangement::<StereoFirst>(), SpeakerArr::kStereo);
        assert_eq!(default_input_arrangement::<Doubler>(), SpeakerArr::kMono);
    }

    /// Notes whether subnormal results flush to zero while a parameter is set.
    struct Probe;

    static SET_FLUSHED: AtomicBool = AtomicBool::new(false);

    impl Shared for Probe {
        fn params(&self) -> &'static [ParamInfo] {
            &[ParamInfo { id: 3, name: "Level", kind: Kind::Float { min: 0.0, max: 10.0, map: crate::Map::Linear }, default: 0.0, automatable: true }]
        }
        fn param(&self, _: u32) -> f64 {
            0.0
        }
        fn set_param(&self, _: u32, _: f64) {
            let product = std::hint::black_box(1e-36f32) * std::hint::black_box(1e-3f32);
            SET_FLUSHED.store(product == 0.0, Ordering::SeqCst);
        }
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
    }

    struct Probing;

    impl Plugin for Probing {
        const INFO: Info = Doubler::INFO;
        const EDITOR_SIZE: (u32, u32) = (1, 1);
        type Shared = Probe;
        fn new() -> (Self, Arc<Probe>) {
            (Probing, Arc::new(Probe))
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

    struct Level;

    impl Class for Level {
        type Interfaces = (IParamValueQueue,);
    }

    impl IParamValueQueueTrait for Level {
        unsafe fn getParameterId(&self) -> ParamID {
            3
        }
        unsafe fn getPointCount(&self) -> int32 {
            1
        }
        unsafe fn getPoint(&self, _: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
            unsafe {
                *offset = 0;
                *value = 0.5;
            }
            kResultOk
        }
        unsafe fn addPoint(&self, _: int32, _: ParamValue, _: *mut int32) -> tresult {
            kResultFalse
        }
    }

    struct OneLevel(ComWrapper<Level>);

    impl Class for OneLevel {
        type Interfaces = (IParameterChanges,);
    }

    impl IParameterChangesTrait for OneLevel {
        unsafe fn getParameterCount(&self) -> int32 {
            1
        }
        unsafe fn getParameterData(&self, _: int32) -> *mut IParamValueQueue {
            self.0.as_com_ref::<IParamValueQueue>().map_or(ptr::null_mut(), |queue| queue.as_ptr())
        }
        unsafe fn addParameterData(&self, _: *const ParamID, _: *mut int32) -> *mut IParamValueQueue {
            ptr::null_mut()
        }
    }

    #[test]
    fn parameter_changes_are_applied_with_subnormals_flushed() {
        let host_product = || std::hint::black_box(1e-36f32) * std::hint::black_box(1e-3f32);
        unsafe {
            let factory = ComPtr::from_raw(factory::<Probing>() as *mut IPluginFactory).unwrap();
            let mut obj = ptr::null_mut();
            let cid = class_id::<Probing>();
            assert_eq!(factory.createInstance(cid.as_ptr(), IComponent::IID.as_ptr() as FIDString, &mut obj), kResultOk);
            let component = ComPtr::from_raw(obj as *mut IComponent).unwrap();
            let processor = component.cast::<IAudioProcessor>().unwrap();
            let changes = ComWrapper::new(OneLevel(ComWrapper::new(Level))).to_com_ptr::<IParameterChanges>().unwrap();
            let mut data = ProcessData {
                processMode: 0,
                symbolicSampleSize: kSample32 as int32,
                numSamples: 0,
                numInputs: 0,
                numOutputs: 0,
                inputs: ptr::null_mut(),
                outputs: ptr::null_mut(),
                inputParameterChanges: changes.as_ptr(),
                outputParameterChanges: ptr::null_mut(),
                inputEvents: ptr::null_mut(),
                outputEvents: ptr::null_mut(),
                processContext: ptr::null_mut(),
            };
            SET_FLUSHED.store(false, Ordering::SeqCst);
            assert_eq!(processor.process(&mut data), kResultOk);
            assert!(SET_FLUSHED.load(Ordering::SeqCst), "applied from process");
            assert!(host_product().is_subnormal(), "the host's floating-point state is back");
        }
    }
}
