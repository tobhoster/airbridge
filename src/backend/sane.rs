//! libsane loaded at runtime with dlopen, so the binary has no build-time SANE
//! dependency and the same image runs with or without scanner support.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::{Mutex, MutexGuard};

use super::{
    Backend, BackendError, CancelHandle, Constraint, DeviceInfo, FrameFormat, FrameParams, OptDesc,
    OptType, OptValue, Session, Unit,
};

type Word = c_int;
type Handle = *mut c_void;

#[repr(C)]
struct RawDevice {
    name: *const c_char,
    vendor: *const c_char,
    model: *const c_char,
    kind: *const c_char,
}

#[repr(C)]
struct RawOptionDescriptor {
    name: *const c_char,
    title: *const c_char,
    desc: *const c_char,
    ty: c_int,
    unit: c_int,
    size: Word,
    cap: Word,
    constraint_type: c_int,
    constraint: *const c_void,
}

#[repr(C)]
struct RawRange {
    min: Word,
    max: Word,
    quant: Word,
}

#[repr(C)]
#[derive(Default)]
struct RawParameters {
    format: c_int,
    last_frame: Word,
    bytes_per_line: Word,
    pixels_per_line: Word,
    lines: Word,
    depth: Word,
}

type FnInit = unsafe extern "C" fn(*mut Word, *const c_void) -> c_int;
type FnExit = unsafe extern "C" fn();
type FnGetDevices = unsafe extern "C" fn(*mut *const *const RawDevice, Word) -> c_int;
type FnOpen = unsafe extern "C" fn(*const c_char, *mut Handle) -> c_int;
type FnClose = unsafe extern "C" fn(Handle);
type FnGetOption = unsafe extern "C" fn(Handle, Word) -> *const RawOptionDescriptor;
type FnControl = unsafe extern "C" fn(Handle, Word, c_int, *mut c_void, *mut Word) -> c_int;
type FnGetParameters = unsafe extern "C" fn(Handle, *mut RawParameters) -> c_int;
type FnStart = unsafe extern "C" fn(Handle) -> c_int;
type FnRead = unsafe extern "C" fn(Handle, *mut u8, Word, *mut Word) -> c_int;
type FnCancel = unsafe extern "C" fn(Handle);

struct Api {
    init: FnInit,
    exit: FnExit,
    get_devices: FnGetDevices,
    open: FnOpen,
    close: FnClose,
    get_option: FnGetOption,
    control: FnControl,
    get_parameters: FnGetParameters,
    start: FnStart,
    read: FnRead,
    cancel: FnCancel,
    // Keeps the function pointers above valid.
    _lib: libloading::Library,
}

const STATUS_GOOD: c_int = 0;
const STATUS_EOF: c_int = 5;
const ACTION_SET: c_int = 1;
const CAP_SOFT_SELECT: Word = 1;
const CAP_INACTIVE: Word = 32;
const FIXED_SCALE: f64 = 65536.0;

fn status_error(status: c_int, context: &str) -> BackendError {
    match status {
        2 => BackendError::Cancelled,
        3 => BackendError::Busy,
        4 => BackendError::Invalid(context.to_owned()),
        6 => BackendError::Jammed,
        7 => BackendError::NoDocs,
        8 => BackendError::CoverOpen,
        9 => BackendError::Unreachable(format!("{context}: I/O error")),
        other => BackendError::Failed(format!("{context}: SANE status {other}")),
    }
}

/// SANE is not thread safe except for `sane_cancel`; every other call holds
/// this lock.
pub struct SaneBackend {
    api: Arc<Api>,
    lock: Arc<Mutex<()>>,
    handles: Arc<AtomicUsize>,
}

