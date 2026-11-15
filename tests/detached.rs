//! A plugin's tests can give its editor a `GuiContext` without any host behind it.

use gain::{GAIN, TEST_GESTURE};
use plugin_format::{GuiContext, Plugin, Shared};

#[test]
fn a_detached_context_changes_the_shared_half_and_asks_no_host() {
    let (_, shared) = gain::Gain::new();
    let cx = GuiContext::detached(&shared);

    cx.begin(GAIN);
    cx.set(GAIN, 7.0);
    cx.end(GAIN);
    cx.mark_dirty();
    assert_eq!(shared.param(GAIN), 7.0);
    assert!(!cx.request_resize(900, 900), "no editor window to resize");
    assert_eq!(cx.track(), None);
}

#[test]
fn what_a_detached_context_was_attached_for_still_reaches_the_shared_half() {
    let (_, shared) = gain::Gain::new();
    shared.attach(GuiContext::detached(&shared));
    shared.set_param(TEST_GESTURE, 1.0);
    assert_eq!(shared.param(GAIN), -6.0);
}

#[test]
fn a_detached_context_does_not_keep_the_shared_half_alive() {
    let (_, shared) = gain::Gain::new();
    let cx = GuiContext::detached(&shared);
    drop(shared);
    cx.set(GAIN, 3.0);
    cx.begin(GAIN);
}
