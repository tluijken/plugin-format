//! What a CLAP host does with a plugin library: loads it, creates and initialises a plugin from it.

#![allow(dead_code, reason = "each test crate uses part of this")]

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;

use clap_sys::entry::clap_plugin_entry;
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;
use clap_sys::version::CLAP_VERSION;

type Extension = unsafe extern "C" fn(*const clap_host, *const c_char) -> *const c_void;
type Callback = unsafe extern "C" fn(*const clap_host);

unsafe extern "C" fn nothing(_: *const clap_host) {}

/// A host that answers `get_extension` and `request_callback` as given and ignores its other requests.
pub const fn host(get_extension: Option<Extension>, request_callback: Option<Callback>) -> clap_host {
    clap_host {
        clap_version: CLAP_VERSION,
        host_data: ptr::null_mut(),
        name: c"plugin-format test host".as_ptr(),
        vendor: c"".as_ptr(),
        url: c"".as_ptr(),
        version: c"0".as_ptr(),
        get_extension,
        request_restart: Some(nothing),
        request_process: Some(nothing),
        request_callback,
    }
}

/// The entry of the library at `binary`, which stays loaded for the whole run, as it would in a host.
/// One library per test crate: later calls get the first one's entry.
///
/// # Safety
/// `binary` is a CLAP plugin library.
pub unsafe fn load(binary: &Path) -> &'static clap_plugin_entry {
    static LIBRARY: OnceLock<libloading::Library> = OnceLock::new();
    unsafe {
        let library = LIBRARY.get_or_init(|| libloading::Library::new(binary).unwrap());
        let entry: libloading::Symbol<*const clap_plugin_entry> = library.get(b"clap_entry\0").unwrap();
        &**entry
    }
}

/// Initialises `entry` with `path`, then creates and initialises the library's first plugin.
///
/// # Safety
/// `entry` is a CLAP entry that has not been initialised, and `host` outlives the plugin.
pub unsafe fn create(entry: &clap_plugin_entry, path: &CStr, host: &clap_host) -> *const clap_plugin {
    unsafe {
        assert!((entry.init.unwrap())(path.as_ptr()));
        let factory = (entry.get_factory.unwrap())(CLAP_PLUGIN_FACTORY_ID.as_ptr()) as *const clap_plugin_factory;
        let desc = ((*factory).get_plugin_descriptor.unwrap())(factory, 0);
        let plugin = ((*factory).create_plugin.unwrap())(factory, host, (*desc).id);
        assert!(!plugin.is_null());
        assert!(((*plugin).init.unwrap())(plugin));
        plugin
    }
}

/// `create` with the path of a built plugin.
///
/// # Safety
/// As `create`.
pub unsafe fn create_at(entry: &clap_plugin_entry, path: &Path, host: &clap_host) -> *const clap_plugin {
    unsafe { create(entry, &CString::new(path.to_str().unwrap()).unwrap(), host) }
}
