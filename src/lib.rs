#![allow(unsafe_op_in_unsafe_fn)]

pub mod portal;
pub mod image_proc;
pub mod pw;

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;
use parking_lot::Mutex;
use libc::{c_int, c_uint, c_ulong, c_char};

struct HookState {
    capture: Option<pw::PipewireCapture>,
    portal_session: Option<portal::PortalSession>,
    attempted: bool,
}


static STATE: LazyLock<Mutex<HookState>> = LazyLock::new(|| {
    Mutex::new(HookState {
        capture: None,
        portal_session: None,
        attempted: false,
    })
});


fn get_state() -> &'static Mutex<HookState> {
    &STATE
}

static G_CAPTURE_XIMAGE: AtomicPtr<x11::xlib::XImage> = AtomicPtr::new(std::ptr::null_mut());
static G_INJECTOR_RUNNING: AtomicBool = AtomicBool::new(false);
static G_XSHM_ATTACH_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
struct XImageTarget {
    width: u32,
    height: u32,
    bytes_per_line: usize,
    format: image_proc::PixelFormat,
    data: *mut u8,
}

unsafe fn inspect_ximage(image: *mut x11::xlib::XImage) -> Result<XImageTarget, &'static str> {
    if image.is_null() {
        return Err("XImage is null");
    }

    let image = &*image;
    if image.data.is_null() {
        return Err("XImage data is null");
    }
    if image.format != x11::xlib::ZPixmap {
        return Err("Only ZPixmap XImages are supported");
    }
    if image.width <= 0 || image.height <= 0 || image.bits_per_pixel != 32 {
        return Err("Only non-empty 32-bit XImages are supported");
    }
    if image.byte_order != x11::xlib::LSBFirst {
        return Err("Only LSB-first XImages are supported");
    }

    let format = if image.red_mask == 0xff0000
        && image.green_mask == 0xff00
        && image.blue_mask == 0xff
    {
        // A 0x00RRGGBB pixel is stored as B, G, R, X on little-endian hosts.
        image_proc::PixelFormat::Bgrx
    } else if image.red_mask == 0xff && image.green_mask == 0xff00 && image.blue_mask == 0xff0000 {
        image_proc::PixelFormat::Rgbx
    } else {
        return Err("Unsupported XImage RGB masks");
    };

    let width = image.width as u32;
    let height = image.height as u32;
    let row_bytes = (width as usize)
        .checked_mul(4)
        .ok_or("Overflow calculating XImage row size")?;
    let bytes_per_line =
        usize::try_from(image.bytes_per_line).map_err(|_| "XImage has a negative row stride")?;
    if bytes_per_line < row_bytes {
        return Err("XImage row stride is smaller than its width");
    }
    bytes_per_line
        .checked_mul(height as usize)
        .ok_or("Overflow calculating XImage buffer size")?;

    Ok(XImageTarget {
        width,
        height,
        bytes_per_line,
        format,
        data: image.data as *mut u8,
    })
}

unsafe fn copy_packed_to_ximage(src: &[u8], target: XImageTarget) {
    let row_bytes = target.width as usize * 4;
    if target.bytes_per_line == row_bytes {
        std::ptr::copy_nonoverlapping(src.as_ptr(), target.data, src.len());
        return;
    }

    for row in 0..target.height as usize {
        std::ptr::copy_nonoverlapping(
            src.as_ptr().add(row * row_bytes),
            target.data.add(row * target.bytes_per_line),
            row_bytes,
        );
    }
}

unsafe fn inject_frame_once(
    frame: &pw::FrameData,
    image: *mut x11::xlib::XImage,
) -> Result<(), &'static str> {
    let target = inspect_ximage(image)?;
    let buffer_len = (target.width as usize)
        .checked_mul(target.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("Overflow calculating staging buffer size")?;
    let mut staging_buffer = vec![0u8; buffer_len];

    image_proc::resize_and_convert(
        &frame.data,
        frame.width,
        frame.height,
        frame.format,
        &mut staging_buffer,
        target.width,
        target.height,
        target.format,
    )?;
    copy_packed_to_ximage(&staging_buffer, target);
    Ok(())
}

