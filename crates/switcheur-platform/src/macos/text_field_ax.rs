//! Expose GPUI text inputs to the Accessibility API.
//!
//! GPUI draws everything in Metal and its `GPUIView` implements no
//! `NSAccessibility` methods, so `AXFocusedUIElement` never resolves to a text
//! field. Dictation tools (MacWhisper…) look for a focused `AXTextField`
//! before inserting text and stay silent over our panel.
//!
//! [`install`] adds a few accessibility methods to `GPUIView` at runtime. The
//! view reports itself as a text field only while GPUI has a focused input
//! handler (`selectedRange` valid). Reads go through GPUI's
//! `NSTextInputClient` methods; `AXSelectedText` / `AXValue` writes go through
//! `insertText:replacementRange:`, the same path as typed text.
//!
//! MacWhisper also ignores processes without a bundle id: test dictation from
//! the `.app` bundle, not `cargo run`.

use std::ffi::{c_char, c_void, CStr};
use std::mem::transmute;

use objc2::ffi::{class_addMethod, objc_release, NSInteger, NSUInteger};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::{msg_send, sel, Encode, Encoding};
use objc2_app_kit::NSApplication;
use objc2_foundation::{MainThreadMarker, NSRect, NSString};

const NS_NOT_FOUND: NSUInteger = isize::MAX as NSUInteger;

/// `NSRange` as GPUI declares it (`{NSRange=QQ}`). objc2's own `NSRange`
/// encodes as `{_NSRange=QQ}` and its debug-mode type check rejects the send.
#[repr(C)]
#[derive(Clone, Copy)]
struct NSRange {
    location: NSUInteger,
    length: NSUInteger,
}

impl NSRange {
    const fn new(location: NSUInteger, length: NSUInteger) -> Self {
        Self { location, length }
    }
}

unsafe impl Encode for NSRange {
    const ENCODING: Encoding =
        Encoding::Struct("NSRange", &[NSUInteger::ENCODING, NSUInteger::ENCODING]);
}

unsafe fn selected_range(this: &AnyObject) -> NSRange {
    unsafe { msg_send![this, selectedRange] }
}

unsafe fn has_input(this: &AnyObject) -> bool {
    unsafe { selected_range(this).location != NS_NOT_FOUND }
}

/// Whole input text. GPUI's `text_for_range` clamps out-of-range ends.
unsafe fn input_text(this: &AnyObject) -> String {
    let mut actual = NSRange::new(0, 0);
    let attributed: *mut AnyObject = unsafe {
        msg_send![
            this,
            attributedSubstringForProposedRange: NSRange::new(0, 1 << 30),
            actualRange: &mut actual as *mut NSRange as *mut c_void
        ]
    };
    let Some(attributed) = (unsafe { attributed.as_ref() }) else {
        return String::new();
    };
    let string: *mut NSString = unsafe { msg_send![attributed, string] };
    let text = unsafe { string.as_ref() }
        .map(|s| s.to_string())
        .unwrap_or_default();
    // GPUI hands back an `alloc`/`init` (+1) object.
    unsafe { objc_release(attributed as *const AnyObject as *mut AnyObject) };
    text
}

fn utf16_len(text: &str) -> NSUInteger {
    text.encode_utf16().count() as NSUInteger
}

fn autoreleased(text: &str) -> *mut NSString {
    Retained::autorelease_return(NSString::from_str(text))
}

extern "C-unwind" fn is_accessibility_element(this: &AnyObject, _: Sel) -> Bool {
    Bool::new(unsafe { has_input(this) })
}

extern "C-unwind" fn accessibility_role(this: &AnyObject, _: Sel) -> *mut NSString {
    if !unsafe { has_input(this) } {
        return std::ptr::null_mut();
    }
    autoreleased("AXTextField")
}

extern "C-unwind" fn accessibility_value(this: &AnyObject, _: Sel) -> *mut NSString {
    if !unsafe { has_input(this) } {
        return std::ptr::null_mut();
    }
    autoreleased(&unsafe { input_text(this) })
}

extern "C-unwind" fn set_accessibility_value(this: &AnyObject, _: Sel, text: *mut NSString) {
    if text.is_null() || !unsafe { has_input(this) } {
        return;
    }
    let all = NSRange::new(0, utf16_len(&unsafe { input_text(this) }));
    unsafe {
        let _: () = msg_send![this, insertText: text, replacementRange: all];
    }
}

extern "C-unwind" fn accessibility_number_of_characters(this: &AnyObject, _: Sel) -> NSInteger {
    if !unsafe { has_input(this) } {
        return 0;
    }
    utf16_len(&unsafe { input_text(this) }) as NSInteger
}

extern "C-unwind" fn accessibility_visible_character_range(this: &AnyObject, _: Sel) -> NSRange {
    if !unsafe { has_input(this) } {
        return NSRange::new(0, 0);
    }
    NSRange::new(0, utf16_len(&unsafe { input_text(this) }))
}

extern "C-unwind" fn accessibility_selected_text(this: &AnyObject, _: Sel) -> *mut NSString {
    let range = unsafe { selected_range(this) };
    if range.location == NS_NOT_FOUND {
        return std::ptr::null_mut();
    }
    let text = unsafe { input_text(this) };
    let selected: Vec<u16> = text
        .encode_utf16()
        .skip(range.location)
        .take(range.length)
        .collect();
    autoreleased(&String::from_utf16_lossy(&selected))
}

