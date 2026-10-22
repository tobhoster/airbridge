//! A deterministic in-process scanner. The test suite drives every edge case
//! through it (1-bit gray, empty feeder, multi-page feeder, a device that
//! stops answering, a wedged backend), and `AIRBRIDGE_BACKEND=fake` serves it
//! from the real binary for container smoke tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::{Condvar, Mutex};
use std::time::Duration;

use super::{
    Backend, BackendError, CancelHandle, Constraint, DeviceInfo, FrameFormat, FrameParams, OptDesc,
    OptType, OptValue, Session, Unit,
};

/// Knobs a test flips while the server runs.
#[derive(Clone, Debug, Default)]
pub struct Behavior {
    /// Sheets in the feeder.
    pub adf_pages: usize,
    /// Deliver 1-bit data whatever gray mode was chosen.
    pub gray_one_bit: bool,
    /// Report `lines = -1` like many feeders do.
    pub unknown_lines: bool,
    /// Fail with "unreachable" after this many bytes of a page.
    pub fail_after_bytes: Option<usize>,
    /// Stop sending after this many bytes until cancelled.
    pub hang_after_bytes: Option<usize>,
    /// Stop sending after this many bytes and ignore cancel (a wedged driver).
    pub wedge_after_bytes: Option<usize>,
    /// Delay inside `start` (warm-up).
    pub start_delay: Duration,
    /// Fail `open` (device gone).
    pub open_fails: bool,
    /// Deliver at most one line per read, sleeping this long first (a slow scanner).
    pub read_delay: Duration,
}

#[derive(Clone, Debug)]
pub struct FakeDevice {
    pub info: DeviceInfo,
    pub options: Vec<OptDesc>,
}

impl FakeDevice {
    /// The option set brscan4 reports for a Brother DCP-L2540DW (captured from
    /// `scanimage -A` against the Dell MFP E514dw).
    pub fn brother_dcp_l2540dw(name: &str, model: &str) -> Self {
        let range = |max: f64| Constraint::Range {
            min: 0.0,
            max,
            quant: 0.0999908,
        };
        let opt = |name: &str, ty, unit, constraint, active| OptDesc {
            name: name.to_owned(),
            ty,
            unit,
            constraint,
            active,
            settable: true,
        };
        Self {
            info: DeviceInfo {
                name: name.to_owned(),
                vendor: "Brother".to_owned(),
                model: model.to_owned(),
                kind: "DCP-L2540DW".to_owned(),
            },
            options: vec![
                opt(
                    "mode",
                    OptType::String,
                    Unit::None,
                    Constraint::Strings(
                        [
                            "Black & White",
                            "Gray[Error Diffusion]",
                            "True Gray",
                            "24bit Color[Fast]",
                        ]
                        .map(str::to_owned)
                        .to_vec(),
                    ),
                    true,
                ),
                opt(
                    "resolution",
                    OptType::Int,
                    Unit::Dpi,
                    Constraint::Words(vec![
                        100., 150., 200., 300., 400., 600., 1200., 2400., 4800., 9600.,
                    ]),
                    true,
                ),
                opt(
                    "source",
                    OptType::String,
                    Unit::None,
                    Constraint::Strings(
                        [
                            "FlatBed",
                            "Automatic Document Feeder(left aligned)",
                            "Automatic Document Feeder(centrally aligned)",
                        ]
                        .map(str::to_owned)
                        .to_vec(),
                    ),
                    true,
                ),
                opt(
                    "brightness",
                    OptType::Int,
                    Unit::Percent,
                    Constraint::Range {
                        min: -50.,
                        max: 50.,
                        quant: 1.,
                    },
                    false,
                ),
                opt(
                    "contrast",
                    OptType::Int,
                    Unit::Percent,
                    Constraint::Range {
                        min: -50.,
                        max: 50.,
                        quant: 1.,
                    },
                    false,
                ),
                opt("tl-x", OptType::Fixed, Unit::Mm, range(211.9), true),
                opt("tl-y", OptType::Fixed, Unit::Mm, range(355.6), true),
                opt("br-x", OptType::Fixed, Unit::Mm, range(211.9), true),
                opt("br-y", OptType::Fixed, Unit::Mm, range(355.6), true),
            ],
        }
    }

