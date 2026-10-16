//! ScanSettings parsing and validation. Input is untrusted network data: the
//! parser is bounded (size, depth, element count) and every failure is a
//! `TicketError`, which the HTTP layer answers with 409.

use quick_xml::events::Event;

use crate::imaging::DocFormat;
use crate::scanner::{ColorMode, InputSource, PageRequest, Profile};

pub const MAX_TICKET_BYTES: usize = 64 * 1024;
const MAX_DEPTH: usize = 16;
const MAX_ELEMENTS: usize = 512;
const MAX_TEXT: usize = 256;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TicketError {
    #[error("malformed ScanSettings: {0}")]
    Malformed(String),
    #[error("unsupported input source {0:?}")]
    InputSource(String),
    #[error("unsupported color mode {0:?}")]
    ColorMode(String),
    #[error("unsupported resolution {0}x{1}")]
    Resolution(u32, u32),
    #[error("unsupported document format {0:?}")]
    Format(String),
    #[error("scan region {0}")]
    Region(String),
}

/// Raw fields as sent by the client.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RawTicket {
    pub input_source: Option<String>,
    pub color_mode: Option<String>,
    pub x_resolution: Option<String>,
    pub y_resolution: Option<String>,
    pub document_format: Option<String>,
    pub document_format_ext: Option<String>,
    pub region: Option<RawRegion>,
    pub intent: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RawRegion {
    pub width: Option<String>,
    pub height: Option<String>,
    pub x_offset: Option<String>,
    pub y_offset: Option<String>,
    pub units: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ticket {
    pub page: PageRequest,
    pub format: DocFormat,
}

pub fn parse(body: &[u8]) -> Result<RawTicket, TicketError> {
    if body.len() > MAX_TICKET_BYTES {
        return Err(TicketError::Malformed("too large".into()));
    }
    let mut reader = quick_xml::Reader::from_reader(body);
    reader.trim_text(true);
    let mut path: Vec<Vec<u8>> = Vec::new();
    let mut raw = RawTicket::default();
    let mut regions = 0;
    let mut elements = 0;
    let mut root_seen = false;
    let mut buf = Vec::new();
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| TicketError::Malformed(e.to_string()))?;
        match event {
            Event::Start(e) => {
                elements += 1;
                if elements > MAX_ELEMENTS || path.len() >= MAX_DEPTH {
                    return Err(TicketError::Malformed("document too complex".into()));
                }
                let name = e.local_name().as_ref().to_vec();
                if path.is_empty() {
                    if name != b"ScanSettings" {
                        return Err(TicketError::Malformed(
                            "root element is not ScanSettings".into(),
                        ));
                    }
                    root_seen = true;
                }
                if name == b"ScanRegion" {
                    regions += 1;
                    if regions > 1 {
                        return Err(TicketError::Region("more than one region".into()));
                    }
                    raw.region = Some(RawRegion::default());
                }
                path.push(name);
            }
            Event::Empty(_) => {
                elements += 1;
                if elements > MAX_ELEMENTS {
                    return Err(TicketError::Malformed("document too complex".into()));
                }
                if path.is_empty() {
                    return Err(TicketError::Malformed("empty root".into()));
                }
            }
            Event::End(_) => {
                path.pop();
            }
            Event::Text(t) => {
                let text = t
                    .unescape()
                    .map_err(|e| TicketError::Malformed(e.to_string()))?;
                if text.len() > MAX_TEXT {
                    return Err(TicketError::Malformed("value too long".into()));
                }
                let value = Some(text.trim().to_owned());
                let leaf = path.last().map(Vec::as_slice);
                let in_region = path.iter().any(|p| p == b"ScanRegion");
                match (leaf, in_region) {
                    (Some(b"Width"), true) => raw.region.get_or_insert_default().width = value,
                    (Some(b"Height"), true) => raw.region.get_or_insert_default().height = value,
                    (Some(b"XOffset"), true) => raw.region.get_or_insert_default().x_offset = value,
                    (Some(b"YOffset"), true) => raw.region.get_or_insert_default().y_offset = value,
                    (Some(b"ContentRegionUnits"), true) => {
                        raw.region.get_or_insert_default().units = value
                    }
                    (Some(b"InputSource"), _) => raw.input_source = value,
                    (Some(b"ColorMode"), _) => raw.color_mode = value,
                    (Some(b"XResolution"), _) => raw.x_resolution = value,
                    (Some(b"YResolution"), _) => raw.y_resolution = value,
                    (Some(b"DocumentFormat"), _) => raw.document_format = value,
                    (Some(b"DocumentFormatExt"), _) => raw.document_format_ext = value,
                    (Some(b"Intent"), _) => raw.intent = value,
                    _ => {}
                }
            }
            Event::Eof => break,
            Event::DocType(_) => return Err(TicketError::Malformed("DOCTYPE not allowed".into())),
            _ => {}
        }
        buf.clear();
    }
    if !root_seen || !path.is_empty() {
        return Err(TicketError::Malformed("incomplete document".into()));
    }
    Ok(raw)
}

