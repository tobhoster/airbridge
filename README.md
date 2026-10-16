# airbridge

Lets older network scanners and printers show up on Macs, iPhones and iPads with no driver: scanners as **AirScan**, printers as **AirPrint**.

Run it on any Linux machine on the same network. It finds the devices itself, bridges the ones Apple can't use, and leaves devices that already support AirScan or AirPrint alone.

## Install

**Docker (Linux)**

```sh
git clone https://github.com/tobhoster/airbridge && cd airbridge
docker compose up -d
```

**Homebrew (Linux)**

```sh
brew install tobhoster/airbridge/airbridge
brew services start airbridge
```

**Kubernetes**: build and push the image, then `kubectl apply -k deploy/kubernetes` (set your image in `deploy/kubernetes/kustomization.yaml`).

Then open Image Capture (or Preview → Import from Scanner) on a Mac: the scanner is listed under **Shared**. Printers appear in the normal Add Printer dialog. The status page is `http://<host>:8090/`.

## Raspberry Pi

Use a Pi 4 or 5 with 64-bit Raspberry Pi OS. The first `docker compose up` compiles airbridge on the Pi, which takes a while; later starts are instant.

```sh
curl -fsSL https://get.docker.com | sh
git clone https://github.com/tobhoster/airbridge && cd airbridge
sudo docker compose up -d
```

Check it is running:

```sh
curl http://localhost:8090/healthz          # {"status":"ok",...}
curl -s http://localhost:8090/ | grep -o '<td>[^<]*</td>' | head   # bridged scanners
```

On a Mac, `dns-sd -B _uscan._tcp` should list the scanner.

Brother's brscan4 driver only exists for x86. On a Pi, Brother scanners are reached through WSD instead, which sane-airscan finds automatically when the device supports it (the Dell MFP E514dw does). Everything else (SANE backends, CUPS printing) is the same as on x86.

## Configuration

Nothing is required. Optional environment variables:

| Variable | Default | Purpose |
|---|---|---|
| `AIRBRIDGE_NO_SCAN`, `AIRBRIDGE_NO_PRINT` | — | Hosts, names or IPs never to bridge |
| `AIRBRIDGE_LOCATION` | host name | Shown on the Mac |
| `AIRBRIDGE_INTERFACES` | `auto` | Interfaces to serve on |
| `AIRBRIDGE_ALLOW` | LAN subnets | Extra client networks (CIDRs) |
| `AIRBRIDGE_MODEL_MAP` | built in | Rebadged Brother models, `Name=BROTHER-MODEL;…` |
| `AIRBRIDGE_PRINTING` | `auto` | `off` for scanning only |
| `AIRBRIDGE_PORT` | `8090` | HTTP port |
| `AIRBRIDGE_BACKEND` | `sane` | `fake` serves a demo scanner |

Build arguments: `BRSCAN4=0` skips Brother's driver; `PRINTING=0` skips CUPS; `PRINTER_DRIVERS` picks the driver packages.

## Pitfalls

- **mDNS needs host networking.** Bridged container networks and Kubernetes pod networks don't reach the LAN, so use `network_mode: host` or `hostNetwork: true`. Docker Desktop on macOS and Windows can't do this.
- **brscan4 crashes without `/etc/protocols`** (slim images). The image installs `netbase`.
- **Dell and other rebadged Brothers** need the Brother model name. The Dell MFP E514dw → DCP-L2540DW mapping is built in.
- **Printers on DHCP move.** airbridge tracks devices by mDNS name, so a new IP is picked up automatically.

## Licence

MIT or Apache-2.0. Brother's brscan4 is proprietary and is not part of this repository: the Docker build downloads it from Brother and checks its SHA-256. No prebuilt image is published.

Development notes (tests, benchmarks, fuzzing) are in [CONTRIBUTING.md](CONTRIBUTING.md).