impl SaneBackend {
    pub fn load(library: &str) -> Result<Self, BackendError> {
        // SAFETY: loading libsane runs its constructors; that is the purpose.
        let lib = unsafe { libloading::Library::new(library) }
            .map_err(|e| BackendError::Failed(format!("dlopen {library}: {e}")))?;
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: the type matches the SANE 1.0 C prototype.
                *unsafe { lib.get(concat!($name, "\0").as_bytes()) }
                    .map_err(|e| BackendError::Failed(format!("{}: {e}", $name)))?
            };
        }
        let api = Api {
            init: sym!("sane_init"),
            exit: sym!("sane_exit"),
            get_devices: sym!("sane_get_devices"),
            open: sym!("sane_open"),
            close: sym!("sane_close"),
            get_option: sym!("sane_get_option_descriptor"),
            control: sym!("sane_control_option"),
            get_parameters: sym!("sane_get_parameters"),
            start: sym!("sane_start"),
            read: sym!("sane_read"),
            cancel: sym!("sane_cancel"),
            _lib: lib,
        };
        let backend = Self {
            api: Arc::new(api),
            lock: Arc::new(Mutex::new(())),
            handles: Arc::new(AtomicUsize::new(0)),
        };
        backend.init()?;
        Ok(backend)
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock()
    }

    fn init(&self) -> Result<(), BackendError> {
        let _g = self.guard();
        let mut version: Word = 0;
        // SAFETY: valid out pointer, no auth callback.
        let status = unsafe { (self.api.init)(&mut version, std::ptr::null()) };
        if status != STATUS_GOOD {
            return Err(status_error(status, "sane_init"));
        }
        Ok(())
    }
}

impl Drop for SaneBackend {
    fn drop(&mut self) {
        let _g = self.guard();
        // SAFETY: matched with sane_init in load().
        unsafe { (self.api.exit)() };
    }
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: SANE strings are NUL-terminated and live while the list does.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

impl Backend for SaneBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, BackendError> {
        let _g = self.guard();
        let mut list: *const *const RawDevice = std::ptr::null();
        // SAFETY: valid out pointer; local_only = false (network scanners too).
        let status = unsafe { (self.api.get_devices)(&mut list, 0) };
        if status != STATUS_GOOD {
            return Err(status_error(status, "sane_get_devices"));
        }
        let mut out = Vec::new();
        let mut i = 0;
        while !list.is_null() {
            // SAFETY: NULL-terminated array owned by SANE until the next call.
            let dev = unsafe { *list.add(i) };
            if dev.is_null() {
                break;
            }
            // SAFETY: non-null entry of the array above.
            let d = unsafe { &*dev };
            out.push(DeviceInfo {
                name: cstr(d.name),
                vendor: cstr(d.vendor),
                model: cstr(d.model),
                kind: cstr(d.kind),
            });
            i += 1;
        }
        Ok(out)
    }

    fn reload(&self) -> Result<(), BackendError> {
        if self.open_handles() != 0 {
            return Err(BackendError::Busy);
        }
        {
            let _g = self.guard();
            // SAFETY: no handle is open (checked above).
            unsafe { (self.api.exit)() };
        }
        self.init()
    }

    fn open(&self, name: &str) -> Result<Box<dyn Session>, BackendError> {
        let cname = CString::new(name).map_err(|_| BackendError::Invalid(name.to_owned()))?;
        let mut handle: Handle = std::ptr::null_mut();
        {
            let _g = self.guard();
            // SAFETY: valid C string and out pointer.
            let status = unsafe { (self.api.open)(cname.as_ptr(), &mut handle) };
            if status != STATUS_GOOD {
                return Err(status_error(status, "sane_open"));
            }
        }
        self.handles.fetch_add(1, Ordering::SeqCst);
        let session = SaneSession {
            api: Arc::clone(&self.api),
            lock: Arc::clone(&self.lock),
            handles: Arc::clone(&self.handles),
            handle: SendHandle(handle),
            alive: Arc::new(Mutex::new(true)),
            options: Vec::new(),
        };
        let mut session = session;
        session.options = session.load_options();
        Ok(Box::new(session))
    }

    fn open_handles(&self) -> usize {
        self.handles.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Copy)]
struct SendHandle(Handle);

impl SendHandle {
    /// Method access makes closures capture the whole (Send) wrapper rather
    /// than the raw pointer field.
    fn ptr(self) -> Handle {
        self.0
    }
}
// SAFETY: a SANE handle is an opaque token; access is serialised by `lock`
// except sane_cancel, which SANE allows from any thread.
unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

