//! What a SANE device can do, expressed in eSCL terms, and how an eSCL ticket
//! maps back onto SANE option values.

use crate::backend::{Constraint, DeviceInfo, OptDesc, OptType, OptValue, Session, Unit};

pub const MM_PER_INCH: f64 = 25.4;
/// eSCL geometry unit: 1/300 inch.
pub const UNITS_PER_INCH: f64 = 300.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputSource {
    Platen,
    Feeder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    Gray8,
    Rgb24,
}

impl ColorMode {
    pub fn escl(self) -> &'static str {
        match self {
            Self::Gray8 => "Grayscale8",
            Self::Rgb24 => "RGB24",
        }
    }
    pub fn channels(self) -> usize {
        match self {
            Self::Gray8 => 1,
            Self::Rgb24 => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub device: DeviceInfo,
    /// SANE `source` value per eSCL input; `None` when the device has no
    /// source option (single-source flatbed).
    pub platen: Option<Option<String>>,
    pub feeder: Option<Option<String>>,
    pub gray_mode: Option<String>,
    pub color_mode: Option<String>,
    pub resolutions: Vec<u32>,
    /// Maximum area in 1/300 inch.
    pub max_width: u32,
    pub max_height: u32,
    pub geometry_unit: Unit,
}

const STANDARD_RESOLUTIONS: [u32; 9] = [75, 100, 150, 200, 300, 400, 600, 1200, 2400];

fn find<'a>(opts: &'a [OptDesc], name: &str) -> Option<&'a OptDesc> {
    opts.iter().find(|o| o.name == name)
}

fn strings(o: Option<&OptDesc>) -> Vec<String> {
    match o.map(|o| &o.constraint) {
        Some(Constraint::Strings(s)) => s.clone(),
        _ => Vec::new(),
    }
}

/// A gray mode that yields real 8-bit gray. Brother's "Gray[Error Diffusion]"
/// is dithered 1-bit and must never win over "True Gray".
pub fn pick_gray(modes: &[String]) -> Option<String> {
    let dithered = |m: &str| {
        let l = m.to_ascii_lowercase();
        l.contains("diffusion") || l.contains("halftone") || l.contains("dither")
    };
    let rank = |m: &String| -> Option<u8> {
        let l = m.to_ascii_lowercase();
        if l == "true gray" || l == "true grey" {
            Some(0)
        } else if l == "gray" || l == "grey" || l == "grayscale" {
            Some(1)
        } else if (l.contains("gray") || l.contains("grey")) && !dithered(m) {
            Some(2)
        } else if l.contains("gray") || l.contains("grey") {
            Some(3)
        } else {
            None
        }
    };
    modes
        .iter()
        .filter_map(|m| rank(m).map(|r| (r, m)))
        .min_by_key(|(r, _)| *r)
        .map(|(_, m)| m.clone())
}

pub fn pick_color(modes: &[String]) -> Option<String> {
    modes
        .iter()
        .find(|m| m.eq_ignore_ascii_case("color"))
        .or_else(|| {
            modes
                .iter()
                .find(|m| m.to_ascii_lowercase().contains("color"))
        })
        .cloned()
}

fn classify_source(name: &str) -> Option<InputSource> {
    let l = name.to_ascii_lowercase();
    if l.contains("duplex") {
        None
    } else if l.contains("adf") || l.contains("feeder") || l.contains("automatic document") {
        Some(InputSource::Feeder)
    } else if l.contains("flatbed") || l.contains("platen") || l == "normal" {
        Some(InputSource::Platen)
    } else {
        None
    }
}

fn to_units(value: f64, unit: Unit, dpi: f64) -> u32 {
    let inches = match unit {
        Unit::Pixel => value / dpi.max(1.0),
        _ => value / MM_PER_INCH,
    };
    (inches * UNITS_PER_INCH).round().max(0.0) as u32
}

impl Profile {
    /// Derive a profile from a device's option list. `None` when the device
    /// cannot produce gray or color images (e.g. a webcam).
    pub fn from_options(device: DeviceInfo, opts: &[OptDesc]) -> Option<Self> {
        let modes = strings(find(opts, "mode"));
        let (gray_mode, color_mode) = if modes.is_empty() {
            (Some(String::new()), None)
        } else {
            (pick_gray(&modes), pick_color(&modes))
        };
        if gray_mode.is_none() && color_mode.is_none() {
            return None;
        }
        let resolutions: Vec<u32> = match find(opts, "resolution").map(|o| &o.constraint) {
            Some(Constraint::Words(w)) => {
                let mut v: Vec<u32> = w
                    .iter()
                    .map(|r| r.round() as u32)
                    .filter(|r| *r > 0)
                    .collect();
                v.sort_unstable();
                v.dedup();
                v
            }
            Some(Constraint::Range { min, max, .. }) => STANDARD_RESOLUTIONS
                .iter()
                .copied()
                .filter(|r| f64::from(*r) >= *min && f64::from(*r) <= *max)
                .collect(),
            _ => vec![300],
        };
        if resolutions.is_empty() {
            return None;
        }
        let sources = strings(find(opts, "source"));
        let (platen, feeder) = if sources.is_empty() {
            (Some(None), None)
        } else {
            let pick = |kind| {
                sources
                    .iter()
                    .find(|s| classify_source(s) == Some(kind))
                    .cloned()
                    .map(Some)
            };
            (pick(InputSource::Platen), pick(InputSource::Feeder))
        };
        if platen.is_none() && feeder.is_none() {
            return None;
        }
        let br = |name: &str| match find(opts, name) {
            Some(OptDesc {
                constraint: Constraint::Range { max, .. },
                unit,
                ..
            }) => Some((*max, *unit)),
            _ => None,
        };
        let ref_dpi = f64::from(
            resolutions
                .iter()
                .copied()
                .find(|r| *r >= 300)
                .unwrap_or(resolutions[0]),
        );
        let (max_width, max_height, geometry_unit) = match (br("br-x"), br("br-y")) {
            (Some((x, ux)), Some((y, uy))) => {
                (to_units(x, ux, ref_dpi), to_units(y, uy, ref_dpi), ux)
            }
            // No geometry options: assume US Letter.
            _ => (2550, 3300, Unit::None),
        };
        Some(Self {
            device,
            platen,
            feeder,
            gray_mode,
            color_mode,
            resolutions,
            max_width,
            max_height,
            geometry_unit,
        })
    }

