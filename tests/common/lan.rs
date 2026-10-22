//! A simulated LAN for discovery tests: the Dell as it really announces
//! itself, a stand-in for brscan4's `brsaneconfig4`, and the two SANE devices
//! (brother4 and WSD) the same Dell shows up as.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;

use airbridge::backend::DeviceInfo;
use airbridge::backend::fake::FakeDevice;
use airbridge::discovery::brscan::Brsaneconfig4;
use airbridge::mdns::ServiceSpec;
use airbridge::mdns::fake::FakeNet;

pub const DELL_HOST: &str = "DELL6D6F61.local.";

pub fn spec(instance: &str, ty: &str, port: u16, txt: &[(&str, &str)]) -> ServiceSpec {
    ServiceSpec {
        instance: instance.into(),
        ty: ty.into(),
        subtype: None,
        port,
        txt: txt
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect(),
    }
}

/// The Dell MFP E514dw as it announces itself (TXT copied from the real device).
pub fn announce_dell(net: &FakeNet, ip: Ipv4Addr) {
    let addrs = vec![IpAddr::V4(ip)];
    net.announce(
        DELL_HOST,
        spec(
            "Dell MFP E514dw",
            "_scanner._tcp",
            54921,
            &[
                ("mfg", "Dell"),
                ("mdl", "MFP E514dw"),
                ("ty", "Dell MFP E514dw"),
                ("flatbed", "T"),
                ("feeder", "T"),
            ],
        ),
        addrs.clone(),
    );
    net.announce(
        DELL_HOST,
        spec(
            "Dell MFP E514dw",
            "_ipp._tcp",
            631,
            &[
                (
                    "URF",
                    "W8,CP1,IS4-1,MT1-3-4-5-8,OB10,PQ4,RS300-600,V1.3,DM1",
                ),
                ("ty", "Dell MFP E514dw"),
                ("rp", "ipp/print"),
                ("usb_MFG", "Dell"),
                ("usb_MDL", "MFP E514dw"),
            ],
        ),
        addrs.clone(),
    );
    net.announce(
        DELL_HOST,
        spec(
            "Dell MFP E514dw",
            "_pdl-datastream._tcp",
            9100,
            &[
                ("ty", "Dell MFP E514dw"),
                ("usb_MFG", "Dell"),
                ("usb_MDL", "MFP E514dw"),
            ],
        ),
        addrs,
    );
}

/// A stand-in for brscan4's `brsaneconfig4` with its real quirks: exit code
/// 0 always, an existing name is "already registered" and keeps its old IP.
pub struct FakeBrscan {
    dir: PathBuf,
}

impl FakeBrscan {
    pub fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("airbridge-brscan-{tag}-{}", std::process::id()));
        // A leftover from an earlier run may or may not exist.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("models4")).unwrap();
        std::fs::write(
            dir.join("models4/ext_11.ini"),
            "[Support Model]\n0x0328,213,2,\"DCP-L2540DW\",133,4\n",
        )
        .unwrap();
        std::fs::write(dir.join("brsanenetdevice4.cfg"), "").unwrap();
        let script = r#"#!/bin/sh
cfg="$(dirname "$0")/brsanenetdevice4.cfg"
echo "$*" >> "$(dirname "$0")/calls.log"
case "$1" in
  -a) name=${2#name=}; model=${3#model=}; ip=${4#ip=}
      if grep -q "^DEVICE=$name ," "$cfg"; then echo "\"$name\" is already registered."; exit 0; fi
      echo "DEVICE=$name , \"$model\" , 0x4f9:0x328 , IP-ADDRESS=$ip " >> "$cfg" ;;
  -r) awk -v n="$2" 'BEGIN{d=0} index($0, "DEVICE=" n " ,")==1 && !d {d=1; next} {print}' "$cfg" > "$cfg.tmp" && mv "$cfg.tmp" "$cfg" ;;
esac
exit 0
"#;
        let bin = dir.join("brsaneconfig4");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        Self { dir }
    }

    pub fn registrar(&self) -> Arc<Brsaneconfig4> {
        Arc::new(Brsaneconfig4::detect(&self.dir).unwrap())
    }

    pub fn entries(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("brsanenetdevice4.cfg"))
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("DEVICE="))
            .map(str::to_owned)
            .collect()
    }

    pub fn calls(&self) -> String {
        std::fs::read_to_string(self.dir.join("calls.log")).unwrap_or_default()
    }
}

/// The Dell as SANE lists it: through brother4 and through sane-airscan's WSD.
pub fn brother4_and_wsd() -> Vec<FakeDevice> {
    let mut brother = FakeDevice::brother_dcp_l2540dw("brother4:net1;dev0", "DELL6D6F61");
    brother.info.kind = "DCP-L2540DW".into();
    let mut wsd = FakeDevice::brother_dcp_l2540dw("airscan:w1:Dell MFP E514dw", "Dell MFP E514dw");
    wsd.info = DeviceInfo {
        name: "airscan:w1:Dell MFP E514dw".into(),
        vendor: "WSD".into(),
        model: "Dell MFP E514dw".into(),
        kind: "ip=192.168.68.54".into(),
    };
    vec![brother, wsd]
}
