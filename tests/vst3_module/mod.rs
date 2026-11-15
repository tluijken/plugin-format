//! Loads a VST3 module and calls its platform's entry point, as a host does before anything else.

use vst3::Steinberg::Vst::{IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentTrait, IEditController};
use vst3::Steinberg::{IPluginBaseTrait, IPluginFactory, IPluginFactoryTrait, PClassInfo, TUID, kResultOk};
use vst3::{ComPtr, Interface};

use crate::built::Built;

/// Stays loaded for the whole run, as it would in a host.
pub fn load(built: &Built) -> libloading::Library {
    unsafe {
        let library = libloading::Library::new(&built.binary).unwrap();
        #[cfg(windows)]
        {
            let entry: libloading::Symbol<extern "system" fn() -> bool> = library.get(b"InitDll\0").unwrap();
            assert!(entry());
        }
        #[cfg(target_os = "macos")]
        {
            let entry: libloading::Symbol<extern "system" fn(*mut std::ffi::c_void) -> bool> = library.get(b"bundleEntry\0").unwrap();
            assert!(mac::with_bundle(&built.path, |bundle| entry(bundle)));
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let entry: libloading::Symbol<extern "system" fn(*mut std::ffi::c_void) -> bool> = library.get(b"ModuleEntry\0").unwrap();
            assert!(entry(std::ptr::null_mut()));
        }
        library
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    use std::path::Path;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(allocator: *const c_void, buffer: *const u8, length: isize, is_directory: u8) -> *const c_void;
        fn CFBundleCreate(allocator: *const c_void, url: *const c_void) -> *mut c_void;
        fn CFRelease(cf: *const c_void);
    }

    /// Calls `f` with a `CFBundleRef` for `bundle`, as `bundleEntry` takes. The module retains it if
    /// it keeps it.
    pub fn with_bundle(bundle: &Path, f: impl FnOnce(*mut c_void) -> bool) -> bool {
        let bytes = bundle.as_os_str().as_encoded_bytes();
        unsafe {
            let url = CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, 1);
            assert!(!url.is_null());
            let cf_bundle = CFBundleCreate(std::ptr::null(), url);
            assert!(!cf_bundle.is_null(), "no bundle at {}", bundle.display());
            let entered = f(cf_bundle);
            CFRelease(cf_bundle);
            CFRelease(url);
            entered
        }
    }
}

/// The library's factory, as `GetPluginFactory` gives it.
pub fn factory(library: &libloading::Library) -> ComPtr<IPluginFactory> {
    unsafe {
        let get: libloading::Symbol<extern "system" fn() -> *mut IPluginFactory> = library.get(b"GetPluginFactory\0").unwrap();
        ComPtr::from_raw(get()).expect("a factory")
    }
}

/// The class id of the library's first class.
pub fn class_id(library: &libloading::Library) -> TUID {
    unsafe {
        let mut info: PClassInfo = std::mem::zeroed();
        assert_eq!(factory(library).getClassInfo(0, &mut info), kResultOk);
        info.cid
    }
}

/// One instance of the library's plugin, as a host holds it.
pub struct Instance {
    pub component: ComPtr<IComponent>,
    pub processor: ComPtr<IAudioProcessor>,
    pub controller: ComPtr<IEditController>,
}

impl Instance {
    /// Creates and initialises an instance of the library's first class.
    pub fn create(library: &libloading::Library) -> Instance {
        unsafe {
            let mut obj = std::ptr::null_mut();
            let result = factory(library).createInstance(class_id(library).as_ptr(), IComponent::IID.as_ptr() as vst3::Steinberg::FIDString, &mut obj);
            assert_eq!(result, kResultOk);
            let component = ComPtr::from_raw(obj as *mut IComponent).expect("a component");
            assert_eq!(component.initialize(std::ptr::null_mut()), kResultOk);
            let processor = component.cast::<IAudioProcessor>().expect("IAudioProcessor");
            let controller = component.cast::<IEditController>().expect("IEditController");
            Instance { component, processor, controller }
        }
    }

    pub fn stop(&self) {
        unsafe {
            self.processor.setProcessing(0);
            assert_eq!(self.component.setActive(0), kResultOk);
            assert_eq!(self.component.terminate(), kResultOk);
        }
    }
}
