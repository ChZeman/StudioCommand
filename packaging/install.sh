#!/usr/bin/env bash
set -euo pipefail

# StudioCommand installer (v0) + Nginx reverse proxy on :9443
#
# Installs to:
#   /opt/studiocommand/{releases/<version>,shared,current}
# Configures:
#   - systemd service: studiocommand (engine on 127.0.0.1:3000)
#   - nginx site on HTTPS :9443
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
mkdir -p "${ROOT}/releases" "${SHARED}/config" "${SHARED}/data" "${SHARED}/updates" "${SHARED}/logs" "${SHARED}/carts"

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
  # Enable a minimal config so certbot's nginx plugin can perform its
  # challenge/redirect edits. This is a transitional step; after cert
  # issuance we install our own authoritative config in conf.d/.
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

  # ---------------------------------------------------------------------------
  # Nginx configuration strategy (IMPORTANT)
  # ---------------------------------------------------------------------------
  # Historically we used /etc/nginx/sites-enabled/ for StudioCommand.
  # On some hosts, distro defaults or prior manual edits can leave multiple
  # "server_name studio.lakesradio.org" blocks on the same listen ports.
  # Nginx will then warn "conflicting server name ... ignored" and may serve the
  # wrong site (often the default "Welcome to nginx!" page).
  #
  # To make upgrades deterministic, we install ONE authoritative file in
  # /etc/nginx/conf.d/ and mark it as default_server on both 80 and 9443.
  # This avoids sites-enabled drift entirely.

  CONF_D="/etc/nginx/conf.d/00-studiocommand.conf"

  echo "[*] Writing authoritative nginx config: ${CONF_D}"
  cat >"${CONF_D}" <<EOF
map \$http_upgrade \$connection_upgrade {
  default upgrade;
  ''      close;
}

server {
  listen 80 default_server;
  server_name ${DOMAIN};

  location ^~ /.well-known/acme-challenge/ {
    root /var/www/letsencrypt;
  }

  location / {
    return 301 https://\$host:9443\$request_uri;
  }
}

server {
  listen 9443 ssl http2 default_server;
  server_name ${DOMAIN};

  ssl_certificate     ${FULLCHAIN};
  ssl_certificate_key ${PRIVKEY};

  # UI (static)
  root ${ROOT}/current/web;
  index index.html;

  # UI entrypoints (multi-page)
# We serve a landing page at '/', and two distinct app entrypoints:
#   /remote -> /remote.html
#   /admin  -> /admin.html
#
# These exact-match locations MUST appear before the catch-all SPA-style
# fallback, otherwise Nginx will happily serve /index.html for /remote and
# /admin, making every page look like the landing page.
location = /remote  { try_files /remote.html =404; }
location = /remote/ { try_files /remote.html =404; }
location = /admin   { try_files /admin.html  =404; }
location = /admin/  { try_files /admin.html  =404; }

# UI routing (SPA-style fallback)
# - Static assets are served normally
# - Unknown paths fall back to the landing page (index.html)
location / {
  try_files \$uri \$uri/ /index.html;
}

  # API proxy
  location /api/ {
    proxy_pass http://127.0.0.1:3000;
    proxy_http_version 1.1;
    proxy_set_header Host \$host;
    proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto \$scheme;
  }

  # WebSocket / WebRTC signaling
  location /ws {
    proxy_pass http://127.0.0.1:3000;
    proxy_http_version 1.1;
    proxy_set_header Upgrade \$http_upgrade;
    proxy_set_header Connection \$connection_upgrade;
    proxy_set_header Host \$host;
  }
}
EOF

  # Remove common sources of "Welcome to nginx!" on upgrades.
  rm -f /etc/nginx/sites-enabled/default || true
  rm -f /etc/nginx/sites-enabled/studiocommand.conf || true
  rm -f /etc/nginx/sites-enabled/studiocommand || true

  nginx -t
  systemctl enable --now nginx
  systemctl reload nginx

# Ensure the newly installed engine is running.
# (If the service was already running, enable --now does not restart it.)
echo "[*] Restarting studiocommand"
systemctl restart studiocommand.service || true

echo
echo "[✓] Installed."
echo "    Engine (internal):  http://127.0.0.1:3000"
echo "    Nginx (public, serves UI + proxies API):     https://${DOMAIN}:9443"
echo
echo "Firewall reminder: allow 9443/tcp (and 80/tcp if using redirect/ACME). Keep 3000 closed externally."