    /// A flatbed-only device whose only gray mode is plain "Gray".
    pub fn simple_flatbed(name: &str, vendor: &str, model: &str, kind: &str) -> Self {
        let mut dev = Self::brother_dcp_l2540dw(name, model);
        dev.info.vendor = vendor.to_owned();
        dev.info.kind = kind.to_owned();
        dev.options.retain(|o| o.name != "source");
        if let Some(mode) = dev.options.iter_mut().find(|o| o.name == "mode") {
            mode.constraint =
                Constraint::Strings(["Lineart", "Gray", "Color"].map(str::to_owned).to_vec());
        }
        if let Some(res) = dev.options.iter_mut().find(|o| o.name == "resolution") {
            res.constraint = Constraint::Words(vec![75., 150., 300., 600.]);
        }
        dev
    }
}

struct Shared {
    devices: Vec<FakeDevice>,
    behavior: Mutex<Behavior>,
    handles: AtomicUsize,
    /// sane_exit/sane_init cycles (each one leaks resources in real SANE).
    reloads: AtomicUsize,
    /// Releases wedged reads so tests do not leak threads.
    released: Mutex<bool>,
    wake: Condvar,
}

#[derive(Clone)]
pub struct FakeBackend {
    shared: Arc<Shared>,
    devices_override: Arc<Mutex<Option<Vec<FakeDevice>>>>,
}

impl FakeBackend {
    pub fn new(devices: Vec<FakeDevice>) -> Self {
        Self {
            shared: Arc::new(Shared {
                devices,
                behavior: Mutex::new(Behavior::default()),
                handles: AtomicUsize::new(0),
                reloads: AtomicUsize::new(0),
                released: Mutex::new(false),
                wake: Condvar::new(),
            }),
            devices_override: Arc::new(Mutex::new(None)),
        }
    }

    /// The backend `AIRBRIDGE_BACKEND=fake` serves: one Brother-profile device.
    pub fn demo() -> Self {
        let mut dev = FakeDevice::brother_dcp_l2540dw("fake:demo", "Demo Scanner");
        dev.info.vendor = "Airbridge".to_owned();
        Self::new(vec![dev])
    }

    pub fn update(&self, f: impl FnOnce(&mut Behavior)) {
        f(&mut self.shared.behavior.lock());
    }

    pub fn behavior(&self) -> Behavior {
        self.shared.behavior.lock().clone()
    }

    /// Replace the device list (simulates devices appearing or vanishing).
    pub fn set_devices(&self, devices: Vec<FakeDevice>) {
        *self.devices_override.lock() = Some(devices);
    }

    pub fn reload_count(&self) -> usize {
        self.shared.reloads.load(Ordering::SeqCst)
    }

    pub fn release_wedged(&self) {
        *self.shared.released.lock() = true;
        self.shared.wake.notify_all();
    }

    fn device_list(&self) -> Vec<FakeDevice> {
        self.devices_override
            .lock()
            .clone()
            .unwrap_or_else(|| self.shared.devices.clone())
    }
}

