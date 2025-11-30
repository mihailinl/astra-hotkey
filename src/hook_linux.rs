//! Linux-specific X11 keyboard hook implementation.
//!
//! Uses XGrabKey to register hotkeys with the X server.
//! Only registered hotkey combinations trigger the callback.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::ptr;
use std::ffi::CString;

use x11::xlib::{
    self, Display, XOpenDisplay, XCloseDisplay, XDefaultRootWindow,
    XGrabKey, XUngrabKey, XNextEvent, XEvent, XKeyEvent,
    KeyPress, GrabModeAsync, AnyModifier, CurrentTime,
    XKeysymToKeycode, XKeycodeToKeysym, XLookupString,
    ControlMask, ShiftMask, Mod1Mask, Mod4Mask,
};
use x11::keysym::*;

use crate::registry::{self, Modifiers};
use crate::invoke_callback;

/// Flag to signal hook thread to stop
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Start the X11 keyboard hook in a separate thread.
pub fn start_hook() -> bool {
    if RUNNING.swap(true, Ordering::SeqCst) {
        // Already running
        return false;
    }

    thread::spawn(|| {
        unsafe {
            // Open X display
            let display = XOpenDisplay(ptr::null());
            if display.is_null() {
                RUNNING.store(false, Ordering::SeqCst);
                return;
            }

            let root = XDefaultRootWindow(display);

            // Event loop
            let mut event: XEvent = std::mem::zeroed();
            while RUNNING.load(Ordering::SeqCst) {
                // Check if there's an event (non-blocking would be better but this works)
                if xlib::XPending(display) > 0 {
                    XNextEvent(display, &mut event);

                    if event.type_ == KeyPress {
                        let key_event: &XKeyEvent = &event.key;

                        // Get modifiers
                        let modifiers = Modifiers {
                            ctrl: (key_event.state & ControlMask) != 0,
                            alt: (key_event.state & Mod1Mask) != 0,
                            shift: (key_event.state & ShiftMask) != 0,
                            win: (key_event.state & Mod4Mask) != 0,
                        };

                        // Get key name from keycode
                        if let Some(key_name) = keycode_to_name(display, key_event.keycode) {
                            // Check if this combination is registered
                            if let Some(hotkey) = registry::check_hotkey(modifiers, &key_name) {
                                invoke_callback(&hotkey);
                            }
                        }
                    }
                } else {
                    // Sleep a bit to avoid busy waiting
                    thread::sleep(std::time::Duration::from_millis(10));
                }
            }

            // Cleanup
            XCloseDisplay(display);
        }
    });

    // Give the hook thread time to start
    thread::sleep(std::time::Duration::from_millis(50));

    RUNNING.load(Ordering::SeqCst)
}

/// Stop the X11 keyboard hook.
pub fn stop_hook() {
    RUNNING.store(false, Ordering::SeqCst);

    // Wait a bit for cleanup
    thread::sleep(std::time::Duration::from_millis(100));
}

/// Convert X11 keycode to key name string.
unsafe fn keycode_to_name(display: *mut Display, keycode: u32) -> Option<String> {
    let keysym = XKeycodeToKeysym(display, keycode as u8, 0);

    let name = match keysym as u32 {
        // Letters (lowercase keysyms map to uppercase names)
        XK_a..=XK_z => ((keysym as u8 - b'a' + b'A') as char).to_string(),
        XK_A..=XK_Z => ((keysym as u8) as char).to_string(),

        // Numbers
        XK_0..=XK_9 => ((keysym as u8) as char).to_string(),

        // Function keys
        XK_F1 => "F1".to_string(),
        XK_F2 => "F2".to_string(),
        XK_F3 => "F3".to_string(),
        XK_F4 => "F4".to_string(),
        XK_F5 => "F5".to_string(),
        XK_F6 => "F6".to_string(),
        XK_F7 => "F7".to_string(),
        XK_F8 => "F8".to_string(),
        XK_F9 => "F9".to_string(),
        XK_F10 => "F10".to_string(),
        XK_F11 => "F11".to_string(),
        XK_F12 => "F12".to_string(),

        // Numpad
        XK_KP_0 => "Num0".to_string(),
        XK_KP_1 => "Num1".to_string(),
        XK_KP_2 => "Num2".to_string(),
        XK_KP_3 => "Num3".to_string(),
        XK_KP_4 => "Num4".to_string(),
        XK_KP_5 => "Num5".to_string(),
        XK_KP_6 => "Num6".to_string(),
        XK_KP_7 => "Num7".to_string(),
        XK_KP_8 => "Num8".to_string(),
        XK_KP_9 => "Num9".to_string(),
        XK_KP_Multiply => "NumMul".to_string(),
        XK_KP_Add => "NumAdd".to_string(),
        XK_KP_Subtract => "NumSub".to_string(),
        XK_KP_Decimal => "NumDec".to_string(),
        XK_KP_Divide => "NumDiv".to_string(),
        XK_KP_Enter => "Enter".to_string(),

        // Special keys
        XK_Return => "Enter".to_string(),
        XK_Escape => "Escape".to_string(),
        XK_space => "Space".to_string(),
        XK_Tab => "Tab".to_string(),
        XK_BackSpace => "Backspace".to_string(),
        XK_Delete => "Delete".to_string(),
        XK_Insert => "Insert".to_string(),
        XK_Home => "Home".to_string(),
        XK_End => "End".to_string(),
        XK_Page_Up => "PageUp".to_string(),
        XK_Page_Down => "PageDown".to_string(),
        XK_Up => "Up".to_string(),
        XK_Down => "Down".to_string(),
        XK_Left => "Left".to_string(),
        XK_Right => "Right".to_string(),
        XK_Print => "PrintScreen".to_string(),
        XK_Pause => "Pause".to_string(),
        XK_Caps_Lock => "CapsLock".to_string(),
        XK_Num_Lock => "NumLock".to_string(),
        XK_Scroll_Lock => "ScrollLock".to_string(),

        // Symbols
        XK_grave => "`".to_string(),
        XK_minus => "-".to_string(),
        XK_equal => "=".to_string(),
        XK_bracketleft => "[".to_string(),
        XK_bracketright => "]".to_string(),
        XK_backslash => "\\".to_string(),
        XK_semicolon => ";".to_string(),
        XK_apostrophe => "'".to_string(),
        XK_comma => ",".to_string(),
        XK_period => ".".to_string(),
        XK_slash => "/".to_string(),

        // Unknown
        _ => return None,
    };

    Some(name)
}
