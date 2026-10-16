//! ScanSettings is the only structured input a LAN client controls. Parsing
//! and validating any byte string must return Ok or a TicketError, never
//! panic, and must stay bounded in time and memory.
#![no_main]

use airbridge::backend::fake::FakeDevice;
use airbridge::escl::ticket;
use airbridge::scanner::Profile;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let dev = FakeDevice::brother_dcp_l2540dw("fuzz:dev", "Fuzz");
    let Some(profile) = Profile::from_options(dev.info, &dev.options) else { return };
    if let Ok(raw) = ticket::parse(data) {
        let _ = ticket::validate(&raw, &profile);
    }
});
