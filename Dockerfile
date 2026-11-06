# syntax=docker/dockerfile:1
#
# airbridge: AirScan + AirPrint for printers and scanners that lack them.
#
# Build args:
#   BRSCAN4=1|runtime|0  Brother brscan4 network scanner driver (amd64 only).
#                        1: downloaded from download.brother.com at build time
#                        and installed in the image. runtime: the image holds
#                        no Brother code; the entrypoint downloads brscan4 on
#                        first start into /var/lib/airbridge (the published
#                        image). Both check BRSCAN4_SHA256; brscan4 is never
#                        stored in the repository or redistributed by us.
#   PRINTING=1|0         CUPS + printer drivers (0 = scanner bridge only).
#   PRINTER_DRIVERS      Debian driver packages to include.

ARG BRSCAN4=1
ARG BRSCAN4_URL=https://download.brother.com/welcome/dlf105200/brscan4-0.4.11-1.amd64.deb
ARG BRSCAN4_SHA256=027b73648722ac8c8eb1a9c419d284a6562cc763feac9740a2b75a683b092972

FROM rust:1-slim-trixie AS build
WORKDIR /src
# nasm: libjpeg-turbo SIMD in the JPEG encoder (mozjpeg-sys).
RUN apt-get update && apt-get install -y --no-install-recommends nasm && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY benches benches
RUN cargo build --release --locked --bin airbridge \
 && strip target/release/airbridge

FROM debian:trixie-slim AS brscan4
ARG BRSCAN4
ARG BRSCAN4_URL
ARG BRSCAN4_SHA256
RUN mkdir /out \
 && if [ "$BRSCAN4" = 1 ] && [ "$(dpkg --print-architecture)" = amd64 ]; then \
      apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
      && curl -fsSL --proto '=https' -o /out/brscan4.deb "$BRSCAN4_URL" \
      && echo "$BRSCAN4_SHA256  /out/brscan4.deb" | sha256sum -c - ; \
    else echo "brscan4 skipped"; fi

FROM debian:trixie-slim
ARG BRSCAN4
ARG BRSCAN4_URL
ARG BRSCAN4_SHA256
ARG PRINTING=1
ARG PRINTER_DRIVERS="printer-driver-brlaser printer-driver-hpcups printer-driver-splix printer-driver-foo2zjs"
# netbase: /etc/protocols. brscan4 calls getprotobyname("tcp") and segfaults
# without it (debian-slim does not ship the file). curl: the runtime brscan4
# download (amd64 only; brscan4 does not exist for other architectures).
RUN apt-get update \
 && apt-get install -y --no-install-recommends libsane1 sane-airscan netbase \
    $(if [ "$PRINTING" = 1 ]; then echo cups-daemon cups-client cups-filters $PRINTER_DRIVERS; fi) \
    $(if [ "$BRSCAN4" = runtime ] && [ "$(dpkg --print-architecture)" = amd64 ]; then echo curl ca-certificates; fi) \
 && rm -rf /var/lib/apt/lists/* /usr/share/doc /usr/share/man
COPY docker/entrypoint.sh docker/brscan4-install.sh /usr/local/lib/airbridge/
COPY --from=brscan4 /out/ /tmp/brscan4/
RUN if [ -f /tmp/brscan4/brscan4.deb ]; then /usr/local/lib/airbridge/brscan4-install.sh /tmp/brscan4/brscan4.deb; fi \
 && rm -rf /tmp/brscan4
ENV BRSCAN4_MODE=$BRSCAN4 \
    BRSCAN4_URL=$BRSCAN4_URL \
    BRSCAN4_SHA256=$BRSCAN4_SHA256 \
    BRSCAN4_CACHE=/var/lib/airbridge
# CUPS: listen on every address (no startup-address pinning), print from the
# local networks, admin from localhost only, no avahi (airbridge announces).
RUN if [ -f /etc/cups/cupsd.conf ]; then \
      sed -i -e 's/^Listen localhost:631/Port 631/' \
             -e 's/^Browsing .*/Browsing No/' \
             -e '/^<Location \/>/,/^<\/Location>/ s/^\(\s*\)Order allow,deny/\1Order allow,deny\n\1Allow @LOCAL/' \
             /etc/cups/cupsd.conf \
      && printf 'ServerAlias *\nDefaultShared Yes\nBrowseLocalProtocols none\n' >> /etc/cups/cupsd.conf \
      && sed -i -e 's/^#\?ErrorLog .*/ErrorLog stderr/' -e 's/^#\?AccessLog .*/AccessLog none/' /etc/cups/cups-files.conf \
      && grep -q '^ErrorLog' /etc/cups/cups-files.conf || echo 'ErrorLog stderr' >> /etc/cups/cups-files.conf; \
    fi
COPY --from=build /src/target/release/airbridge /usr/local/bin/airbridge
EXPOSE 8090 631
HEALTHCHECK --interval=30s --timeout=10s --start-period=20s CMD ["/usr/local/bin/airbridge", "healthcheck"]
ENTRYPOINT ["/usr/local/lib/airbridge/entrypoint.sh"]
