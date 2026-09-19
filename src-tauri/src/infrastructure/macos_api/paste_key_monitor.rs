//! Passive global listener for physical ⌘V — plays paste sound without intercepting the shortcut.

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::permissions::{
    has_accessibility_permission, V_KEY_CODE,
};

type CFMachPortRef = *mut std::ffi::c_void;
type CFRunLoopRef = *mut std::ffi::c_void;
type CFRunLoopSourceRef = *mut std::ffi::c_void;
type CGEventRef = *mut std::ffi::c_void;
type CGEventTapProxy = *mut std::ffi::c_void;
type CGEventType = u32;
type CGEventMask = u64;
type CGEventFlags = u64;
type CGEventTapLocation = u32;
type CGEventTapPlacement = u32;
type CGEventTapOptions = u32;
type CGEventField = u32;

const K_CG_EVENT_KEY_DOWN: CGEventType = 10;
const K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT: CGEventType = 0xFFFF_FFFE;
const K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT: CGEventType = 0xFFFF_FFFF;
const K_CG_EVENT_TAP_OPTION_LISTEN_ONLY: CGEventTapOptions = 1;
const K_CG_HEAD_INSERT_EVENT_TAP: CGEventTapPlacement = 0;
const K_CG_HID_EVENT_TAP: CGEventTapLocation = 0;
const K_CG_EVENT_FLAG_MASK_COMMAND: CGEventFlags = 1 << 20;
const K_CG_KEYBOARD_EVENT_KEYCODE: CGEventField = 9;
const K_CG_KEYBOARD_EVENT_AUTOREPEAT: CGEventField = 11;

static SUPPRESS_CMDV_SOUND_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
static MONITOR_STARTED: OnceLock<()> = OnceLock::new();
static ACTIVE_EVENT_TAP: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Skip tap feedback briefly after TieZ posts a synthetic ⌘V (schedule_paste_sound handles that).
pub fn suppress_paste_sound_briefly() {
    SUPPRESS_CMDV_SOUND_UNTIL_MS.store(now_ms().saturating_add(250), Ordering::Release);
}

fn should_play_for_physical_cmd_v() -> bool {
    now_ms() >= SUPPRESS_CMDV_SOUND_UNTIL_MS.load(Ordering::Acquire)
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: CGEventTapLocation,
        place: CGEventTapPlacement,
        options: CGEventTapOptions,
        events_of_interest: CGEventMask,
        callback: Option<
            unsafe extern "C" fn(
                CGEventTapProxy,
                CGEventType,
                CGEventRef,
                *mut std::ffi::c_void,
            ) -> CGEventRef,
        >,
        user_info: *mut std::ffi::c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetFlags(event: CGEventRef) -> CGEventFlags;
    fn CGEventGetIntegerValueField(event: CGEventRef, field: CGEventField) -> i64;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: *const std::ffi::c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(
        rl: CFRunLoopRef,
        source: CFRunLoopSourceRef,
        mode: *const std::ffi::c_void,
    );
    fn CFRunLoopRemoveSource(
        rl: CFRunLoopRef,
        source: CFRunLoopSourceRef,
        mode: *const std::ffi::c_void,
    );
    fn CFRunLoopRun();
    fn CFRunLoopStop(rl: CFRunLoopRef);
    fn CFMachPortInvalidate(port: CFMachPortRef);
    fn CFRelease(cf: *const std::ffi::c_void);
    static kCFRunLoopCommonModes: *const std::ffi::c_void;
}

unsafe extern "C" fn cmd_v_callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: CGEventRef,
    _user_info: *mut std::ffi::c_void,
) -> CGEventRef {
    if event_type == K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT
        || event_type == K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT
    {
        let tap = ACTIVE_EVENT_TAP.load(Ordering::Acquire);
        if tap.is_null() {
            return event;
        }

        // Revoking Accessibility disables the tap. Re-arming it then leaves TieZ
        // in the HID event path without permission, and the window server pays
        // for a denied TCC check on every event it delivers, which stalls
        // keyboard and mouse input system-wide. Let the monitor thread fall back
        // to polling instead; it rebuilds the tap once the grant returns.
        if !has_accessibility_permission() {
            CGEventTapEnable(tap as CFMachPortRef, false);
            CFRunLoopStop(CFRunLoopGetCurrent());
            return event;
        }

        CGEventTapEnable(tap as CFMachPortRef, true);
        return event;
    }

    if event_type == K_CG_EVENT_KEY_DOWN && !event.is_null() {
        let flags = CGEventGetFlags(event);
        let keycode = CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE) as u16;
        let autorepeat = CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_AUTOREPEAT);

        if autorepeat == 0
            && keycode == V_KEY_CODE
            && (flags & K_CG_EVENT_FLAG_MASK_COMMAND) != 0
            && should_play_for_physical_cmd_v()
        {
            if let Some(app) = crate::global_state::GLOBAL_APP_HANDLE.get() {
                crate::services::ui_sound::schedule_paste_sound(app);
            }
        }

    }
    event
}

/// Installs the ⌘V tap and services it until the Accessibility grant goes away.
unsafe fn try_start_event_tap() {
    if !has_accessibility_permission() {
        return;
    }

    let mask: CGEventMask = 1u64 << K_CG_EVENT_KEY_DOWN;
    let tap = CGEventTapCreate(
        K_CG_HID_EVENT_TAP,
        K_CG_HEAD_INSERT_EVENT_TAP,
        K_CG_EVENT_TAP_OPTION_LISTEN_ONLY,
        mask,
        Some(cmd_v_callback),
        std::ptr::null_mut(),
    );
    if tap.is_null() {
        eprintln!("[paste-key-monitor] CGEventTapCreate returned null");
        return;
    }

    ACTIVE_EVENT_TAP.store(tap as *mut std::ffi::c_void, Ordering::Release);

    let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
    if source.is_null() {
        eprintln!("[paste-key-monitor] Failed to create run loop source");
        ACTIVE_EVENT_TAP.store(std::ptr::null_mut(), Ordering::Release);
        CFMachPortInvalidate(tap);
        CFRelease(tap);
        return;
    }

    let run_loop = CFRunLoopGetCurrent();
    CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);

    CGEventTapEnable(tap, true);
    eprintln!("[paste-key-monitor] ⌘V listener active");
    CFRunLoopRun();

    // Only reached once the callback stopped the loop, so drop the tap and let
    // the caller poll for the permission to come back.
    eprintln!("[paste-key-monitor] Accessibility revoked, ⌘V listener stopped");
    ACTIVE_EVENT_TAP.store(std::ptr::null_mut(), Ordering::Release);
    CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
    CFRelease(source);
    CFMachPortInvalidate(tap);
    CFRelease(tap);
}

pub fn start_paste_key_monitor() {
    if MONITOR_STARTED.set(()).is_err() {
        return;
    }

    std::thread::Builder::new()
        .name("tiez-paste-key-monitor".into())
        .spawn(|| {
            loop {
                // Returns as soon as the Accessibility grant goes away, so keep
                // polling and rebuild the tap when it is granted again.
                unsafe { try_start_event_tap() };
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .ok();
}
