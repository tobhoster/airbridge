#!/bin/sh
# Install a brscan4 .deb and enable its SANE backend. Shared by the image
# build (BRSCAN4=1) and the entrypoint (BRSCAN4=runtime).
#
# Usage: brscan4-install.sh <brscan4.deb>
set -eu
# brscan4's postinst adds brother4 to dll.conf; its udev step is irrelevant
# for network scanners and may fail in a container.
dpkg -i "$1" >/dev/null 2>&1 || true
grep -qx brother4 /etc/sane.d/dll.conf || echo brother4 >>/etc/sane.d/dll.conf
test -x /opt/brother/scanner/brscan4/brsaneconfig4
