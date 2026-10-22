//! Discovery: what gets bridged, what never does, and how identity survives
//! address changes and name clashes.

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use airbridge::backend::fake::FakeDevice;
use airbridge::discovery::PrinterTarget;
use airbridge::discovery::cups::{QueueInfo, SpoolError, Spooler};
use airbridge::mdns::fake::FakeNet;
use common::lan::{FakeBrscan, announce_dell, brother4_and_wsd, spec};
use common::{Opts, start, wait_until};
use hyper::StatusCode;
use parking_lot::Mutex;

#[tokio::test]
async fn dell_is_bridged_once_via_brother4_with_rebadge_mapping() {
    let net = FakeNet::new();
    announce_dell(&net, Ipv4Addr::new(192, 168, 68, 54));
    let brscan = FakeBrscan::new("once");
    let h = start(Opts {
        net,
        devices: brother4_and_wsd(),
        brscan: Some(brscan.registrar()),
        ..Default::default()
    })
    .await;
    wait_until(Duration::from_secs(2), || {
        h.net.published("_uscan._tcp").len() == 1
    })
    .await;
    let scanners = h.app.scanners();
    assert_eq!(scanners.len(), 1, "brother4 and WSD are the same scanner");
    assert_eq!(
        scanners[0].profile.device.name, "brother4:net1;dev0",
        "the vendor backend wins over WSD"
    );
    assert_eq!(scanners[0].name, "Dell MFP E514dw");
    assert_eq!(
        brscan.entries(),
        vec!["DEVICE=DELL6D6F61 , \"DCP-L2540DW\" , 0x4f9:0x328 , IP-ADDRESS=192.168.68.54 "]
    );
    let (name, host, _) = &h.net.published("_uscan._tcp")[0];
    assert_eq!(name, "Dell MFP E514dw._uscan._tcp.local.");
    assert_eq!(host, "airbridge-testhost.local.");
}

#[tokio::test]
async fn printer_ip_change_re_registers_without_duplicates() {
    let net = FakeNet::new();
    announce_dell(&net, Ipv4Addr::new(192, 168, 68, 69));
    let brscan = FakeBrscan::new("ipchange");
    let h = start(Opts {
        net,
        devices: brother4_and_wsd(),
        brscan: Some(brscan.registrar()),
        ..Default::default()
    })
    .await;
    wait_until(Duration::from_secs(2), || brscan.entries().len() == 1).await;
    assert!(brscan.entries()[0].ends_with("IP-ADDRESS=192.168.68.69 "));

    // DHCP moved the printer (this happened for real: .69 -> .54).
    announce_dell(&h.net, Ipv4Addr::new(192, 168, 68, 54));
    wait_until(Duration::from_secs(2), || {
        brscan.entries().iter().any(|e| e.contains("192.168.68.54"))
    })
    .await;
    assert_eq!(
        brscan.entries(),
        vec!["DEVICE=DELL6D6F61 , \"DCP-L2540DW\" , 0x4f9:0x328 , IP-ADDRESS=192.168.68.54 "]
    );
    assert!(
        brscan.calls().contains("-r DELL6D6F61"),
        "removed before re-adding: {}",
        brscan.calls()
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.net.published("_uscan._tcp").len(), 1);
    assert_eq!(h.app.scanners().len(), 1);
    assert_eq!(h.get("/eSCL/ScannerStatus").await.status, StatusCode::OK);
}

#[tokio::test]
async fn devices_that_already_speak_airscan_or_airprint_are_never_bridged() {
    let net = FakeNet::new();
    let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 68, 80));
    net.announce(
        "HPENVY.local.",
        spec(
            "HP ENVY 6000",
            "_uscan._tcp",
            8080,
            &[("ty", "HP ENVY 6000"), ("rs", "eSCL")],
        ),
        vec![ip],
    );
    net.announce(
        "HPENVY.local.",
        spec(
            "HP ENVY 6000",
            "_scanner._tcp",
            54921,
            &[("mfg", "HP"), ("mdl", "ENVY 6000")],
        ),
        vec![ip],
    );
    net.announce(
        "HPENVY.local.",
        spec(
            "HP ENVY 6000",
            "_ipp._tcp",
            631,
            &[("URF", "W8,SRGB24,RS300"), ("rp", "ipp/print")],
        ),
        vec![ip],
    );
    net.announce(
        "HPENVY.local.",
        spec(
            "HP ENVY 6000",
            "_pdl-datastream._tcp",
            9100,
            &[("usb_MFG", "HP")],
        ),
        vec![ip],
    );
    let mut wsd = FakeDevice::brother_dcp_l2540dw("airscan:w2:HP ENVY 6000", "HP ENVY 6000");
    wsd.info.kind = "ip=192.168.68.80".into();
    let mut escl = FakeDevice::brother_dcp_l2540dw("airscan:e0:HP ENVY 6000", "HP ENVY 6000");
    escl.info.kind = "ip=192.168.68.80".into();
    let spooler = Arc::new(FakeSpooler::default());
    let brscan = FakeBrscan::new("native");
    let h = start(Opts {
        net,
        devices: vec![wsd, escl],
        brscan: Some(brscan.registrar()),
        spooler: Some(spooler.clone()),
        wait_for_scanners: 0,
        ..Default::default()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.app.kick();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.app.scanners().is_empty());
    assert_eq!(brscan.calls(), "", "no brscan4 registration");
    let ours: Vec<_> = h
        .net
        .published("_uscan._tcp")
        .into_iter()
        .filter(|(_, host, _)| host.starts_with("airbridge"))
        .collect();
    assert!(ours.is_empty(), "{ours:?}");
    assert!(
        spooler.created.lock().is_empty(),
        "native AirPrint printer re-shared"
    );
}