struct SaneSession {
    api: Arc<Api>,
    lock: Arc<Mutex<()>>,
    handles: Arc<AtomicUsize>,
    handle: SendHandle,
    /// Cleared before close so a late cancel never touches a closed handle.
    alive: Arc<Mutex<bool>>,
    /// (option index, descriptor)
    options: Vec<(Word, OptDesc)>,
}

impl SaneSession {
    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock()
    }

    fn load_options(&self) -> Vec<(Word, OptDesc)> {
        let _g = self.guard();
        let mut out = Vec::new();
        for index in 1..1024 {
            // SAFETY: SANE returns NULL past the last option.
            let p = unsafe { (self.api.get_option)(self.handle.0, index) };
            if p.is_null() {
                break;
            }
            // SAFETY: non-null descriptor valid until the next control call.
            let d = unsafe { &*p };
            let ty = match d.ty {
                0 => OptType::Bool,
                1 => OptType::Int,
                2 => OptType::Fixed,
                3 => OptType::String,
                4 => OptType::Button,
                _ => OptType::Group,
            };
            let unit = match d.unit {
                1 => Unit::Pixel,
                2 => Unit::Bit,
                3 => Unit::Mm,
                4 => Unit::Dpi,
                5 => Unit::Percent,
                6 => Unit::Microsecond,
                _ => Unit::None,
            };
            let scale = |w: Word| {
                if ty == OptType::Fixed {
                    f64::from(w) / FIXED_SCALE
                } else {
                    f64::from(w)
                }
            };
            let constraint = match d.constraint_type {
                1 if !d.constraint.is_null() => {
                    // SAFETY: constraint type RANGE points at a SANE_Range.
                    let r = unsafe { &*(d.constraint as *const RawRange) };
                    Constraint::Range {
                        min: scale(r.min),
                        max: scale(r.max),
                        quant: scale(r.quant),
                    }
                }
                2 if !d.constraint.is_null() => {
                    let words = d.constraint as *const Word;
                    // SAFETY: WORD_LIST: first word is the count.
                    let n = unsafe { *words }.max(0) as usize;
                    Constraint::Words(
                        // SAFETY: n words follow the count.
                        (1..=n).map(|i| scale(unsafe { *words.add(i) })).collect(),
                    )
                }
                3 if !d.constraint.is_null() => {
                    let list = d.constraint as *const *const c_char;
                    let mut v = Vec::new();
                    let mut i = 0;
                    loop {
                        // SAFETY: NULL-terminated string list.
                        let s = unsafe { *list.add(i) };
                        if s.is_null() {
                            break;
                        }
                        v.push(cstr(s));
                        i += 1;
                    }
                    Constraint::Strings(v)
                }
                _ => Constraint::None,
            };
            out.push((
                index,
                OptDesc {
                    name: cstr(d.name),
                    ty,
                    unit,
                    constraint,
                    active: d.cap & CAP_INACTIVE == 0,
                    settable: d.cap & CAP_SOFT_SELECT != 0,
                },
            ));
        }
        out
    }
}

impl Session for SaneSession {
    fn options(&self) -> Vec<OptDesc> {
        self.options.iter().map(|(_, d)| d.clone()).collect()
    }

