use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::os::raw::c_int;

use cocoa::appkit::{NSBackingStoreType, NSMainMenuWindowLevel};
use cocoa::base::{id, nil, NO, YES};
use cocoa::foundation::{NSAutoreleasePool, NSPoint, NSRect, NSSize};
use core_graphics::display::CGDisplay;
use objc::{class, msg_send, sel, sel_impl};

use crate::runtime::{ComputerUseError, ErrorCode, Rect};

/// A click-through, non-activating AppKit panel used only as a visual boundary.
/// Every AppKit operation is synchronously marshalled onto the application's
/// main queue; tool requests are served from Tokio worker threads.
pub(super) struct Overlay {
    window: usize,
    stopped: bool,
}

impl Overlay {
    pub fn new(frame: Rect) -> Result<Self, ComputerUseError> {
        if !frame.is_valid() {
            return Err(ComputerUseError::invalid_coordinates());
        }
        let window = on_main(move || create_panel(frame))?;
        Ok(Self {
            window,
            stopped: false,
        })
    }

    pub fn follow(&mut self, frame: Rect) {
        if self.stopped || !frame.is_valid() {
            return;
        }
        let window = self.window;
        on_main_async(move || unsafe {
            let panel = window as id;
            let appkit_frame = appkit_frame(frame);
            let _: () = msg_send![panel, setFrame: appkit_frame display: YES];
        });
    }

    pub fn normal(&mut self) {
        self.set_alpha(0.78);
    }

    pub fn pulse(&mut self) {
        self.set_alpha(1.0);
        std::thread::sleep(std::time::Duration::from_millis(55));
        self.normal();
    }

    pub fn dim(&mut self) {
        self.set_alpha(0.22);
    }

    fn set_alpha(&mut self, alpha: f64) {
        if self.stopped {
            return;
        }
        let window = self.window;
        on_main_async(move || unsafe {
            let panel = window as id;
            let _: () = msg_send![panel, setAlphaValue: alpha];
            let _: () = msg_send![panel, orderFrontRegardless];
        });
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        let window = self.window;
        on_main_async(move || unsafe {
            let panel = window as id;
            let _: () = msg_send![panel, orderOut: nil];
            let _: () = msg_send![panel, close];
            let _: () = msg_send![panel, release];
        });
        self.stopped = true;
        self.window = 0;
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.stop();
    }
}

fn create_panel(frame: Rect) -> Result<usize, ComputerUseError> {
    unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let _: id = msg_send![class!(NSApplication), sharedApplication];
        let appkit_frame = appkit_frame(frame);
        let panel: id = msg_send![class!(NSPanel), alloc];
        // NSNonactivatingPanelMask is bit 7. cocoa 0.26 does not expose it.
        let style_mask: u64 = 1 << 7;
        let panel: id = msg_send![panel,
            initWithContentRect:appkit_frame
            styleMask:style_mask
            backing:NSBackingStoreType::NSBackingStoreBuffered
            defer:NO
        ];
        if panel == nil {
            let _: () = msg_send![pool, drain];
            return Err(ComputerUseError::new(
                ErrorCode::Internal,
                "failed to create Computer Use overlay",
                true,
            ));
        }

        let clear: id = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![panel, setBackgroundColor: clear];
        let _: () = msg_send![panel, setOpaque: NO];
        let _: () = msg_send![panel, setHasShadow: NO];
        let _: () = msg_send![panel, setIgnoresMouseEvents: YES];
        let _: () = msg_send![panel, setCanHide: NO];
        let _: () = msg_send![panel, setHidesOnDeactivate: NO];
        let _: () = msg_send![panel, setReleasedWhenClosed: NO];
        let _: () = msg_send![panel, setBecomesKeyOnlyIfNeeded: YES];
        let level = NSMainMenuWindowLevel as i64 + 1;
        let _: () = msg_send![panel, setLevel: level];
        // Stay with the current Space, remain outside Cmd-Tab/window cycling,
        // and remain visible beside a target full-screen window.
        let behavior: u64 = (1 << 3) | (1 << 6) | (1 << 8);
        let _: () = msg_send![panel, setCollectionBehavior: behavior];

        let view: id = msg_send![panel, contentView];
        let _: () = msg_send![view, setWantsLayer: YES];
        let layer: id = msg_send![view, layer];
        let color: id = msg_send![class!(NSColor), colorWithSRGBRed: 0.13f64 green: 0.78f64 blue: 1.0f64 alpha: 0.95f64];
        let cg_color: id = msg_send![color, CGColor];
        let _: () = msg_send![layer, setBorderColor: cg_color];
        let _: () = msg_send![layer, setBorderWidth: 4.0f64];
        let _: () = msg_send![layer, setCornerRadius: 8.0f64];
        let _: () = msg_send![layer, setShadowColor: cg_color];
        let _: () = msg_send![layer, setShadowOpacity: 0.9f32];
        let _: () = msg_send![layer, setShadowRadius: 10.0f64];
        let shadow_offset = NSSize::new(0.0, 0.0);
        let _: () = msg_send![layer, setShadowOffset: shadow_offset];
        let _: () = msg_send![panel, setAlphaValue: 0.78f64];
        let _: () = msg_send![panel, orderFrontRegardless];
        let _: () = msg_send![pool, drain];
        Ok(panel as usize)
    }
}

fn appkit_frame(frame: Rect) -> NSRect {
    const PADDING: f64 = 7.0;
    let main_height = CGDisplay::main().bounds().size.height;
    NSRect::new(
        NSPoint::new(
            frame.x - PADDING,
            main_height - frame.y - frame.height - PADDING,
        ),
        NSSize::new(frame.width + PADDING * 2.0, frame.height + PADDING * 2.0),
    )
}

fn on_main<R, F>(operation: F) -> R
where
    R: Send,
    F: FnOnce() -> R + Send,
{
    if unsafe { pthread_main_np() } != 0 {
        return operation();
    }

    struct Context<F, R> {
        operation: Option<F>,
        result: MaybeUninit<R>,
    }

    unsafe extern "C" fn run<F, R>(context: *mut c_void)
    where
        F: FnOnce() -> R,
    {
        let context = &mut *(context as *mut Context<F, R>);
        let operation = context
            .operation
            .take()
            .expect("main queue operation missing");
        context.result.write(operation());
    }

    let mut context = Context {
        operation: Some(operation),
        result: MaybeUninit::uninit(),
    };
    unsafe {
        dispatch_sync_f(
            std::ptr::addr_of!(_dispatch_main_q).cast_mut().cast(),
            (&mut context as *mut Context<F, R>).cast(),
            run::<F, R>,
        );
        context.result.assume_init()
    }
}

fn on_main_async<F>(operation: F)
where
    F: FnOnce() + Send + 'static,
{
    if unsafe { pthread_main_np() } != 0 {
        operation();
        return;
    }

    unsafe extern "C" fn run<F>(context: *mut c_void)
    where
        F: FnOnce(),
    {
        let operation = unsafe { Box::from_raw(context.cast::<F>()) };
        operation();
    }

    let operation = Box::new(operation);
    unsafe {
        dispatch_async_f(
            std::ptr::addr_of!(_dispatch_main_q).cast_mut().cast(),
            Box::into_raw(operation).cast(),
            run::<F>,
        );
    }
}

#[link(name = "QuartzCore", kind = "framework")]
extern "C" {}

extern "C" {
    #[link_name = "_dispatch_main_q"]
    static _dispatch_main_q: u8;
    fn dispatch_sync_f(
        queue: *mut c_void,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
    fn dispatch_async_f(
        queue: *mut c_void,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
    fn pthread_main_np() -> c_int;
}