extern "C-unwind" fn accessibility_selected_text_range(this: &AnyObject, _: Sel) -> NSRange {
    let range = unsafe { selected_range(this) };
    if range.location == NS_NOT_FOUND {
        NSRange::new(0, 0)
    } else {
        range
    }
}

extern "C-unwind" fn set_accessibility_selected_text(
    this: &AnyObject,
    _: Sel,
    text: *mut NSString,
) {
    if text.is_null() || !unsafe { has_input(this) } {
        return;
    }
    let replacement = NSRange::new(NS_NOT_FOUND, 0);
    unsafe {
        let _: () = msg_send![this, insertText: text, replacementRange: replacement];
    }
}

/// Screen rect (Cocoa coordinates) of `range`; dictation tools anchor their
/// overlay on the caret with it (`AXBoundsForRange`).
extern "C-unwind" fn accessibility_frame_for_range(
    this: &AnyObject,
    _: Sel,
    range: NSRange,
) -> NSRect {
    if !unsafe { has_input(this) } {
        return NSRect::ZERO;
    }
    unsafe {
        msg_send![
            this,
            firstRectForCharacterRange: range,
            actualRange: std::ptr::null_mut::<AnyObject>()
        ]
    }
}

unsafe fn add(cls: &AnyClass, sel: Sel, imp: Imp, types: &CStr) {
    let ok = unsafe {
        class_addMethod(
            cls as *const AnyClass as *mut AnyClass,
            sel,
            imp,
            types.as_ptr() as *const c_char,
        )
    };
    if !ok.as_bool() {
        tracing::warn!(?sel, "text_field_ax: GPUIView already implements selector");
    }
}

/// Idempotent enough: a second call only logs warnings.
pub fn install() {
    let Some(cls) = AnyClass::get(c"GPUIView") else {
        tracing::warn!("text_field_ax: GPUIView class not found");
        return;
    };
    type Getter<R> = extern "C-unwind" fn(&AnyObject, Sel) -> R;
    type Setter = extern "C-unwind" fn(&AnyObject, Sel, *mut NSString);
    type FrameForRange = extern "C-unwind" fn(&AnyObject, Sel, NSRange) -> NSRect;
    const RANGE: &CStr = c"{_NSRange=QQ}@:";
    unsafe {
        add(
            cls,
            sel!(isAccessibilityElement),
            transmute::<Getter<Bool>, Imp>(is_accessibility_element),
            c"B@:",
        );
        add(
            cls,
            sel!(isAccessibilityFocused),
            transmute::<Getter<Bool>, Imp>(is_accessibility_element),
            c"B@:",
        );
        add(
            cls,
            sel!(isAccessibilityEnabled),
            transmute::<Getter<Bool>, Imp>(is_accessibility_element),
            c"B@:",
        );
        add(
            cls,
            sel!(accessibilityRole),
            transmute::<Getter<*mut NSString>, Imp>(accessibility_role),
            c"@@:",
        );
        add(
            cls,
            sel!(accessibilityValue),
            transmute::<Getter<*mut NSString>, Imp>(accessibility_value),
            c"@@:",
        );
        add(
            cls,
            sel!(setAccessibilityValue:),
            transmute::<Setter, Imp>(set_accessibility_value),
            c"v@:@",
        );
        add(
            cls,
            sel!(accessibilityNumberOfCharacters),
            transmute::<Getter<NSInteger>, Imp>(accessibility_number_of_characters),
            c"q@:",
        );
        add(
            cls,
            sel!(accessibilityVisibleCharacterRange),
            transmute::<Getter<NSRange>, Imp>(accessibility_visible_character_range),
            RANGE,
        );
        add(
            cls,
            sel!(accessibilitySelectedText),
            transmute::<Getter<*mut NSString>, Imp>(accessibility_selected_text),
            c"@@:",
        );
        add(
            cls,
            sel!(setAccessibilitySelectedText:),
            transmute::<Setter, Imp>(set_accessibility_selected_text),
            c"v@:@",
        );
        add(
            cls,
            sel!(accessibilitySelectedTextRange),
            transmute::<Getter<NSRange>, Imp>(accessibility_selected_text_range),
            RANGE,
        );
        add(
            cls,
            sel!(accessibilityFrameForRange:),
            transmute::<FrameForRange, Imp>(accessibility_frame_for_range),
            c"{CGRect={CGPoint=dd}{CGSize=dd}}@:{_NSRange=QQ}",
        );
    }
}

extern "C" {
    fn NSAccessibilityPostNotification(element: *mut AnyObject, notification: *const NSString);
}

/// Tell AX observers that our key window and its input took focus. AppKit
/// never posts these for GPUI, so observer-driven tools would keep the
/// previously focused app's element.
pub fn notify_focus_changed() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let Some(window) = app.keyWindow() else {
        return;
    };
    let window_ptr = Retained::as_ptr(&window) as *mut AnyObject;
    unsafe {
        NSAccessibilityPostNotification(window_ptr, &*NSString::from_str("AXFocusedWindowChanged"));
        if let Some(responder) = window.firstResponder() {
            NSAccessibilityPostNotification(
                Retained::as_ptr(&responder) as *mut AnyObject,
                &*NSString::from_str("AXFocusedUIElementChanged"),
            );
        }
    }
}
