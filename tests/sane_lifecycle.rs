//! SANE initialisation must stay rare.
//!
//! Every sane_exit/sane_init leaked ~37 libusb file descriptors with
//! Debian's stock backends. Re-initialising on every re-probe exhausted the
//! 1024-fd limit and crashed the daemon (exit 139): about every 2 h on the
//! 600 s timer, within minutes while an open kept failing (5 s retries), and
//! sometimes in the middle of a scan, cutting the image short.

mod common;

use std::net::Ipv4Addr;
use std::time::Duration;

use airbridge::backend::fake::FakeDevice;
use airbridge::mdns::fake::FakeNet;
use common::lan::{FakeBrscan, announce_dell, brother4_and_wsd};
use common::{Opts, start, wait_until};

#[tokio::test]
async fn sane_is_reinitialised_only_when_a_registration_changes() {
    let net = FakeNet::new();
    announce_dell(&net, Ipv4Addr::new(192, 168, 68, 54));
    let brscan = FakeBrscan::new("reinit");
    let h = start(Opts {
        net,
        devices: brother4_and_wsd(),
        brscan: Some(brscan.registrar()),
        tweak: Box::new(|c| c.rescan = Duration::from_millis(30)),
        ..Default::default()
    })
    .await;
    wait_until(Duration::from_secs(2), || h.backend.reload_count() == 1).await;

    // Many timer re-probes, then a device that cannot be opened (retried).
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut devices = brother4_and_wsd();
    devices.push(FakeDevice::simple_flatbed(
        "epson2:net:192.168.68.77",
        "Epson",
        "XP-4100",
        "flatbed scanner",
    ));
    h.backend.set_devices(devices);
    h.backend.update(|b| b.open_fails = true);
    h.app.kick();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        h.backend.reload_count(),
        1,
        "timers and retries must not re-initialise SANE"
    );

    // The printer moved: brsaneconfig4 changed, so exactly one re-init.
    h.backend.update(|b| b.open_fails = false);
    announce_dell(&h.net, Ipv4Addr::new(192, 168, 68, 60));
    wait_until(Duration::from_secs(2), || h.backend.reload_count() == 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.backend.reload_count(), 2);
    assert!(brscan.entries()[0].contains("192.168.68.60"));
}
