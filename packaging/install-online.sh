#!/usr/bin/env bash
set -euo pipefail

# NOTE ABOUT INTERACTIVE PROMPTS
# When executed as `curl ... | sudo bash`, stdin is a pipe (not a terminal).
# To still allow user prompts, we read from /dev/tty when available.
TTY_IN="/dev/tty"
HAVE_TTY="false"
if [[ -r "${TTY_IN}" ]]; then
  HAVE_TTY="true"
fi

###############################################################################
# StudioCommand "one-liner" online installer 
#
# Typical usage:
#   curl -fsSL https://raw.githubusercontent.com/ChZeman/StudioCommand/main/packaging/install-online.sh | \
#     sudo bash -s -- --domain studiocommand.example.org --email admin@example.org
#
# Behavior:
#   - If --version is omitted, we fetch the latest GitHub Release tag and ASK you
#     to confirm before installing.
#   - If required args are missing, we prompt interactively (unless --noninteractive).
#   - We detect CPU architecture (x86_64 or aarch64), download the matching tarball,
#     optionally verify checksums, then run packaging/install.sh to do the real install.
#
# Why two scripts?
#   - install.sh: offline/local tarball installer (deterministic inputs; good for support)
#   - install-online.sh: convenience wrapper (discovers version + downloads correct asset)
###############################################################################

OWNER="ChZeman"
REPO="StudioCommand"
PUBLIC_HTTPS_PORT="8443"

WORKDIR="/tmp/studiocommand-installer"
mkdir -p "${WORKDIR}"

VERSION=""           # e.g. v0.1.0
DOMAIN=""            # required
EMAIL=""             # optional; enables Let's Encrypt
NONINTERACTIVE="false"

usage() {
  cat <<EOF
StudioCommand online installer

Usage:
  sudo $0 --domain <host> [--email <email>] [--version <tag>] [--noninteractive]

Examples:
  sudo $0 --domain studiocommand.example.org --email admin@example.org
  sudo $0 --domain studiocommand.example.org --version v0.1.0
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="$2"; shift 2;;
    --domain) DOMAIN="$2"; shift 2;;
    --email) EMAIL="$2"; shift 2;;
    --noninteractive) NONINTERACTIVE="true"; shift 1;;
    -h|--help) usage; exit 0;;
    *) echo "Unknown arg: $1"; usage; exit 1;;
  esac
done

prompt() {
  local var_name="$1"
  local message="$2"
  local default_value="${3:-}"
  if [[ "${NONINTERACTIVE}" == "true" ]]; then
    return 0
  fi
  local prompt_text="${message}"
  if [[ -n "${default_value}" ]]; then
    prompt_text+=" [${default_value}]"
  fi
  prompt_text+=": "
  if [[ "${HAVE_TTY}" == "true" ]]; then
    read -r -p "${prompt_text}" input < "${TTY_IN}" || true
  else
    read -r -p "${prompt_text}" input || true
  fi
  if [[ -z "${input}" ]]; then
    input="${default_value}"
  fi
  printf -v "${var_name}" "%s" "${input}"
}

confirm() {
  local message="$1"
  if [[ "${NONINTERACTIVE}" == "true" ]]; then
    return 0
  fi
  echo
  if [[ "${HAVE_TTY}" == "true" ]]; then
    read -r -p "${message} [y/N]: " ans < "${TTY_IN}" || true
  else
    read -r -p "${message} [y/N]: " ans || true
  fi
  [[ "${ans}" == "y" || "${ans}" == "Y" ]]
}

if [[ "${EUID}" -ne 0 ]]; then
  echo "ERROR: Please run as root (use sudo)."
  exit 1
fi

# Ensure curl exists (this script depends on it for API calls and downloads).
if ! command -v curl >/dev/null 2>&1; then
  echo "[*] Installing curl (required)"
  apt-get update
  apt-get install -y curl
fi

# Map CPU architecture to release tarball naming.
ARCH_RAW="$(uname -m)"
case "${ARCH_RAW}" in
  x86_64|amd64) ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *)
    echo "ERROR: Unsupported architecture '${ARCH_RAW}'. Supported: x86_64, aarch64."
    exit 1
    ;;
esac