fn number(v: &str, what: &str) -> Result<u32, TicketError> {
    v.parse::<u32>()
        .map_err(|_| TicketError::Malformed(format!("{what} is not a number: {v:?}")))
}

pub fn validate(raw: &RawTicket, p: &Profile) -> Result<Ticket, TicketError> {
    let source = match raw.input_source.as_deref() {
        None => p
            .sources()
            .next()
            .ok_or_else(|| TicketError::InputSource("none".into()))?,
        Some("Platen") => InputSource::Platen,
        Some("Feeder") => InputSource::Feeder,
        Some(other) => return Err(TicketError::InputSource(other.to_owned())),
    };
    if !p.sources().any(|s| s == source) {
        return Err(TicketError::InputSource(format!("{source:?}")));
    }
    let modes = p.color_modes();
    let color = match raw.color_mode.as_deref() {
        None => *modes
            .last()
            .ok_or_else(|| TicketError::ColorMode("none".into()))?,
        Some("Grayscale8") => ColorMode::Gray8,
        Some("RGB24") => ColorMode::Rgb24,
        Some(other) => return Err(TicketError::ColorMode(other.to_owned())),
    };
    if !modes.contains(&color) {
        return Err(TicketError::ColorMode(color.escl().to_owned()));
    }
    let default_res = p
        .resolutions
        .iter()
        .copied()
        .find(|r| *r >= 300)
        .unwrap_or(p.resolutions[0]);
    let xr = raw
        .x_resolution
        .as_deref()
        .map(|v| number(v, "XResolution"))
        .transpose()?;
    let yr = raw
        .y_resolution
        .as_deref()
        .map(|v| number(v, "YResolution"))
        .transpose()?;
    let (xr, yr) = match (xr, yr) {
        (None, None) => (default_res, default_res),
        (Some(x), None) => (x, x),
        (None, Some(y)) => (y, y),
        (Some(x), Some(y)) => (x, y),
    };
    if xr != yr || !p.resolutions.contains(&xr) {
        return Err(TicketError::Resolution(xr, yr));
    }
    let mime = raw
        .document_format_ext
        .as_deref()
        .or(raw.document_format.as_deref())
        .unwrap_or("image/jpeg");
    let format = DocFormat::from_mime(mime).ok_or_else(|| TicketError::Format(mime.to_owned()))?;
    let region = match &raw.region {
        None => (0, 0, p.max_width, p.max_height),
        Some(r) => {
            match r.units.as_deref() {
                None | Some("escl:ThreeHundredthsOfInches") | Some("ThreeHundredthsOfInches") => {}
                Some(other) => {
                    return Err(TicketError::Region(format!("unsupported units {other:?}")));
                }
            }
            let get = |v: &Option<String>, what: &str, default: u32| {
                v.as_deref().map_or(Ok(default), |v| number(v, what))
            };
            let x = get(&r.x_offset, "XOffset", 0)?;
            let y = get(&r.y_offset, "YOffset", 0)?;
            let w = get(&r.width, "Width", p.max_width)?;
            let h = get(&r.height, "Height", p.max_height)?;
            if w == 0 || h == 0 {
                return Err(TicketError::Region("is empty".into()));
            }
            let fits =
                |off: u32, len: u32, max: u32| off.checked_add(len).is_some_and(|end| end <= max);
            if !fits(x, w, p.max_width) || !fits(y, h, p.max_height) {
                return Err(TicketError::Region(format!(
                    "{x},{y} {w}x{h} exceeds {}x{}",
                    p.max_width, p.max_height
                )));
            }
            (x, y, w, h)
        }
    };
    Ok(Ticket {
        page: PageRequest {
            source,
            color,
            resolution: xr,
            region,
        },
        format,
    })
}