unsafe fn is_caller_from_meeting_sdk() -> bool {
    let mut callstack: [*mut libc::c_void; 4] = [std::ptr::null_mut(); 4];
    let frames = libc::backtrace(callstack.as_mut_ptr(), 4);
    
    for i in 2..frames as usize {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(callstack[i], &mut info) != 0 && !info.dli_fname.is_null() {
            let fname = std::ffi::CStr::from_ptr(info.dli_fname).to_string_lossy();
            if fname.contains("libmeeting_sdk.so") {
                eprintln!("[screenshare-hook] Caller validated: {}", fname);
                return true;
            }
        }
    }
    
    if frames >= 3 {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(callstack[2], &mut info) != 0 && !info.dli_fname.is_null() {
            let fname = std::ffi::CStr::from_ptr(info.dli_fname).to_string_lossy();
            eprintln!("[screenshare-hook] XShmAttach from non-meeting caller: {}", fname);
        }
    }
    
    false
}
unsafe fn is_caller_validated() -> bool {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(name) = exe.file_name() {
            if name == "tblive" {
                eprintln!("[screenshare-hook] Caller validated: running inside tblive process");
                return true;
            }
        }
    }
    is_caller_from_meeting_sdk()
}


fn ensure_capture_started(state: &mut HookState) {
    if state.capture.is_some() || state.attempted {
        return;
    }
    state.attempted = true;
    eprintln!("[screenshare-hook] Initiating wayland screenshare capture...");

    match portal::start_screencast() {
        Ok(mut portal_sess) => {
            if portal_sess.streams.is_empty() {
                eprintln!("[screenshare-hook] Portal returned no stream node IDs");
                state.attempted = false;
                return;
            }
            let stream_info = &portal_sess.streams[0];
            let node_id = stream_info.node_id;
            let target_object = stream_info.pipewire_serial.map(|s| s.to_string());
            
            let owned_fd = match portal_sess.take_fd() {
                Some(fd) => fd,
                None => {
                    eprintln!("[screenshare-hook] PipeWire FD already consumed");
                    state.attempted = false;
                    return;
                }
            };

            match pw::start_capture(owned_fd, node_id, target_object) {
                Ok(cap) => {
                    state.capture = Some(cap);
                    state.portal_session = Some(portal_sess); // Keep the portal session alive!
                    eprintln!("[screenshare-hook] Screen capture started successfully via PipeWire node {}", node_id);
                }
                Err(e) => {
                    eprintln!("[screenshare-hook] Failed to start PipeWire capture: {}", e);
                    state.attempted = false;
                }
            }
        }
        Err(e) => {
            eprintln!("[screenshare-hook] XDG Portal handshake failed: {}", e);
            state.attempted = false;
        }
    }
}


fn start_portal_session() {
    let mut guard = get_state().lock();
    ensure_capture_started(&mut guard);
    if let Some(ref cap) = guard.capture {
        let frame_handle = cap.latest_frame_handle();
        G_INJECTOR_RUNNING.store(true, Ordering::Release);
        thread::spawn(move || {
            injector_loop(frame_handle);
        });
    }
}



