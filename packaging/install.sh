#!/usr/bin/env bash
set -euo pipefail

# StudioCommand installer (v0) + Nginx reverse proxy on :8443
#
# Installs to:
#   /opt/studiocommand/{releases/<version>,shared,current}
# Configures:
#   - systemd service: studiocommand (engine on 127.0.0.1:3000)
#   - nginx site on HTTPS :8443
#   - Let's Encrypt if --email provided; otherwise self-signed cert
#
# Usage:
#   sudo ./packaging/install.sh --version 0.1.0 --tar /path/to/studiocommand-linux-x86_64.tar.gz \
#       --domain studiocommand.yourstation.org --email you@example.org
#
# Self-signed fallback:
#   sudo ./packaging/install.sh --version 0.1.0 --tar /path/to/studiocommand-linux-x86_64.tar.gz \
#       --domain studiocommand.yourstation.org

VERSION=""
TARBALL=""
DOMAIN=""
EMAIL=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="$2"; shift 2;;
    --tar)     TARBALL="$2"; shift 2;;
    --domain)  DOMAIN="$2"; shift 2;;
    --email)   EMAIL="$2"; shift 2;;
    *) echo "Unknown arg: $1"; exit 1;;
  esac
done

if [[ -z "${VERSION}" || -z "${TARBALL}" ]]; then
  echo "Usage: sudo $0 --version <x.y.z> --tar <tarball> --domain <host> [--email <email>]"
  exit 1
fi

if [[ -z "${DOMAIN}" ]]; then
  echo "ERROR: --domain is required (used for nginx server_name and certificate)."
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
cp -f "${UNIT_SRC}" "${UNIT_DST}"

chown -R studiocommand:studiocommand "${ROOT}"

systemctl daemon-reload
systemctl enable --now studiocommand.service

echo "[*] Installing nginx"
apt-get update
apt-get install -y nginx

mkdir -p /var/www/letsencrypt

SITE_AVAIL="/etc/nginx/sites-available/studiocommand"
SITE_ENABLED="/etc/nginx/sites-enabled/studiocommand"

# Prefer template from release (packaged under nginx/), else use repo template
if [[ -f "${REL}/nginx/nginx-studiocommand.conf" ]]; then
  cp -f "${REL}/nginx/nginx-studiocommand.conf" "${SITE_AVAIL}"
else
  cp -f "$(dirname "$0")/nginx-studiocommand.conf" "${SITE_AVAIL}"
fi

sed -i "s/studiocommand\.example\.org/${DOMAIN}/g" "${SITE_AVAIL}"

CERT_DIR="/etc/letsencrypt/live/${DOMAIN}"
FULLCHAIN="${CERT_DIR}/fullchain.pem"
PRIVKEY="${CERT_DIR}/privkey.pem"

if [[ -n "${EMAIL}" ]]; then
  echo "[*] Installing certbot and requesting Let's Encrypt cert for ${DOMAIN}"
  apt-get install -y certbot python3-certbot-nginx
  ln -sf "${SITE_AVAIL}" "${SITE_ENABLED}"
  rm -f /etc/nginx/sites-enabled/default || true
  nginx -t
  systemctl reload nginx

  certbot --nginx -d "${DOMAIN}" --non-interactive --agree-tos -m "${EMAIL}" --redirect || EMAIL=""
fi

if [[ -z "${EMAIL}" ]]; then
  echo "[*] Creating self-signed certificate for ${DOMAIN}"
  apt-get install -y openssl
  mkdir -p "${CERT_DIR}"
  openssl req -x509 -nodes -newkey rsa:2048 -days 3650 \
    -keyout "${PRIVKEY}" -out "${FULLCHAIN}" -subj "/CN=${DOMAIN}" >/dev/null 2>&1
fi

ln -sf "${SITE_AVAIL}" "${SITE_ENABLED}"
rm -f /etc/nginx/sites-enabled/default || true

nginx -t
systemctl enable --now nginx
systemctl reload nginx

echo
echo "[✓] Installed."
echo "    Engine (internal):  http://127.0.0.1:3000"
echo "    Nginx (public):     https://${DOMAIN}:8443"
echo
echo "Firewall reminder: allow 8443/tcp (and 80/tcp if using redirect/ACME). Keep 3000 closed externally."
