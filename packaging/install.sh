#!/usr/bin/env bash
set -e

# Adds the renew apt repository, then you `apt-get install renew`.
# Adapted from the hyperiondb house packaging pattern (see pg_replica).

BASE_URL="${RENEW_REPO_URL:-https://hyperiondb.github.io/renew}"
DIST="${RENEW_DIST:-stable}"
KEYRING="/usr/share/keyrings/renew.gpg"
LIST="/etc/apt/sources.list.d/renew.list"

if [ "$(id -u)" -ne 0 ]; then
  echo "this installer must run as root — pipe it to: sudo bash" >&2
  exit 1
fi

if ! command -v curl >/dev/null 2>&1; then
  apt-get update
  apt-get install -y curl ca-certificates
fi

curl -fsSL "${BASE_URL}/renew.gpg" -o "${KEYRING}"
ARCH="$(dpkg --print-architecture)"
echo "deb [arch=${ARCH} signed-by=${KEYRING}] ${BASE_URL} ${DIST} main" > "${LIST}"
apt-get update

echo
echo "renew apt repository added. Install with:"
echo "  apt-get install -y renew"
