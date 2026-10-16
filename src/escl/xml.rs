//! eSCL documents. The layout matches AirSane's output byte for byte (golden
//! tests compare against captured AirSane responses), because that layout is
//! known to work with macOS Image Capture.
//!
//! `let _ = write!(out, ..)` below targets a `String`, whose `fmt::Write`
//! implementation never returns an error.

use std::fmt::Write;

use crate::scanner::{ColorMode, InputSource, Profile};

const HEAD: &str = "<?xml version='1.0' encoding='UTF-8'?>\n";
const NS: &str = "xmlns:pwg='http://www.pwg.org/schemas/2010/12/sm' xmlns:scan='http://schemas.hp.com/imaging/escl/2011/05/03'";

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

pub struct CapsContext<'a> {
    pub make_and_model: &'a str,
    pub uuid: &'a str,
    /// e.g. `http://host:8090/eSCL`
    pub admin_uri: &'a str,
    pub formats: &'a [&'a str],
}

fn input_caps(
    out: &mut String,
    p: &Profile,
    profile_name: usize,
    intents: &[&str],
    formats: &[&str],
) {
    let _ = write!(
        out,
        "<scan:MinWidth>0</scan:MinWidth>\n<scan:MinHeight>0</scan:MinHeight>\n<scan:MaxWidth>{w}</scan:MaxWidth>\n<scan:MaxHeight>{h}</scan:MaxHeight>\n<scan:MaxPhysicalWidth>{w}</scan:MaxPhysicalWidth>\n<scan:MaxPhysicalHeight>{h}</scan:MaxPhysicalHeight>\n<scan:MaxScanRegions>1</scan:MaxScanRegions>\n<scan:SettingProfiles>\n<scan:SettingProfile name='{profile_name}'>\n<scan:ColorModes>\n",
        w = p.max_width,
        h = p.max_height
    );
    for m in p.color_modes() {
        let _ = writeln!(out, "<scan:ColorMode>{}</scan:ColorMode>", m.escl());
    }
    out.push_str("</scan:ColorModes>\n<scan:ColorSpaces>\n<scan:ColorSpace>RGB</scan:ColorSpace>\n</scan:ColorSpaces>\n<scan:SupportedResolutions>\n<scan:DiscreteResolutions>\n");
    for r in &p.resolutions {
        let _ = write!(
            out,
            "<scan:DiscreteResolution>\n<scan:XResolution>{r}</scan:XResolution>\n<scan:YResolution>{r}</scan:YResolution>\n</scan:DiscreteResolution>\n"
        );
    }
    out.push_str(
        "</scan:DiscreteResolutions>\n</scan:SupportedResolutions>\n<scan:DocumentFormats>\n",
    );
    for f in formats {
        let _ = writeln!(out, "<pwg:DocumentFormat>{f}</pwg:DocumentFormat>");
    }
    out.push_str("</scan:DocumentFormats>\n</scan:SettingProfile>\n</scan:SettingProfiles>\n<scan:SupportedIntents>\n");
    for i in intents {
        let _ = writeln!(out, "<scan:SupportedIntent>{i}</scan:SupportedIntent>");
    }
    out.push_str("</scan:SupportedIntents>\n");
}