    pub fn sources(&self) -> impl Iterator<Item = InputSource> + '_ {
        [
            (InputSource::Platen, &self.platen),
            (InputSource::Feeder, &self.feeder),
        ]
        .into_iter()
        .filter(|(_, s)| s.is_some())
        .map(|(k, _)| k)
    }

    pub fn color_modes(&self) -> Vec<ColorMode> {
        let mut v = Vec::new();
        if self.gray_mode.is_some() {
            v.push(ColorMode::Gray8);
        }
        if self.color_mode.is_some() {
            v.push(ColorMode::Rgb24);
        }
        v
    }

    fn sane_mode(&self, mode: ColorMode) -> Option<&str> {
        match mode {
            ColorMode::Gray8 => self.gray_mode.as_deref(),
            ColorMode::Rgb24 => self.color_mode.as_deref(),
        }
        .filter(|m| !m.is_empty())
    }
}

/// A validated scan request, ready to apply to a session.
#[derive(Clone, Debug, PartialEq)]
pub struct PageRequest {
    pub source: InputSource,
    pub color: ColorMode,
    pub resolution: u32,
    /// x, y, width, height in 1/300 inch.
    pub region: (u32, u32, u32, u32),
}

fn number_option(opts: &[OptDesc], name: &str) -> bool {
    find(opts, name).is_some_and(|o| o.active && matches!(o.ty, OptType::Int | OptType::Fixed))
}

/// Apply a request to an open session. Order matters: source first (it can
/// change the other options' ranges), then mode, resolution and geometry.
pub fn apply(
    profile: &Profile,
    session: &mut dyn Session,
    req: &PageRequest,
) -> Result<(), crate::backend::BackendError> {
    let source = match req.source {
        InputSource::Platen => profile.platen.clone().flatten(),
        InputSource::Feeder => profile.feeder.clone().flatten(),
    };
    if let Some(source) = source {
        session.set("source", &OptValue::Str(source))?;
    }
    if let Some(mode) = profile.sane_mode(req.color) {
        session.set("mode", &OptValue::Str(mode.to_owned()))?;
    }
    let opts = session.options();
    if number_option(&opts, "resolution") {
        session.set("resolution", &OptValue::Number(f64::from(req.resolution)))?;
    }
    let (x, y, w, h) = req.region;
    let conv = |units: u32| -> f64 {
        let inches = f64::from(units) / UNITS_PER_INCH;
        match profile.geometry_unit {
            Unit::Pixel => inches * f64::from(req.resolution),
            _ => inches * MM_PER_INCH,
        }
    };
    let clamp = |name: &str, v: f64| match find(&opts, name).map(|o| &o.constraint) {
        Some(Constraint::Range { min, max, .. }) => v.clamp(*min, *max),
        _ => v,
    };
    for (name, value) in [("tl-x", x), ("tl-y", y), ("br-x", x + w), ("br-y", y + h)] {
        if number_option(&opts, name) {
            session.set(name, &OptValue::Number(clamp(name, conv(value))))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn true_gray_beats_dithered_gray() {
        let modes = s(&[
            "Black & White",
            "Gray[Error Diffusion]",
            "True Gray",
            "24bit Color[Fast]",
        ]);
        assert_eq!(pick_gray(&modes).as_deref(), Some("True Gray"));
        assert_eq!(pick_color(&modes).as_deref(), Some("24bit Color[Fast]"));
    }

    #[test]
    fn plain_gray_and_lineart() {
        assert_eq!(
            pick_gray(&s(&["Lineart", "Gray", "Color"])).as_deref(),
            Some("Gray")
        );
        assert_eq!(pick_gray(&s(&["Lineart", "Color"])), None);
    }

    #[test]
    fn dithered_gray_is_last_resort() {
        assert_eq!(
            pick_gray(&s(&["Gray[Error Diffusion]", "Color"])).as_deref(),
            Some("Gray[Error Diffusion]")
        );
    }
}