#[tokio::test]
async fn dell_printing_stays_native_while_an_old_laser_gets_airprint() {
    let net = FakeNet::new();
    announce_dell(&net, Ipv4Addr::new(192, 168, 68, 54));
    net.announce(
        "HPLJ2055.local.",
        spec(
            "HP LaserJet P2055dn",
            "_pdl-datastream._tcp",
            9100,
            &[
                ("ty", "HP LaserJet P2055dn"),
                ("usb_MFG", "Hewlett-Packard"),
                ("usb_MDL", "HP LaserJet P2055dn"),
                ("usb_CMD", "PJL,PCL,POSTSCRIPT"),
            ],
        ),
        vec![IpAddr::V4(Ipv4Addr::new(192, 168, 68, 90))],
    );
    let spooler = Arc::new(FakeSpooler::default());
    let h = start(Opts {
        net,
        spooler: Some(spooler.clone()),
        ..Default::default()
    })
    .await;
    wait_until(Duration::from_secs(2), || {
        !h.net
            .published("_ipp._tcp")
            .iter()
            .all(|(_, host, _)| !host.starts_with("airbridge"))
    })
    .await;
    let created = spooler.created.lock().clone();
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(created[0].0.uri, "socket://192.168.68.90:9100");
    assert_eq!(
        created[0].0.device_id,
        "MFG:Hewlett-Packard;MDL:HP LaserJet P2055dn;CMD:PJL,PCL,POSTSCRIPT;"
    );
    assert!(
        !created.iter().any(|(t, _)| t.display.contains("Dell")),
        "the Dell is never re-shared"
    );

    let ours: Vec<_> = h
        .net
        .published("_ipp._tcp")
        .into_iter()
        .filter(|(_, host, _)| host.starts_with("airbridge"))
        .collect();
    assert_eq!(ours.len(), 1);
    let s = h.net.spec(&ours[0].0).unwrap();
    assert_eq!(s.subtype.as_deref(), Some("_universal"));
    assert_eq!(s.port, 631);
    let txt: std::collections::BTreeMap<_, _> = s.txt.into_iter().collect();
    assert_eq!(txt["rp"], "printers/HP_LaserJet_P2055dn");
    assert!(txt["URF"].contains("W8") && txt["URF"].contains("RS300"));
    assert!(txt["pdl"].contains("image/urf") && txt["pdl"].contains("application/pdf"));
}

#[tokio::test]
async fn no_print_exclusion_is_honoured() {
    let net = FakeNet::new();
    net.announce(
        "OLDLASER.local.",
        spec(
            "Old Laser",
            "_pdl-datastream._tcp",
            9100,
            &[("ty", "Old Laser")],
        ),
        vec![IpAddr::V4(Ipv4Addr::new(192, 168, 68, 91))],
    );
    let spooler = Arc::new(FakeSpooler::default());
    let h = start(Opts {
        net,
        spooler: Some(spooler.clone()),
        tweak: Box::new(|c| c.no_print = vec!["oldlaser".into()]),
        ..Default::default()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.app.kick();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(spooler.created.lock().is_empty());
}

#[tokio::test]
async fn mdns_name_conflict_renames_instead_of_crashing() {
    let net = FakeNet::new();
    net.announce(
        "elsewhere.local.",
        spec(
            "Brother DellE514dw",
            "_uscan._tcp",
            8090,
            &[("ty", "someone else")],
        ),
        vec![IpAddr::V4(Ipv4Addr::new(192, 168, 68, 7))],
    );
    let h = start(Opts {
        net,
        ..Default::default()
    })
    .await;
    wait_until(Duration::from_secs(2), || !h.app.announced().is_empty()).await;
    let announced = h.app.announced();
    assert_eq!(announced[0].1, "Brother DellE514dw (2)._uscan._tcp.local.");
    assert_eq!(h.get("/healthz").await.status, StatusCode::OK);
    // Withdrawing uses the new name and leaves the other host's service alone.
    h.app.shutdown().await;
    let left = h.net.published("_uscan._tcp");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].1, "elsewhere.local.");
}

#[derive(Default)]
struct FakeSpooler {
    created: Mutex<Vec<(PrinterTarget, String)>>,
}

impl Spooler for FakeSpooler {
    fn driver_for(&self, device_id: &str) -> Result<Option<String>, SpoolError> {
        Ok((!device_id.starts_with("MFG:;"))
            .then(|| "drv:///hpcups.drv/hp-laserjet_p2055dn.ppd".to_owned()))
    }

    fn ensure_queue(
        &self,
        target: &PrinterTarget,
        driver: &str,
        _location: &str,
    ) -> Result<(), SpoolError> {
        let mut c = self.created.lock();
        if !c.iter().any(|(t, _)| t.queue == target.queue) {
            c.push((target.clone(), driver.to_owned()));
        }
        Ok(())
    }

    fn queues(&self) -> Result<Vec<QueueInfo>, SpoolError> {
        Ok(self
            .created
            .lock()
            .iter()
            .map(|(t, _)| QueueInfo {
                name: t.queue.clone(),
                info: t.display.clone(),
                make_model: format!("{} pcl3, hpcups 3.22.10", t.display),
                uri: t.uri.clone(),
                color: false,
                duplex: true,
            })
            .collect())
    }
}
