#!/usr/bin/env bash
set -euo pipefail

VERSION=""
TARBALL=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="$2"; shift 2;;
    --tar) TARBALL="$2"; shift 2;;
    *) echo "Unknown arg: $1"; exit 1;;
  esac
done

if [[ -z "${VERSION}" || -z "${TARBALL}" ]]; then
  echo "Usage: sudo ./install.sh --version <x.y.z> --tar <tarball>"
  exit 1
fi

ROOT="/opt/studiocommand"
REL="${ROOT}/releases/${VERSION}"
SHARED="${ROOT}/shared"
CURRENT="${ROOT}/current"

echo "[*] Installing StudioCommand ${VERSION} to ${ROOT}"

mkdir -p "${ROOT}/releases" "${SHARED}/config" "${SHARED}/data" "${SHARED}/updates" "${SHARED}/logs"

if ! id -u studiocommand >/dev/null 2>&1; then
  useradd --system --home /opt/studiocommand --shell /usr/sbin/nologin studiocommand
fi

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT

echo "[*] Extracting ${TARBALL}"
tar -xzf "${TARBALL}" -C "${TMP}"

if [[ ! -d "${TMP}/studiocommand" ]]; then
  echo "Tarball missing studiocommand/ root folder"
  exit 1
fi

rm -rf "${REL}.pending"
mkdir -p "${REL}.pending"
cp -a "${TMP}/studiocommand/." "${REL}.pending/"

rm -rf "${REL}"
mv "${REL}.pending" "${REL}"

ln -sfn "${REL}" "${CURRENT}"

UNIT_SRC="${REL}/systemd/studiocommand.service"
UNIT_DST="/etc/systemd/system/studiocommand.service"
if [[ -f "${UNIT_SRC}" ]]; then
  cp -f "${UNIT_SRC}" "${UNIT_DST}"
else
  echo "Missing systemd unit at ${UNIT_SRC}"
  exit 1
fi

chown -R studiocommand:studiocommand "${ROOT}"

systemctl daemon-reload
systemctl enable --now studiocommand.service

echo "[✓] Installed. Engine listens on 127.0.0.1:3000 (use reverse proxy for LAN/Internet)."