fn injector_loop(latest_frame: std::sync::Arc<Mutex<Option<pw::FrameData>>>) {

    eprintln!("[screenshare-hook] injector thread started");
    let mut inject_count: u64 = 0;
    
    let mut ximage: *mut x11::xlib::XImage = std::ptr::null_mut();
    while G_INJECTOR_RUNNING.load(Ordering::Acquire) {
        ximage = G_CAPTURE_XIMAGE.load(Ordering::Acquire);
        if !ximage.is_null() {
            unsafe {
                let img = &*ximage;
                if !img.data.is_null() && img.width >= 640 && img.height >= 480 && img.width < 16384 && img.height < 16384 {
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    
    if !G_INJECTOR_RUNNING.load(Ordering::Acquire) {
        return;
    }
    
    let target = unsafe {
        match inspect_ximage(ximage) {
            Ok(target) => target,
            Err(e) => {
                eprintln!("[screenshare-hook] cannot inject into XImage: {}", e);
                return;
            }
        }
    };

    let buf_size = target.width as usize * target.height as usize * 4;
    let mut staging_buffer = vec![0u8; buf_size];
    
    eprintln!(
        "[screenshare-hook] injector locked XImage: {}x{} stride={} format={:?} data={:?}",
        target.width, target.height, target.bytes_per_line, target.format, target.data
    );
    
    while G_INJECTOR_RUNNING.load(Ordering::Acquire) {
        let mut frame_copied = false;
        {
            let guard = latest_frame.lock();
            if let Some(frame) = &*guard {
                if let Err(e) = image_proc::resize_and_convert(
                    &frame.data,
                    frame.width,
                    frame.height,
                    frame.format,
                    &mut staging_buffer,
                    target.width,
                    target.height,
                    target.format,
                ) {
                    eprintln!("[screenshare-hook] resize_and_convert failed in injector: {}", e);
                } else {
                    frame_copied = true;
                }
            }
        }

        
        if frame_copied && G_INJECTOR_RUNNING.load(Ordering::Acquire) {
            unsafe {
                copy_packed_to_ximage(&staging_buffer, target);
            }
            inject_count += 1;
            if inject_count == 1 || inject_count % 200 == 0 {
                eprintln!("[screenshare-hook] injector frame #{}", inject_count);
            }
        }
        
        thread::sleep(Duration::from_millis(33));
    }
    
    eprintln!("[screenshare-hook] injector thread stopped");
    
    let mut guard = get_state().lock();
    if let Some(mut cap) = guard.capture.take() {
        cap.stop();
    }
    guard.portal_session = None; // Terminate portal session and Close it
    guard.attempted = false;
    G_CAPTURE_XIMAGE.store(std::ptr::null_mut(), Ordering::Release);

}

type XShmAttachType = unsafe extern "C" fn(
    *mut x11::xlib::Display,
    *mut x11::xshm::XShmSegmentInfo,
) -> c_int;

type XShmDetachType = unsafe extern "C" fn(
    *mut x11::xlib::Display,
    *mut x11::xshm::XShmSegmentInfo,
) -> c_int;

type XShmGetImageType = unsafe extern "C" fn(
    *mut x11::xlib::Display,
    x11::xlib::Drawable,
    *mut x11::xlib::XImage,
    c_int,
    c_int,
    c_ulong,
) -> c_int;

type XShmCreateImageType = unsafe extern "C" fn(
    *mut x11::xlib::Display,
    *mut x11::xlib::Visual,
    c_uint,
    c_int,
    *mut c_char,
    *mut x11::xshm::XShmSegmentInfo,
    c_uint,
    c_uint,
) -> *mut x11::xlib::XImage;

struct DlHandle(*mut libc::c_void);
unsafe impl Send for DlHandle {}
unsafe impl Sync for DlHandle {}

static XEXT_HANDLE: LazyLock<DlHandle> = LazyLock::new(|| {
    unsafe {
        let mut handle = libc::dlopen(b"libXext.so.6\0".as_ptr() as *const c_char, libc::RTLD_LAZY);
        if handle.is_null() {
            handle = libc::dlopen(b"libXext.so\0".as_ptr() as *const c_char, libc::RTLD_LAZY);
        }
        if handle.is_null() {
            eprintln!("[screenshare-hook] Failed to dlopen libXext");
        }
        DlHandle(handle)
    }
});

static X11_HANDLE: LazyLock<DlHandle> = LazyLock::new(|| {
    unsafe {
        let mut handle = libc::dlopen(b"libX11.so.6\0".as_ptr() as *const c_char, libc::RTLD_LAZY);
        if handle.is_null() {
            handle = libc::dlopen(b"libX11.so\0".as_ptr() as *const c_char, libc::RTLD_LAZY);
        }
        if handle.is_null() {
            eprintln!("[screenshare-hook] Failed to dlopen libX11");
        }
        DlHandle(handle)
    }
});

static ORIGINAL_XSHMATTACH: LazyLock<Option<XShmAttachType>> = LazyLock::new(|| {
    unsafe {
        let handle = XEXT_HANDLE.0;
        if handle.is_null() { return None; }

        let symbol = libc::dlsym(handle, b"XShmAttach\0".as_ptr() as *const c_char);
        if symbol.is_null() { None } else { Some(std::mem::transmute(symbol)) }
    }
});

static ORIGINAL_XSHMDETACH: LazyLock<Option<XShmDetachType>> = LazyLock::new(|| {
    unsafe {
        let handle = XEXT_HANDLE.0;
        if handle.is_null() { return None; }

        let symbol = libc::dlsym(handle, b"XShmDetach\0".as_ptr() as *const c_char);
        if symbol.is_null() { None } else { Some(std::mem::transmute(symbol)) }
    }
});

static ORIGINAL_XSHMGETIMAGE: LazyLock<Option<XShmGetImageType>> = LazyLock::new(|| {
    unsafe {
        let handle = XEXT_HANDLE.0;
        if handle.is_null() { return None; }

        let symbol = libc::dlsym(handle, b"XShmGetImage\0".as_ptr() as *const c_char);
        if symbol.is_null() { None } else { Some(std::mem::transmute(symbol)) }
    }
});

static ORIGINAL_XSHMCREATEIMAGE: LazyLock<Option<XShmCreateImageType>> = LazyLock::new(|| {
    unsafe {
        let handle = XEXT_HANDLE.0;
        if handle.is_null() { return None; }

        let symbol = libc::dlsym(handle, b"XShmCreateImage\0".as_ptr() as *const c_char);
        if symbol.is_null() { None } else { Some(std::mem::transmute(symbol)) }
    }
});


#[unsafe(no_mangle)]
pub unsafe extern "C" fn XShmCreateImage(
    dpy: *mut x11::xlib::Display,
    visual: *mut x11::xlib::Visual,
    depth: c_uint,
    format: c_int,
    data: *mut c_char,
    shminfo: *mut x11::xshm::XShmSegmentInfo,
    width: c_uint,
    height: c_uint,
) -> *mut x11::xlib::XImage {
    let original = match &*ORIGINAL_XSHMCREATEIMAGE {
        Some(f) => f,
        None => return std::ptr::null_mut(),
    };
    
    let image = original(dpy, visual, depth, format, data, shminfo, width, height);
    
    let attach_count = G_XSHM_ATTACH_COUNT.load(Ordering::Acquire);
    if !image.is_null() && width >= 640 && height >= 480 && attach_count >= 1 {
        eprintln!(
            "[screenshare-hook] XShmCreateImage ({}x{}, attach={})",
            width, height, attach_count
        );
        G_CAPTURE_XIMAGE.store(image, Ordering::Release);
    }
    
    image
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn XShmAttach(
    dpy: *mut x11::xlib::Display,
    shminfo: *mut x11::xshm::XShmSegmentInfo,
) -> c_int {
    let original = match &*ORIGINAL_XSHMATTACH {
        Some(f) => f,
        None => return 0,
    };
    
    let result = original(dpy, shminfo);
    
    if is_caller_validated() {
        let count = G_XSHM_ATTACH_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
        eprintln!("[screenshare-hook] XShmAttach from meeting SDK (count={})", count);
        
        if count == 2 {
            thread::spawn(|| {
                start_portal_session();
            });
        }
    }
    
    result
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn XShmDetach(
    dpy: *mut x11::xlib::Display,
    shminfo: *mut x11::xshm::XShmSegmentInfo,
) -> c_int {
    G_INJECTOR_RUNNING.store(false, Ordering::Release);
    
    let original = match &*ORIGINAL_XSHMDETACH {
        Some(f) => f,
        None => return 0,
    };
    
    original(dpy, shminfo)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn XShmGetImage(
    dpy: *mut x11::xlib::Display,
    d: x11::xlib::Drawable,
    image: *mut x11::xlib::XImage,
    x: c_int,
    y: c_int,
    plane_mask: c_ulong,
) -> c_int {
    let original = match &*ORIGINAL_XSHMGETIMAGE {
        Some(f) => f,
        None => return 0,
    };
    let result = original(dpy, d, image, x, y, plane_mask);

    if !G_INJECTOR_RUNNING.load(Ordering::Acquire) && !image.is_null() {
        let mut guard = get_state().lock();
        ensure_capture_started(&mut guard);

        if let Some(cap) = &guard.capture {
            if let Some(frame) = cap.get_latest_frame() {
                if let Err(e) = inject_frame_once(&frame, image) {
                    eprintln!("[screenshare-hook] one-shot XShm injection failed: {}", e);
                }
            }
        }
    }

    result
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn XGetImage(
    dpy: *mut x11::xlib::Display,
    d: x11::xlib::Drawable,
    x: c_int,
    y: c_int,
    width: c_uint,
    height: c_uint,
    plane_mask: c_ulong,
    format: c_int,
) -> *mut x11::xlib::XImage {
    static ORIGINAL_XGETIMAGE: LazyLock<Option<unsafe extern "C" fn(
        *mut x11::xlib::Display,
        x11::xlib::Drawable,
        c_int,
        c_int,
        c_uint,
        c_uint,
        c_ulong,
        c_int,
    ) -> *mut x11::xlib::XImage>> = LazyLock::new(|| {
        unsafe {
            let handle = X11_HANDLE.0;
            if handle.is_null() { return None; }

            let symbol = libc::dlsym(handle, b"XGetImage\0".as_ptr() as *const c_char);
            if symbol.is_null() { None } else { Some(std::mem::transmute(symbol)) }
        }
    });


    let original = match &*ORIGINAL_XGETIMAGE {
        Some(f) => f,
        None => return std::ptr::null_mut(),
    };
    let image = original(dpy, d, x, y, width, height, plane_mask, format);

    if !G_INJECTOR_RUNNING.load(Ordering::Acquire) && !image.is_null() {
        let mut guard = get_state().lock();
        ensure_capture_started(&mut guard);

        if let Some(cap) = &guard.capture {
            if let Some(frame) = cap.get_latest_frame() {
                if let Err(e) = inject_frame_once(&frame, image) {
                    eprintln!("[screenshare-hook] one-shot XGetImage injection failed: {}", e);
                }
            }
        }
    }

    image
}


#[unsafe(no_mangle)]
pub unsafe extern "C" fn shmdt(shmaddr: *const libc::c_void) -> c_int {
    G_INJECTOR_RUNNING.store(false, Ordering::Release);
    
    static ORIGINAL_SHMDT: LazyLock<Option<unsafe extern "C" fn(*const libc::c_void) -> c_int>> = LazyLock::new(|| {
        unsafe {
            let mut handle = libc::dlopen(b"libc.so.6\0".as_ptr() as *const c_char, libc::RTLD_LAZY);
            if handle.is_null() {
                handle = libc::dlopen(b"libc.so\0".as_ptr() as *const c_char, libc::RTLD_LAZY);
            }
            if handle.is_null() { return None; }
            let symbol = libc::dlsym(handle, b"shmdt\0".as_ptr() as *const c_char);
            if symbol.is_null() { None } else { Some(std::mem::transmute(symbol)) }
        }
    });


    if let Some(f) = &*ORIGINAL_SHMDT {
        f(shmaddr)
    } else {
        -1
    }
}