    fn set(&mut self, name: &str, value: &OptValue) -> Result<(), BackendError> {
        let Some((index, desc)) = self.options.iter().find(|(_, d)| d.name == name) else {
            return Err(BackendError::Invalid(format!("no option {name}")));
        };
        let index = *index;
        let status = {
            let _g = self.guard();
            let mut info: Word = 0;
            match (desc.ty, value) {
                (OptType::String, OptValue::Str(s)) => {
                    let mut buf = s.as_bytes().to_vec();
                    buf.push(0);
                    buf.resize(buf.len().max(256), 0);
                    // SAFETY: NUL-terminated buffer at least as large as the option.
                    unsafe {
                        (self.api.control)(
                            self.handle.0,
                            index,
                            ACTION_SET,
                            buf.as_mut_ptr().cast(),
                            &mut info,
                        )
                    }
                }
                (OptType::Fixed, OptValue::Number(n)) => {
                    let mut w: Word = (n * FIXED_SCALE).round() as Word;
                    // SAFETY: single-word option.
                    unsafe {
                        (self.api.control)(
                            self.handle.0,
                            index,
                            ACTION_SET,
                            (&mut w as *mut Word).cast(),
                            &mut info,
                        )
                    }
                }
                (OptType::Int, OptValue::Number(n)) => {
                    let mut w: Word = n.round() as Word;
                    // SAFETY: single-word option.
                    unsafe {
                        (self.api.control)(
                            self.handle.0,
                            index,
                            ACTION_SET,
                            (&mut w as *mut Word).cast(),
                            &mut info,
                        )
                    }
                }
                (OptType::Bool, OptValue::Bool(b)) => {
                    let mut w: Word = Word::from(*b);
                    // SAFETY: single-word option.
                    unsafe {
                        (self.api.control)(
                            self.handle.0,
                            index,
                            ACTION_SET,
                            (&mut w as *mut Word).cast(),
                            &mut info,
                        )
                    }
                }
                _ => return Err(BackendError::Invalid(format!("type mismatch for {name}"))),
            }
        };
        if status != STATUS_GOOD {
            return Err(status_error(status, name));
        }
        // Setting an option may change others (e.g. source changes ranges).
        self.options = self.load_options();
        Ok(())
    }

    fn start(&mut self) -> Result<FrameParams, BackendError> {
        let _g = self.guard();
        // SAFETY: open handle.
        let status = unsafe { (self.api.start)(self.handle.0) };
        if status != STATUS_GOOD {
            return Err(status_error(status, "sane_start"));
        }
        let mut p = RawParameters::default();
        // SAFETY: valid out pointer.
        let status = unsafe { (self.api.get_parameters)(self.handle.0, &mut p) };
        if status != STATUS_GOOD {
            return Err(status_error(status, "sane_get_parameters"));
        }
        Ok(FrameParams {
            format: match p.format {
                0 => FrameFormat::Gray,
                1 => FrameFormat::Rgb,
                _ => FrameFormat::SingleChannel,
            },
            last_frame: p.last_frame != 0,
            bytes_per_line: p.bytes_per_line.max(0) as usize,
            pixels_per_line: p.pixels_per_line.max(0) as usize,
            lines: usize::try_from(p.lines).ok().filter(|&l| l > 0),
            depth: p.depth.clamp(0, 255) as u8,
        })
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, BackendError> {
        let _g = self.guard();
        let mut len: Word = 0;
        let max = Word::try_from(buf.len()).unwrap_or(Word::MAX);
        // SAFETY: buffer of at least `max` bytes.
        let status = unsafe { (self.api.read)(self.handle.0, buf.as_mut_ptr(), max, &mut len) };
        match status {
            STATUS_GOOD => Ok(len.max(0) as usize),
            STATUS_EOF => Ok(0),
            other => Err(status_error(other, "sane_read")),
        }
    }

    fn cancel_handle(&self) -> CancelHandle {
        let api = Arc::clone(&self.api);
        let handle = self.handle;
        let alive = Arc::clone(&self.alive);
        Arc::new(move || {
            let alive = alive.lock();
            if *alive {
                // SAFETY: sane_cancel is allowed from any thread; `alive`
                // proves the handle is still open.
                unsafe { (api.cancel)(handle.ptr()) };
            }
        })
    }

    fn cancel(&mut self) {
        // SAFETY: open handle.
        unsafe { (self.api.cancel)(self.handle.0) };
    }
}

impl Drop for SaneSession {
    fn drop(&mut self) {
        *self.alive.lock() = false;
        {
            let _g = self.guard();
            // SAFETY: closes the handle opened in open(); cancel first so a
            // backend mid-frame releases the device.
            unsafe {
                (self.api.cancel)(self.handle.0);
                (self.api.close)(self.handle.0);
            }
        }
        self.handles.fetch_sub(1, Ordering::SeqCst);
    }
}
