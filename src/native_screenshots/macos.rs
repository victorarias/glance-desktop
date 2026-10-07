#![allow(unexpected_cfgs)]
use super::Tracker;
use cocoa::{
    base::{id, nil},
    foundation::{NSAutoreleasePool, NSString},
};
use objc::{class, msg_send, sel, sel_impl};
use std::{ffi::c_void, marker::PhantomData, rc::Rc, time::Instant};

type Ref = *mut c_void;
type Callback = unsafe extern "C" fn(Ref, u32, Ref, Ref) -> Ref;
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        mask: u64,
        callback: Callback,
        data: Ref,
    ) -> Ref;
    fn CGEventGetIntegerValueField(event: Ref, field: u32) -> i64;
    fn CGEventGetFlags(event: Ref) -> u64;
    fn CGEventTapIsEnabled(tap: Ref) -> bool;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopCommonModes: Ref;
    fn CFMachPortCreateRunLoopSource(allocator: Ref, port: Ref, order: isize) -> Ref;
    fn CFMachPortInvalidate(port: Ref);
    fn CFRunLoopGetMain() -> Ref;
    fn CFRunLoopAddSource(run_loop: Ref, source: Ref, mode: Ref);
    fn CFRunLoopRemoveSource(run_loop: Ref, source: Ref, mode: Ref);
    fn CFRelease(value: Ref);
}

const PREFERENCE: &str = "GlanceNativeClipboardScreenshots";
const COPY_SOURCE: &str = "sh.glance.clipboard-source";
const PERMISSION: &str = "Allow Glance in System Settings → Privacy & Security → Input Monitoring, then restart Glance or turn native screenshots off and on.";

pub(crate) fn load_enabled() -> bool {
    unsafe {
        let key = NSString::alloc(nil).init_str(PREFERENCE);
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let value: bool = msg_send![defaults, boolForKey: key];
        let _: () = msg_send![key, release];
        value
    }
}
pub(crate) fn save_enabled(enabled: bool) {
    unsafe {
        let key = NSString::alloc(nil).init_str(PREFERENCE);
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let _: () = msg_send![defaults, setBool: enabled forKey: key];
        let _: () = msg_send![key, release];
    }
}
pub(crate) fn clipboard_generation() -> i64 {
    unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let board: id = msg_send![class!(NSPasteboard), generalPasteboard];
        let generation: i64 = if board.is_null() {
            -1
        } else {
            msg_send![board, changeCount]
        };
        pool.drain();
        generation
    }
}
/// Decode off the UI thread, and reject a clipboard owner changing mid-read.
pub(crate) fn snapshot(generation: i64) -> Result<Option<image::RgbaImage>, String> {
    super::consistent_snapshot(generation, clipboard_generation, || {
        if own_copy() {
            return Ok(None);
        }
        crate::platform::clipboard_image().map(Some)
    })
    .map(Option::flatten)
}
fn own_copy() -> bool {
    unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let board: id = msg_send![class!(NSPasteboard), generalPasteboard];
        let own = !board.is_null() && board_is_own_copy(board);
        pool.drain();
        own
    }
}
unsafe fn string(value: &str) -> id {
    unsafe {
        let value = NSString::alloc(nil).init_str(value);
        msg_send![value, autorelease]
    }
}
unsafe fn board_is_own_copy(board: id) -> bool {
    unsafe {
        let marker: id = msg_send![board, stringForType: string(COPY_SOURCE)];
        !marker.is_null()
    }
}
pub(crate) fn copy_image(image: image::RgbaImage) -> Result<(), String> {
    let mut png = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let board: id = msg_send![class!(NSPasteboard), generalPasteboard];
        let result = write_image(board, png.get_ref());
        pool.drain();
        result
    }
}
/// Publish image representations and source marker in one pasteboard item.
/// Recording changeCount after a write can instead stamp another app's copy.
unsafe fn write_image(board: id, png: &[u8]) -> Result<(), String> {
    unsafe {
        if board.is_null() {
            return Err("The macOS clipboard is unavailable.".into());
        }
        let data: id = msg_send![class!(NSData), dataWithBytes: png.as_ptr() length: png.len()];
        let native_image: id = msg_send![class!(NSImage), alloc];
        let native_image: id = msg_send![native_image, initWithData: data];
        if native_image.is_null() {
            return Err("Could not prepare the clipboard image.".into());
        }
        let native_image: id = msg_send![native_image, autorelease];
        let tiff: id = msg_send![native_image, TIFFRepresentation];
        if tiff.is_null() {
            return Err("Could not encode the clipboard image.".into());
        }
        let item: id = msg_send![class!(NSPasteboardItem), new];
        let item: id = msg_send![item, autorelease];
        let png_ok: bool = msg_send![item, setData: data forType: string("public.png")];
        let tiff_ok: bool = msg_send![item, setData: tiff forType: string("public.tiff")];
        let marker_ok: bool =
            msg_send![item, setString: string("Glance") forType: string(COPY_SOURCE)];
        if !png_ok || !tiff_ok || !marker_ok {
            return Err("Could not prepare clipboard representations.".into());
        }
        let objects: id = msg_send![class!(NSArray), arrayWithObject: item];
        let _: i64 = msg_send![board, clearContents];
        let written: bool = msg_send![board, writeObjects: objects];
        if written {
            Ok(())
        } else {
            Err("Could not write the clipboard image.".into())
        }
    }
}

