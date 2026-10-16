# airbridge

AirScan (eSCL) and AirPrint for network printers and scanners that don't speak them, so Macs, iPhones and iPads can use them with no driver.

airbridge runs on a Linux box or Kubernetes node on the same LAN as the printer. It finds devices over mDNS, bridges whatever lacks native Apple support, and leaves alone anything that already has it:

- **Scanners** are driven through SANE (the stock backends, sane-airscan's WSD support, and Brother's brscan4). airbridge serves them as AirScan. Image Capture, Preview and the iOS Files app then see them under *Shared*.
- **Printers** are driven through CUPS with the usual open drivers (brlaser, hpcups, splix, foo2zjs). airbridge announces the queues as AirPrint printers.
- **Already compatible devices are skipped.** A scanner that advertises `_uscan._tcp` is never re-exported, and a printer whose `_ipp._tcp` record carries `URF` is never re-shared.

One Rust daemon covers the eSCL server, the mDNS responder and browser, discovery and health. No avahi, no dbus and no AirSane are involved.

## Quick start

### Docker Compose (Linux host)

```sh
git clone https://github.com/tobhoster/airbridge && cd airbridge
docker compose up -d
```

That is all, in the common case. The image is built locally, with brscan4 downloaded from Brother during the build. The container uses the host network because mDNS and the scanners' own protocols need the LAN. Docker Desktop on macOS and Windows cannot do this, so run it on a Linux machine, a NAS or a Raspberry Pi class box. The status page is at `http://<host>:8090/`.

### Kubernetes

```sh
docker build -t registry.example.com/airbridge:0.1.0 . && docker push registry.example.com/airbridge:0.1.0
cd deploy/kubernetes && kustomize edit set image airbridge=registry.example.com/airbridge:0.1.0
kubectl apply -n <namespace> -k .
```

The Deployment is a single `hostNetwork` replica with `strategy: Recreate`, because two copies would announce every scanner twice. Its liveness probe checks `/healthz`. The pod may run on any node that sits on the printers' LAN.

## Configuration

Everything is optional; set environment variables only to change a default.

| Variable | Default | Meaning |
|---|---|---|
| `AIRBRIDGE_PORT` | `8090` | eSCL/HTTP port |
| `AIRBRIDGE_INTERFACES` | `auto` | Interfaces to serve and announce on. `auto` means those holding a default route, never docker/veth/cni/flannel/tailscale/wg |
| `AIRBRIDGE_ALLOW` | none | Extra client networks (CIDRs). The subnets of the served interfaces are always allowed |
| `AIRBRIDGE_NO_SCAN` / `AIRBRIDGE_NO_PRINT` | none | Comma-separated host names, device names or IPs never to bridge for that role |
| `AIRBRIDGE_MODEL_MAP` | built-in | Rebadged Brother models: `Advertised Name=BROTHER-MODEL;…` (the Dell MFP E514dw → DCP-L2540DW mapping is built in) |
| `AIRBRIDGE_LOCATION` | host name | Location shown in Image Capture and the print dialog |
| `AIRBRIDGE_HOSTNAME` | `airbridge-<host>` | mDNS host label; differs from the machine's own so it never collides with avahi |
| `AIRBRIDGE_JOB_TIMEOUT` | `120` | Seconds without data before a scan is aborted |
| `AIRBRIDGE_GAMMA` | `0.555555` | Gamma applied to scans; compensates macOS's 1.8 display gamma, as AirSane does |
| `AIRBRIDGE_PRINTING` | `auto` | `off` turns the bridge into a scanner-only service |
| `AIRBRIDGE_RESCAN_INTERVAL` | `600` | Seconds between SANE re-probes; mDNS changes trigger one at once |
| `AIRBRIDGE_BACKEND` | `sane` | `fake` serves a simulated scanner, for trying airbridge without hardware |

Build arguments:

| Argument | Default | Meaning |
|---|---|---|
| `BRSCAN4` | `1` | Brother brscan4 (amd64 only). The package is downloaded from download.brother.com at build time and verified against a pinned SHA-256 |
| `PRINTING` | `1` | `0` omits CUPS and drivers |
| `PRINTER_DRIVERS` | brlaser hpcups splix foo2zjs | Debian driver packages; add e.g. `printer-driver-gutenprint` |

Print queues added by hand through the CUPS web interface (`http://localhost:631`, admin from localhost only) are announced too. Mount `/etc/cups` on a volume to keep them across restarts.

## How it stays up

