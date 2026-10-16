#!/bin/sh
# Reference for the `encode/jpeg_600dpi_letter_rgb` bench: AirSane's own JPEG
# encoder (libjpeg-turbo, quality 90, as used in AirSane's scanjob.cpp) on the
# same 5100x6600 RGB page. Prints megapixels per second.
#
# Usage: scripts/airsane-jpeg-baseline.sh [airsane-source-dir]
# Needs: g++, libjpeg-turbo headers, git (clones AirSane if no dir is given).
set -eu
src="${1:-}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
if [ -z "$src" ]; then
    git clone -q https://github.com/SimulPiscator/AirSane "$work/AirSane"
    src="$work/AirSane"
fi
cat > "$work/bench.cpp" <<'EOF'
#include "imageformats/jpegencoder.h"
#include <chrono>
#include <cstdio>
#include <sstream>
#include <vector>
int main() {
  const int w = 5100, h = 6600, runs = 5;
  std::vector<unsigned char> row(w * 3);
  double best = 1e9;
  for (int r = 0; r < runs; ++r) {
    std::ostringstream out;
    JpegEncoder enc;
    enc.setQualityPercent(90);
    enc.setWidth(w).setHeight(h).setBitDepth(8).setResolutionDpi(600);
    enc.setColorspace(ImageEncoder::RGB).setDestination(&out);
    auto t0 = std::chrono::steady_clock::now();
    for (int y = 0; y < h; ++y) {
      for (int x = 0; x < w; ++x) {
        int base = (x * 3 + y * 5) % 256;
        row[3 * x] = base; row[3 * x + 1] = (base + 85) % 256; row[3 * x + 2] = (base + 170) % 256;
      }
      enc.writeLine(row.data());
    }
    enc.endDocument();
    double s = std::chrono::duration<double>(std::chrono::steady_clock::now() - t0).count();
    if (s < best) best = s;
  }
  std::printf("airsane jpeg 600dpi letter rgb: %.3f s, %.1f Mpx/s\n", best, w * h / best / 1e6);
}
EOF
g++ -O2 -std=c++17 -I"$src" "$work/bench.cpp" "$src/imageformats/jpegencoder.cpp" "$src/imageformats/imageencoder.cpp" -ljpeg -o "$work/bench"
"$work/bench"