get_latest_version() {
  # Use GitHub API rather than scraping HTML. No jq required.
  local api="https://api.github.com/repos/${OWNER}/${REPO}/releases/latest"
  local tag
  tag="$(curl -fsSL "${api}" | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)"
  if [[ -z "${tag}" ]]; then
    echo "ERROR: Could not determine latest release tag from GitHub."
    echo "Create a Release in GitHub (Releases -> Draft new release) and try again."
    exit 1
  fi
  echo "${tag}"
}

normalize_version_no_v() {
  local v="$1"
  echo "${v#v}"
}

if [[ -z "${VERSION}" ]]; then
  echo "[*] No --version provided; discovering latest GitHub Release..."
  VERSION="$(get_latest_version)"
  if ! confirm "Latest release is '${VERSION}'. Install this version?"; then
    echo "Aborted."
    exit 0
  fi
fi
VERSION_NO_V="$(normalize_version_no_v "${VERSION}")"

if [[ -z "${DOMAIN}" ]]; then
  prompt DOMAIN "Enter the hostname for StudioCommand (DNS should point to this server)"
fi
if [[ -z "${DOMAIN}" ]]; then
  echo "ERROR: --domain is required."
  exit 1
fi

if [[ -z "${EMAIL}" && "${NONINTERACTIVE}" != "true" ]]; then
  prompt EMAIL "Email for Let's Encrypt (recommended; blank = self-signed cert)" ""
fi

echo
echo "StudioCommand install plan:"
echo "  - Version:       ${VERSION} (normalized: ${VERSION_NO_V})"
echo "  - Architecture:  ${ARCH}"
echo "  - Public URL:    https://${DOMAIN}:${PUBLIC_HTTPS_PORT}"
if [[ -n "${EMAIL}" ]]; then
  echo "  - TLS:           Let's Encrypt (email: ${EMAIL})"
else
  echo "  - TLS:           Self-signed (browser will warn)"
fi
echo "  - Engine port:   127.0.0.1:3000 (internal)"
echo "  - Nginx port:    0.0.0.0:${PUBLIC_HTTPS_PORT} (public)"

if ! confirm "Proceed? This will install packages and configure systemd + nginx."; then
  echo "Aborted."
  exit 0
fi

TARBALL_NAME="studiocommand-linux-${ARCH}.tar.gz"
TARBALL_PATH="${WORKDIR}/${TARBALL_NAME}"
TARBALL_URL="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${TARBALL_NAME}"

echo
echo "[*] Downloading ${TARBALL_NAME}"
curl -fL --retry 5 --retry-delay 1 -o "${TARBALL_PATH}" "${TARBALL_URL}"

# Optional checksum verification (recommended once you start shipping widely).
SHA_URL="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/sha256sums.txt"
SHA_PATH="${WORKDIR}/sha256sums.txt"
echo "[*] Attempting checksum verification (optional)"
if curl -fsSL -o "${SHA_PATH}" "${SHA_URL}"; then
  echo "    - Found sha256sums.txt, verifying..."
  (cd "${WORKDIR}" && sha256sum -c --ignore-missing "$(basename "${SHA_PATH}")") || {
    echo "ERROR: Checksum verification failed. Refusing to install."
    exit 1
  }
  echo "    - Checksum OK."
else
  echo "    - No sha256sums.txt found; skipping checksum verification."
fi

# Download install.sh from the same tag so behavior matches the chosen version.
INSTALL_SH_URL="https://raw.githubusercontent.com/${OWNER}/${REPO}/${VERSION}/packaging/install.sh"
INSTALL_SH_PATH="${WORKDIR}/install.sh"

echo
echo "[*] Downloading installer: ${INSTALL_SH_URL}"
curl -fsSL -o "${INSTALL_SH_PATH}" "${INSTALL_SH_URL}"
chmod +x "${INSTALL_SH_PATH}"

echo "[*] Running installer"
if [[ -n "${EMAIL}" ]]; then
  "${INSTALL_SH_PATH}" --version "${VERSION_NO_V}" --tar "${TARBALL_PATH}" --domain "${DOMAIN}" --email "${EMAIL}"
else
  "${INSTALL_SH_PATH}" --version "${VERSION_NO_V}" --tar "${TARBALL_PATH}" --domain "${DOMAIN}"
fi

echo
echo "[✓] Done. Open: https://${DOMAIN}:${PUBLIC_HTTPS_PORT}"