struct State {
    tracker: Tracker,
    failed: bool,
}
pub(crate) struct Monitor {
    tap: Ref,
    source: Ref,
    state: Box<State>,
    // The tap, its callback state, and polling are all on AppKit's main thread.
    _main_thread: PhantomData<Rc<()>>,
}
impl Monitor {
    pub(crate) fn new(request_permission: bool) -> Result<Self, String> {
        unsafe {
            if !CGPreflightListenEventAccess() {
                if request_permission {
                    let _ = CGRequestListenEventAccess();
                }
                if !CGPreflightListenEventAccess() {
                    return Err(PERMISSION.into());
                }
            }
            let mut state = Box::new(State {
                tracker: Tracker::default(),
                failed: false,
            });
            // Session tap, head insertion, listen-only. Never alters or posts events.
            let tap = CGEventTapCreate(
                1,
                0,
                1,
                (1 << 1) | (1 << 2) | (1 << 10),
                callback,
                (&mut *state as *mut State).cast(),
            );
            if tap.is_null() {
                return Err(format!(
                    "Native screenshot observer unavailable. {PERMISSION}"
                ));
            }
            let source = CFMachPortCreateRunLoopSource(std::ptr::null_mut(), tap, 0);
            if source.is_null() {
                CFRelease(tap);
                return Err("Could not start the native screenshot observer.".into());
            }
            CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopCommonModes);
            Ok(Self {
                tap,
                source,
                state,
                _main_thread: PhantomData,
            })
        }
    }
    pub(crate) fn poll(&mut self) -> Result<Option<(u64, i64)>, String> {
        if self.state.failed || !unsafe { CGEventTapIsEnabled(self.tap) } {
            self.cancel();
            return Err(format!("Native screenshot observer stopped. {PERMISSION}"));
        }
        if self.state.tracker.request.is_none() {
            return Ok(None);
        }
        let generation = clipboard_generation();
        if generation < 0 {
            self.cancel();
            return Err(
                "The macOS clipboard is unavailable. Turn native screenshots off and on to retry."
                    .into(),
            );
        }
        Ok(self.state.tracker.poll(generation, Instant::now()))
    }
    pub(crate) fn cancel(&mut self) {
        self.state.tracker.cancel();
    }
    pub(crate) fn accept(&mut self, token: u64, generation: i64) -> bool {
        let accepted = self
            .state
            .tracker
            .accepts(token, generation, Instant::now());
        if accepted {
            self.cancel();
        }
        accepted
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        // Invalidate before releasing the Box so no callback can see freed state.
        unsafe {
            CFRunLoopRemoveSource(CFRunLoopGetMain(), self.source, kCFRunLoopCommonModes);
            CFMachPortInvalidate(self.tap);
            CFRelease(self.source);
            CFRelease(self.tap);
        }
    }
}
unsafe extern "C" fn callback(_: Ref, kind: u32, event: Ref, data: Ref) -> Ref {
    // Main-run-loop source; state stays at a stable address until tap invalidation.
    let state = unsafe { &mut *data.cast::<State>() };
    match kind {
        10 => {
            let key = unsafe { CGEventGetIntegerValueField(event, 9) };
            let flags = unsafe { CGEventGetFlags(event) };
            let repeated = unsafe { CGEventGetIntegerValueField(event, 8) } != 0;
            let chord = (1 << 17) | (1 << 18) | (1 << 20); // shift, control, command
            if key == 21 && flags & chord == chord && flags & (1 << 19) == 0 {
                if !repeated {
                    let baseline = clipboard_generation();
                    if baseline >= 0 {
                        state.tracker.shortcut(baseline);
                    } else {
                        state.tracker.cancel();
                        state.failed = true;
                    }
                }
            } else if key != 49 {
                // Space switches between region/window selection. Other keys
                // conservatively cancel attribution, including Escape and Copy.
                state.tracker.cancel();
            }
        }
        1 => state.tracker.mouse_down(),
        2 => state.tracker.mouse_up(Instant::now()),
        0xffff_fffe | 0xffff_ffff => {
            state.tracker.cancel();
            state.failed = true;
        }
        _ => {}
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventCreateKeyboardEvent(source: Ref, key: u16, down: bool) -> Ref;
        fn CGEventSetFlags(event: Ref, flags: u64);
        fn CGEventSetIntegerValueField(event: Ref, field: u32, value: i64);
    }
    #[test]
    fn native_callback_is_passive_and_cancels_escape_and_disabled_taps() {
        // Create events locally and call the callback; never post synthetic input.
        let mut state = State {
            tracker: Tracker::default(),
            failed: false,
        };
        let data = (&mut state as *mut State).cast();
        unsafe {
            let event = CGEventCreateKeyboardEvent(std::ptr::null_mut(), 21, true);
            assert!(!event.is_null());
            CGEventSetFlags(event, (1 << 17) | (1 << 18) | (1 << 20));
            assert_eq!(callback(std::ptr::null_mut(), 10, event, data), event);
            let baseline = state.tracker.request.as_ref().unwrap().baseline;
            CGEventSetIntegerValueField(event, 8, 1);
            callback(std::ptr::null_mut(), 10, event, data);
            assert_eq!(state.tracker.next_token, 1, "repeat does not rearm");
            CGEventSetIntegerValueField(event, 9, 49); // Space with chord modifiers held
            callback(std::ptr::null_mut(), 10, event, data);
            assert!(
                state.tracker.request.is_some(),
                "window selection stays armed"
            );
            callback(std::ptr::null_mut(), 1, event, data);
            callback(std::ptr::null_mut(), 2, event, data);
            assert_eq!(
                state.tracker.poll(baseline + 1, Instant::now()),
                Some((1, baseline + 1))
            );
            CGEventSetIntegerValueField(event, 9, 53); // Escape
            callback(std::ptr::null_mut(), 10, event, data);
            assert!(!state.tracker.accepts(1, baseline + 1, Instant::now()));
            assert_eq!(state.tracker.poll(baseline + 2, Instant::now()), None);
            state.tracker.shortcut(baseline + 2);
            callback(std::ptr::null_mut(), 0xffff_fffe, event, data);
            assert!(state.failed);
            assert!(state.tracker.request.is_none());
            CFRelease(event);
        }
    }
    #[test]
    fn missing_permission_is_reported_without_requesting_it() {
        if !unsafe { CGPreflightListenEventAccess() } {
            let error = Monitor::new(false)
                .err()
                .expect("permission must be required");
            assert!(error.contains("Input Monitoring"));
        }
    }
    #[test]
    fn own_clipboard_item_has_source_and_lossless_image_representations() {
        // A unique private pasteboard keeps the user's clipboard untouched.
        unsafe {
            let pool = NSAutoreleasePool::new(nil);
            let board: id = msg_send![class!(NSPasteboard), pasteboardWithUniqueName];
            assert!(!board.is_null());
            for (width, height) in [(8, 6), (2560, 1440)] {
                let started = Instant::now();
                let pixels = image::RgbaImage::from_fn(width, height, |x, y| {
                    image::Rgba([
                        (x as u8).wrapping_mul(20),
                        (y as u8).wrapping_mul(30),
                        100,
                        [0, 64, 128, 255][x as usize % 4],
                    ])
                });
                let mut png = std::io::Cursor::new(Vec::new());
                pixels.write_to(&mut png, image::ImageFormat::Png).unwrap();
                write_image(board, png.get_ref()).unwrap();
                assert!(board_is_own_copy(board));
                let tiff: id = msg_send![board, dataForType: string("public.tiff")];
                assert!(!tiff.is_null());
                let length: usize = msg_send![tiff, length];
                let bytes: *const u8 = msg_send![tiff, bytes];
                let decoded = image::load_from_memory(std::slice::from_raw_parts(bytes, length))
                    .unwrap()
                    .to_rgba8();
                assert_eq!(decoded, pixels);
                let png_data: id = msg_send![board, dataForType: string("public.png")];
                let png_length: usize = msg_send![png_data, length];
                let png_bytes: *const u8 = msg_send![png_data, bytes];
                assert_eq!(
                    image::load_from_memory(std::slice::from_raw_parts(png_bytes, png_length))
                        .unwrap()
                        .to_rgba8(),
                    pixels
                );
                eprintln!(
                    "Private pasteboard {width}x{height}: PNG/TIFF RGBA fidelity verified in {:?}",
                    started.elapsed()
                );
                let generation: i64 = msg_send![board, changeCount];
                let _: i64 = msg_send![board, clearContents];
                assert!(
                    !board_is_own_copy(board),
                    "a later owner never inherits the marker"
                );
                let later: i64 = msg_send![board, changeCount];
                assert_ne!(generation, later);
            }
            let _: () = msg_send![board, releaseGlobally];
            pool.drain();
        }
    }
}
