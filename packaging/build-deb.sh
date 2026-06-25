#!/usr/bin/env bash
set -euo pipefail

# Build a .deb for the architecture of the current host (amd64 or arm64).
# Adapted from the hyperiondb house packaging pattern (see pg_replica).

PKG="renew"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="$(dpkg --print-architecture)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "${ROOT}/Cargo.toml" | head -1)"
OUT_DIR="${ROOT}/dist"

cd "${ROOT}"
cargo build --release -p renew
BIN="${ROOT}/target/release/renew"
[ -x "${BIN}" ] || { echo "binary not found at ${BIN}" >&2; exit 1; }

PKGROOT="$(mktemp -d)"
install -D -m 0755 "${BIN}" "${PKGROOT}/usr/bin/renew"
install -D -m 0644 "${ROOT}/packaging/renew.service" \
  "${PKGROOT}/lib/systemd/system/renew.service"
install -D -m 0644 "${ROOT}/renew.example.json" \
  "${PKGROOT}/usr/share/doc/renew/renew.example.json"
install -D -m 0644 "${ROOT}/README.md" "${PKGROOT}/usr/share/doc/renew/README.md"
install -D -m 0644 "${ROOT}/docs/configuration.md" \
  "${PKGROOT}/usr/share/doc/renew/configuration.md"
install -D -m 0644 "${ROOT}/docs/deployment.md" \
  "${PKGROOT}/usr/share/doc/renew/deployment.md"
install -D -m 0644 "${ROOT}/LICENCE" "${PKGROOT}/usr/share/doc/renew/copyright"

mkdir -p "${PKGROOT}/DEBIAN"

cat > "${PKGROOT}/DEBIAN/control" <<EOF
Package: ${PKG}
Version: ${VERSION}
Architecture: ${ARCH}
Maintainer: Tadas Talaikis <info@nordlet.com>
Section: admin
Priority: optional
Depends: git
Recommends: docker.io, ca-certificates
Homepage: https://github.com/hyperiondb/renew
Description: Self-healing deployment daemon
 Reconciles running docker compose stacks against a git repository on a
 cron-like schedule, driven by a single JSON config file. Pulls new images and
 recreates only drifted containers, gated by a peer-health quorum. Runs under
 systemd.
EOF

# postinst: register the unit with systemd, but do not enable/start — the daemon
# needs /etc/renew/renew.json first.
cat > "${PKGROOT}/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload >/dev/null 2>&1 || true
fi
if [ "$1" = "configure" ]; then
  cat <<'MSG'
renew installed. To activate (only systemd is supported):
  sudo mkdir -p /etc/renew
  sudo cp /usr/share/doc/renew/renew.example.json /etc/renew/renew.json
  sudo "${EDITOR:-nano}" /etc/renew/renew.json
  sudo systemctl enable --now renew
Requires git and docker (with the compose plugin) on PATH.
MSG
fi
EOF
chmod 0755 "${PKGROOT}/DEBIAN/postinst"

cat > "${PKGROOT}/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ -d /run/systemd/system ] && [ "$1" = "remove" ]; then
  systemctl stop renew.service >/dev/null 2>&1 || true
  systemctl disable renew.service >/dev/null 2>&1 || true
fi
EOF
chmod 0755 "${PKGROOT}/DEBIAN/prerm"

cat > "${PKGROOT}/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload >/dev/null 2>&1 || true
fi
EOF
chmod 0755 "${PKGROOT}/DEBIAN/postrm"

mkdir -p "${OUT_DIR}"
DEB="${OUT_DIR}/${PKG}_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "${PKGROOT}" "${DEB}"
rm -rf "${PKGROOT}"
echo "built ${DEB}"
