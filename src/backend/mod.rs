//! Scanner backends. `sane` drives libsane (loaded with dlopen); `fake` is an
//! in-process backend used by the test suite and by `AIRBRIDGE_BACKEND=fake`.

pub mod fake;
pub mod sane;

use std::sync::Arc;

/// One device as listed by `sane_get_devices`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub vendor: String,
    pub model: String,
    /// SANE's free-form "type" field. sane-airscan puts `ip=<addr>` here.
    pub kind: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptType {
    Bool,
    Int,
    Fixed,
    String,
    Button,
    Group,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    None,
    Pixel,
    Bit,
    Mm,
    Dpi,
    Percent,
    Microsecond,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Constraint {
    None,
    Range { min: f64, max: f64, quant: f64 },
    Words(Vec<f64>),
    Strings(Vec<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct OptDesc {
    pub name: String,
    pub ty: OptType,
    pub unit: Unit,
    pub constraint: Constraint,
    pub active: bool,
    pub settable: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OptValue {
    Bool(bool),
    Number(f64),
    Str(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameFormat {
    Gray,
    Rgb,
    /// Three-pass scanners deliver one colour per frame; not supported.
    SingleChannel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameParams {
    pub format: FrameFormat,
    pub last_frame: bool,
    pub bytes_per_line: usize,
    pub pixels_per_line: usize,
    /// `None` when the backend does not know the page length (common for ADFs).
    pub lines: Option<usize>,
    pub depth: u8,
}

/// A variant per decision the job manager makes.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// Document feeder is empty: end of an ADF job, or 409 on its first page.
    #[error("document feeder is empty")]
    NoDocs,
    #[error("paper jam")]
    Jammed,
    #[error("cover open")]
    CoverOpen,
    #[error("device busy")]
    Busy,
    #[error("cancelled")]
    Cancelled,
    /// The device did not answer: unreachable, asleep, unplugged.
    #[error("device unreachable: {0}")]
    Unreachable(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("backend failure: {0}")]
    Failed(String),
}

/// Asynchronous cancel. SANE allows `sane_cancel` from another thread.
pub type CancelHandle = Arc<dyn Fn() + Send + Sync>;

pub trait Backend: Send + Sync + 'static {
    fn devices(&self) -> Result<Vec<DeviceInfo>, BackendError>;
    /// Re-read backend configuration (e.g. after brsaneconfig4). Must only be
    /// called while no session is open.
    fn reload(&self) -> Result<(), BackendError>;
    fn open(&self, name: &str) -> Result<Box<dyn Session>, BackendError>;
    /// Number of open device handles; leak tests assert this returns to 0.
    fn open_handles(&self) -> usize;
}

/// An open device. Closing happens on drop.
pub trait Session: Send {
    fn options(&self) -> Vec<OptDesc>;
    fn set(&mut self, name: &str, value: &OptValue) -> Result<(), BackendError>;
    fn start(&mut self) -> Result<FrameParams, BackendError>;
    /// `Ok(0)` marks the end of the frame.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, BackendError>;
    fn cancel_handle(&self) -> CancelHandle;
    fn cancel(&mut self);
}
