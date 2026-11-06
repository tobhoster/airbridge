# airbridge

Lets older network scanners and printers show up on Macs, iPhones and iPads with no driver: scanners as **AirScan**, printers as **AirPrint**.

Run it on any Linux machine on the same network. It finds the devices itself, bridges the ones Apple can't use, and leaves devices that already support AirScan or AirPrint alone.

## Install

### Supported: Docker Compose on x86_64 Linux

This is the one fully supported setup. You need a Linux machine on the same network as the devices, with Docker installed.

```sh
git clone https://github.com/tobhoster/airbridge && cd airbridge
docker compose up -d
```

What this does for you:

- **Builds the image locally.** Brother scanners only work through Brother's brscan4 driver, which is proprietary and x86-only. The build downloads it from Brother and checks its SHA-256. It also installs `netbase`, `sane-airscan` and CUPS. No prebuilt image is published, because brscan4 can't be redistributed.
- **Uses host networking** (`network_mode: host`). mDNS doesn't work without it.

Check it is running:

```sh
curl http://localhost:8090/healthz          # {"status":"ok",...}
curl -s http://localhost:8090/ | grep -o '<td>[^<]*</td>' | head   # bridged scanners
```

Then open Image Capture (or Preview → Import from Scanner) on a Mac: the scanner is listed under **Shared**. Printers appear in the normal Add Printer dialog. On a Mac, `dns-sd -B _uscan._tcp` should list the scanner. The status page is `http://<host>:8090/`.

**Without Compose**, the same image runs with `docker run`:

```sh
git clone https://github.com/tobhoster/airbridge && cd airbridge
docker build -t airbridge:local .
docker run -d --name airbridge --network host --restart unless-stopped \
  -e AIRBRIDGE_INTERFACES=eth0,eth1 \
  airbridge:local
```

`AIRBRIDGE_INTERFACES` is optional. The example is for a host with two network interfaces; leave the line out to use `auto`.

### Best effort

**Raspberry Pi / ARM.** The same Compose setup, but **without brscan4**: Brother only ships it for x86. Brother scanners are then reached only through WSD, and only if the device supports WSD scanning (the Dell MFP E514dw does; many older Brothers don't). Printing works the same as on x86. Use a Pi 4 or 5 with 64-bit Raspberry Pi OS. The first `docker compose up` compiles airbridge on the Pi, which takes a while.

```sh
curl -fsSL https://get.docker.com | sh
git clone https://github.com/tobhoster/airbridge && cd airbridge
sudo docker compose up -d
```

**Homebrew on Linux (advanced).** Convenient, but you install SANE, `sane-airscan`, brscan4 and CUPS yourself, so problems are harder to diagnose.

```sh
brew install tobhoster/airbridge/airbridge
brew services start airbridge
```

**Kubernetes (community, advanced).** Build and push the image, set it in `deploy/kubernetes/kustomization.yaml`, then `kubectl apply -k deploy/kubernetes`. The manifests use `hostNetwork: true`; don't remove it.

### Not supported

- **Docker Desktop on macOS or Windows**: it has no real host networking, so mDNS can't reach the LAN.
- **Bridged container networks** (Docker's default network, Kubernetes pod networks without `hostNetwork`): same reason.

## Configuration

Nothing is required. Optional environment variables:

| Variable | Default | Purpose |
|---|---|---|
| `AIRBRIDGE_NO_SCAN`, `AIRBRIDGE_NO_PRINT` | — | Hosts, names or IPs never to bridge |
| `AIRBRIDGE_LOCATION` | host name | Shown on the Mac |
| `AIRBRIDGE_INTERFACES` | `auto` | Comma-separated interfaces to find devices on and serve on. `auto` finds devices on every LAN interface and serves on the default-route one(s) |
| `AIRBRIDGE_ALLOW` | LAN subnets | Extra client networks (CIDRs) |
| `AIRBRIDGE_MODEL_MAP` | built in | Rebadged Brother models, `Name=BROTHER-MODEL;…` |
| `AIRBRIDGE_PRINTING` | `auto` | `off` for scanning only |
| `AIRBRIDGE_PORT` | `8090` | HTTP port |
| `AIRBRIDGE_BACKEND` | `sane` | `fake` serves a demo scanner |
| `RUST_LOG` | `info` | `info,airbridge=debug` logs every mDNS service seen; add `mdns_sd=debug` to trace the mDNS library |

Build arguments: `BRSCAN4=0` skips Brother's driver; `PRINTING=0` skips CUPS; `PRINTER_DRIVERS` picks the driver packages.

## Pitfalls

- **brscan4 crashes without `/etc/protocols`** (slim images). The image installs `netbase`.
- **Dell and other rebadged Brothers** need the Brother model name. The Dell MFP E514dw → DCP-L2540DW mapping is built in.
- **Brother models that brscan4 doesn't know are skipped.** airbridge checks the model against brscan4's own list (402 models in 0.4.11) and logs `brscan4 does not support <model>`. Check yours with `docker compose exec airbridge sh -c "grep -h '\"MFC-9340CDW\"' /opt/brother/scanner/brscan4/models4/*.ini"`; no output means brscan4 can't drive it.
- **Printers on DHCP move.** airbridge tracks devices by mDNS name, so a new IP is picked up automatically.

## Troubleshooting

If nothing shows up, check these in order:

1. **Status page.** `http://<host>:8090/` lists every device airbridge saw but didn't bridge, and why.
2. **Log.** airbridge logs each discovery decision. If nothing has been found 30 s after startup, it warns and names the interfaces it is browsing. To see every mDNS service it hears, set `RUST_LOG=info,airbridge=debug` and restart.
3. **Is the device announcing?** On the host, `avahi-browse -rt _scanner._tcp` lists Brother scanners and `avahi-browse -rt _ipp._tcp` lists printers. If the device isn't listed there, airbridge can't see it either.
4. **Interfaces.** The log line `mDNS: browsing on these interfaces` shows where airbridge looks for devices. The `listening` lines show where Macs can reach it. If either is missing an interface you need, list your interfaces explicitly, e.g. `AIRBRIDGE_INTERFACES=eth0,eth1`.

Still stuck? [Open an issue](https://github.com/tobhoster/airbridge/issues/new/choose); questions are welcome. The form asks for the outputs above.

## Licence

MIT or Apache-2.0. Brother's brscan4 is proprietary and is not part of this repository: the Docker build downloads it from Brother and checks its SHA-256. No prebuilt image is published.

Development notes (tests, benchmarks, fuzzing) are in [CONTRIBUTING.md](CONTRIBUTING.md).