impl Backend for FakeBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, BackendError> {
        Ok(self.device_list().into_iter().map(|d| d.info).collect())
    }

    fn reload(&self) -> Result<(), BackendError> {
        if self.open_handles() != 0 {
            return Err(BackendError::Busy);
        }
        self.shared.reloads.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn open(&self, name: &str) -> Result<Box<dyn Session>, BackendError> {
        if self.behavior().open_fails {
            return Err(BackendError::Unreachable(format!("{name} does not answer")));
        }
        let Some(dev) = self.device_list().into_iter().find(|d| d.info.name == name) else {
            return Err(BackendError::Invalid(format!("no device {name}")));
        };
        self.shared.handles.fetch_add(1, Ordering::SeqCst);
        let mut values = Vec::new();
        for o in &dev.options {
            let v = match &o.constraint {
                Constraint::Strings(s) => OptValue::Str(s.first().cloned().unwrap_or_default()),
                Constraint::Words(w) => OptValue::Number(w.first().copied().unwrap_or(0.)),
                Constraint::Range { max, .. } if o.name.starts_with("br-") => {
                    OptValue::Number(*max)
                }
                _ => OptValue::Number(0.),
            };
            values.push((o.name.clone(), v));
        }
        Ok(Box::new(FakeSession {
            shared: Arc::clone(&self.shared),
            options: dev.options,
            values,
            cancel: Arc::new((Mutex::new(false), Condvar::new())),
            page: None,
        }))
    }

    fn open_handles(&self) -> usize {
        self.shared.handles.load(Ordering::SeqCst)
    }
}

struct Page {
    params: FrameParams,
    produced: usize,
    total: usize,
}

struct FakeSession {
    shared: Arc<Shared>,
    options: Vec<OptDesc>,
    values: Vec<(String, OptValue)>,
    cancel: Arc<(Mutex<bool>, Condvar)>,
    page: Option<Page>,
}

impl FakeSession {
    fn number(&self, name: &str) -> f64 {
        match self.values.iter().find(|(n, _)| n == name) {
            Some((_, OptValue::Number(n))) => *n,
            _ => 0.,
        }
    }

    fn string(&self, name: &str) -> String {
        match self.values.iter().find(|(n, _)| n == name) {
            Some((_, OptValue::Str(s))) => s.clone(),
            _ => String::new(),
        }
    }

    fn cancelled(&self) -> bool {
        *self.cancel.0.lock()
    }

    fn pixel(format: FrameFormat, x: usize, y: usize, c: usize) -> u8 {
        let base = (x * 3 + y * 5) % 256;
        match format {
            FrameFormat::Gray => base as u8,
            _ => ((base + c * 85) % 256) as u8,
        }
    }

    fn byte_at(&self, page: &Page, offset: usize) -> u8 {
        let p = page.params;
        let line = offset / p.bytes_per_line;
        let col = offset % p.bytes_per_line;
        match (p.format, p.depth) {
            (FrameFormat::Gray, 1) => {
                // Checkerboard of 8-pixel cells; SANE 1-bit: 1 = black.
                if (col + line / 8).is_multiple_of(2) {
                    0xFF
                } else {
                    0x00
                }
            }
            (FrameFormat::Gray, _) => Self::pixel(p.format, col, line, 0),
            _ => Self::pixel(p.format, col / 3, line, col % 3),
        }
    }
}

impl Session for FakeSession {
    fn options(&self) -> Vec<OptDesc> {
        self.options.clone()
    }

    fn set(&mut self, name: &str, value: &OptValue) -> Result<(), BackendError> {
        let Some(desc) = self.options.iter().find(|o| o.name == name) else {
            return Err(BackendError::Invalid(format!("no option {name}")));
        };
        let ok = match (&desc.constraint, value) {
            (Constraint::Strings(list), OptValue::Str(s)) => list.contains(s),
            (Constraint::Words(list), OptValue::Number(n)) => {
                list.iter().any(|w| (w - n).abs() < 1e-6)
            }
            (Constraint::Range { min, max, .. }, OptValue::Number(n)) => {
                *n >= min - 1e-6 && *n <= max + 1e-6
            }
            _ => false,
        };
        if !ok {
            return Err(BackendError::Invalid(format!("{name}={value:?}")));
        }
        if let Some(slot) = self.values.iter_mut().find(|(n, _)| n == name) {
            slot.1 = value.clone();
        }
        Ok(())
    }