pub fn capabilities(p: &Profile, ctx: &CapsContext<'_>) -> String {
    let mut out = String::with_capacity(6 * 1024);
    let _ = write!(
        out,
        "{HEAD}<scan:ScannerCapabilities {NS}>\n<pwg:Version>2.0</pwg:Version>\n<pwg:MakeAndModel>{mm}</pwg:MakeAndModel>\n<pwg:SerialNumber>1.0</pwg:SerialNumber>\n<scan:UUID>{uuid}</scan:UUID>\n<scan:AdminURI>{admin}</scan:AdminURI>\n<scan:IconURI>{admin}/ScannerIcon</scan:IconURI>\n",
        mm = escape(ctx.make_and_model),
        uuid = ctx.uuid,
        admin = escape(ctx.admin_uri),
    );
    for (profile, source) in p.sources().enumerate() {
        match source {
            InputSource::Platen => {
                out.push_str("<scan:Platen>\n<scan:PlatenInputCaps>\n");
                input_caps(
                    &mut out,
                    p,
                    profile,
                    &["Preview", "TextAndGraphic", "Photo"],
                    ctx.formats,
                );
                out.push_str("</scan:PlatenInputCaps>\n</scan:Platen>\n");
            }
            InputSource::Feeder => {
                out.push_str("<scan:Adf>\n<scan:AdfSimplexInputCaps>\n");
                input_caps(
                    &mut out,
                    p,
                    profile,
                    &["TextAndGraphic", "Photo"],
                    ctx.formats,
                );
                out.push_str("</scan:AdfSimplexInputCaps>\n<scan:AdfOptions>\n</scan:AdfOptions>\n</scan:Adf>\n");
            }
        }
    }
    out.push_str("</scan:ScannerCapabilities>\n");
    crlf(&out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Pending,
    Processing,
    Completed,
    Canceled,
    Aborted,
}

impl JobState {
    pub fn name(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Processing => "Processing",
            Self::Completed => "Completed",
            Self::Canceled => "Canceled",
            Self::Aborted => "Aborted",
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Canceled | Self::Aborted)
    }
}

pub struct JobView<'a> {
    pub uri: &'a str,
    pub uuid: &'a str,
    pub age_secs: u64,
    pub state: JobState,
    pub images_completed: u32,
    pub reason: &'a str,
}

pub fn status(busy: bool, jobs: &[JobView<'_>], adf_empty: bool) -> String {
    let mut out = String::with_capacity(512 + jobs.len() * 400);
    let _ = write!(
        out,
        "{HEAD}<scan:ScannerStatus {NS}>\n<pwg:Version>2.0</pwg:Version>\n<pwg:State>{}</pwg:State>\n<scan:Jobs>\n",
        if busy { "Processing" } else { "Idle" }
    );
    for j in jobs {
        let _ = write!(
            out,
            "<scan:JobInfo>\n<pwg:JobUri>{uri}</pwg:JobUri>\n<pwg:JobUuid>{uuid}</pwg:JobUuid>\n<scan:Age>{age}</scan:Age>\n<pwg:JobState>{state}</pwg:JobState>\n<pwg:ImagesCompleted>{n}</pwg:ImagesCompleted>\n<pwg:JobStateReasons>\n<pwg:JobStateReason>{reason}</pwg:JobStateReason>\n</pwg:JobStateReasons>\n</scan:JobInfo>\n",
            uri = j.uri,
            uuid = j.uuid,
            age = j.age_secs,
            state = j.state.name(),
            n = j.images_completed,
            reason = j.reason,
        );
    }
    out.push_str("</scan:Jobs>\n");
    if adf_empty {
        out.push_str("<scan:AdfState>ScannerAdfEmpty</scan:AdfState>\n");
    }
    out.push_str("</scan:ScannerStatus>\n");
    crlf(&out)
}

/// AirSane terminates every line with CRLF; so do we.
fn crlf(s: &str) -> String {
    s.replace('\n', "\r\n")
}

/// TXT record for `_uscan._tcp`, as AirSane publishes it.
pub fn uscan_txt(
    p: &Profile,
    ty: &str,
    note: &str,
    uuid: &str,
    rs: &str,
    base_url: &str,
) -> Vec<(String, String)> {
    let is: Vec<&str> = p
        .sources()
        .map(|s| match s {
            InputSource::Platen => "platen",
            InputSource::Feeder => "adf",
        })
        .collect();
    let cs: Vec<&str> = p
        .color_modes()
        .iter()
        .map(|m| match m {
            ColorMode::Gray8 => "grayscale",
            ColorMode::Rgb24 => "color",
        })
        .collect();
    [
        ("txtvers", "1".to_owned()),
        ("vers", "2.0".to_owned()),
        ("pdl", "application/pdf,image/jpeg,image/png".to_owned()),
        ("ty", ty.to_owned()),
        ("note", note.to_owned()),
        ("uuid", uuid.to_owned()),
        ("rs", rs.to_owned()),
        ("cs", cs.join(",")),
        ("is", is.join(",")),
        ("duplex", "F".to_owned()),
        ("adminurl", format!("{base_url}/{rs}")),
        ("representation", format!("{base_url}/{rs}/ScannerIcon")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}