Every rule below comes from a failure observed while bridging a Dell MFP E514dw with AirSane on a home network:

- **It never binds to the startup addresses only.** Listeners follow interface and address changes at runtime: a DHCP renewal, Wi-Fi roaming, a NIC that comes up after the service. AirSane binds to the addresses present when it starts, so a late Wi-Fi address made the scanner discoverable but unreachable.
- **Access rules are rebuilt on every address change.** AirSane's `local on <iface>` rule is evaluated once, at startup.
- **Devices are tracked by mDNS host name, never by DHCP address.** When the printer's IP changes, the driver registration is updated and no second device appears. brscan4 stored `ip=192.168.68.69`, the printer later moved to `.54`, and AirSane silently served a scanner that no longer existed.
- **Scans are never interrupted by a reconfiguration**, which waits until the scanner is idle. A page that delivers no data for `AIRBRIDGE_JOB_TIMEOUT` is cancelled. If the driver ignores the cancel, `/healthz` turns unhealthy and the orchestrator restarts the container instead of leaving a dead scanner announced.
- **A crashed component (cupsd, the mDNS responder) makes the process exit**, so the restart policy brings everything back cleanly.

## Pitfalls

- **brscan4 segfaults in slim containers** whenever it opens a network scanner, because it calls `getprotobyname("tcp")` and `/etc/protocols` is missing. Install `netbase`. The crash goes away under gdb and strace, which makes it look like a race.
- **`brsaneconfig4` exits 0 even when it did nothing.** Re-adding an existing name prints "already registered" and keeps the old IP, so changes must be remove-then-add, verified against `brsanenetdevice4.cfg`.
- **Brother's "Gray[Error Diffusion]" mode is dithered 1-bit.** Real gray is "True Gray". airbridge picks it, and expands 1-bit data from any backend into 8-bit gray.
- **Rebadged printers need the Brother model name.** The Dell MFP E514dw is a DCP-L2540DW; brscan4 does not know Dell names.
- **Fedora 44's RPM refuses Brother's brscan4 RPM** ("no digest"), because the package has no checksums at all. On the host, `rpm -i --define '_pkgverify_level none'` installs it. The container uses the Debian package.
- **A Brother scanner shows up twice in SANE**: brother4, plus sane-airscan's WSD discovery. airbridge matches them by IP and announces one, preferring the vendor backend.
- **mDNS needs the host network.** Bridged container networks and Kubernetes pod networks do not carry multicast to the LAN.
- **Running next to avahi:** airbridge announces under its own host name (`airbridge-<host>.local`), because claiming the machine's name would conflict with avahi's.

## Development

```sh
cargo test                       # hermetic suite, a few seconds
AIRBRIDGE_ALLOW_NETWORK=1 cargo test --test hardware -- --ignored hardware   # real LAN, real scanner
cargo bench --bench perf         # criterion baselines
scripts/perf-smoke.sh            # startup, idle RSS, idle CPU of the release binary
cd fuzz && cargo +nightly fuzz run scan_settings corpus/scan_settings seeds/scan_settings
```

**The default suite cannot touch the network.** `.cargo/config.toml` sets `scripts/netns-runner.sh` as the cargo runner, so every test binary runs in a fresh network namespace that contains only loopback. The suite uses a fake SANE backend, an in-process mDNS network and fake interface addresses. CI fails if it takes longer than 15 s. Set `AIRBRIDGE_ALLOW_NETWORK=1` for `cargo run` and for the hardware suite.

Measured on the development machine (Intel Core i7-12700T, Fedora 44):

| Metric | airbridge | AirSane |
|---|---|---|
| JPEG encode, 600 dpi US Letter RGB (5100×6600) | 116 ms (290 Mpx/s) | 128–131 ms (257–263 Mpx/s), `scripts/airsane-jpeg-baseline.sh` |
| ScannerCapabilities over HTTP | p50 40 µs, p99 0.12–0.28 ms | — |
| Startup to first mDNS announcement | 325 ms | — |
| Idle RSS / idle CPU | 6.3 MiB / 2 ticks per 10 s | — |

JPEG is libjpeg-turbo, the same encoder AirSane uses, fed one scanline at a time, so a page is never held in memory.

## Licences

airbridge is dual-licensed under MIT or Apache-2.0. Brother's brscan4 is proprietary. This repository does not contain it, and no prebuilt image is published: the Dockerfile downloads it from Brother when you build. The golden-file tests compare against output captured from [AirSane](https://github.com/SimulPiscator/AirSane) (GPL-3.0); no AirSane code is included.