    fn start(&mut self) -> Result<FrameParams, BackendError> {
        let behavior = self.shared.behavior.lock().clone();
        *self.cancel.0.lock() = false;
        if !behavior.start_delay.is_zero() {
            std::thread::sleep(behavior.start_delay);
        }
        let source = self.string("source");
        if source.contains("Document Feeder") {
            let mut b = self.shared.behavior.lock();
            if b.adf_pages == 0 {
                return Err(BackendError::NoDocs);
            }
            b.adf_pages -= 1;
        }
        let res = self.number("resolution");
        let width_mm = self.number("br-x") - self.number("tl-x");
        let height_mm = self.number("br-y") - self.number("tl-y");
        let px = |mm: f64| ((mm / 25.4 * res).round() as usize).max(1);
        let (w, h) = (px(width_mm), px(height_mm));
        let mode = self.string("mode");
        let (format, depth) = if mode.contains("Color") {
            (FrameFormat::Rgb, 8)
        } else if mode == "True Gray" || mode == "Gray" {
            (FrameFormat::Gray, if behavior.gray_one_bit { 1 } else { 8 })
        } else {
            (FrameFormat::Gray, 1)
        };
        let bytes_per_line = match (format, depth) {
            (FrameFormat::Gray, 1) => w.div_ceil(8),
            (FrameFormat::Gray, _) => w,
            _ => w * 3,
        };
        let params = FrameParams {
            format,
            last_frame: true,
            bytes_per_line,
            pixels_per_line: w,
            lines: if behavior.unknown_lines {
                None
            } else {
                Some(h)
            },
            depth,
        };
        self.page = Some(Page {
            params,
            produced: 0,
            total: bytes_per_line * h,
        });
        Ok(params)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, BackendError> {
        let behavior = self.behavior_snapshot();
        let Some(page) = self.page.as_ref() else {
            return Err(BackendError::Invalid("read before start".into()));
        };
        let produced = page.produced;
        if behavior.fail_after_bytes.is_some_and(|l| produced >= l) {
            return Err(BackendError::Unreachable(
                "scanner stopped answering".into(),
            ));
        }
        if behavior.hang_after_bytes.is_some_and(|l| produced >= l) {
            let mut cancelled = self.cancel.0.lock();
            while !*cancelled {
                self.cancel.1.wait(&mut cancelled);
            }
            return Err(BackendError::Cancelled);
        }
        if behavior.wedge_after_bytes.is_some_and(|l| produced >= l) {
            let mut released = self.shared.released.lock();
            while !*released {
                self.shared.wake.wait(&mut released);
            }
            return Err(BackendError::Failed("wedged backend released".into()));
        }
        if self.cancelled() {
            return Err(BackendError::Cancelled);
        }
        let remaining = page.total - produced;
        let mut n = remaining.min(buf.len());
        if !behavior.read_delay.is_zero() {
            std::thread::sleep(behavior.read_delay);
            n = n.min(page.params.bytes_per_line);
        }
        for cap in [
            behavior.fail_after_bytes,
            behavior.hang_after_bytes,
            behavior.wedge_after_bytes,
        ]
        .into_iter()
        .flatten()
        {
            if cap > produced {
                n = n.min(cap - produced);
            }
        }
        for (i, b) in buf[..n].iter_mut().enumerate() {
            *b = self.byte_at(page, produced + i);
        }
        if let Some(page) = self.page.as_mut() {
            page.produced += n;
        }
        Ok(n)
    }

    fn cancel_handle(&self) -> CancelHandle {
        let cancel = Arc::clone(&self.cancel);
        Arc::new(move || {
            *cancel.0.lock() = true;
            cancel.1.notify_all();
        })
    }

    fn cancel(&mut self) {
        (self.cancel_handle())();
    }
}

impl FakeSession {
    fn behavior_snapshot(&self) -> Behavior {
        self.shared.behavior.lock().clone()
    }
}

impl Drop for FakeSession {
    fn drop(&mut self) {
        self.shared.handles.fetch_sub(1, Ordering::SeqCst);
    }
}
