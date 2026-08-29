#!/usr/bin/env bash
#
# Snatch installer.
#
# Builds the workspace, installs both binaries into ~/.local/bin, stages the
# WebExtension, and registers the native messaging host with every Firefox- and
# Chromium-based browser found on the system. Everything is per-user: no sudo,
# no files outside $HOME.

set -euo pipefail

SOURCE_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

readonly HOST_NAME="com.snatch.dl.nmh"
# gallery-dl moved to Codeberg and is not packaged by most distributions, so
# the Linux route is the standalone binary from its releases page.
readonly GALLERY_DL_REPO="https://codeberg.org/mikf/gallery-dl"
readonly GALLERY_DL_API="https://codeberg.org/api/v1/repos/mikf/gallery-dl/releases/latest"
readonly YT_DLP_REPO="https://github.com/yt-dlp/yt-dlp"
readonly YT_DLP_API="https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest"
readonly FIREFOX_EXT_ID="snatch@snatch.dl"
readonly DESKTOP_FILE_NAME="com.snatch.dl.desktop"

readonly BIN_DIR="${HOME}/.local/bin"
readonly DATA_HOME="${XDG_DATA_HOME:-${HOME}/.local/share}"
readonly CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME}/.config}"
readonly DATA_DIR="${DATA_HOME}/snatch-dl"
# Tools Snatch installs for itself live here, not in ~/.local/bin, so
# uninstalling Snatch cannot remove something installed for other uses.
readonly MANAGED_BIN="${DATA_DIR}/bin"
readonly APPS_DIR="${DATA_HOME}/applications"
readonly ICONS_DIR="${DATA_HOME}/icons/hicolor"
# extension/ is the Chromium extension, complete and loadable straight from
# the checkout: its manifest carries both the service worker and the committed
# public key, so "Load unpacked" needs no install step and always resolves to
# the id the native messaging host allows.
readonly CHROMIUM_EXT_DIR="${SOURCE_DIR}/extension"

# Firefox is the one that has to be generated. The two browsers genuinely
# disagree and no single manifest satisfies both: Manifest V3 in Chromium
# accepts only `background.service_worker` and rejects `background.scripts`
# outright, while Firefox has no service-worker background at all and runs an
# event page declared with `background.scripts`.
readonly FIREFOX_EXT_DIR="${SOURCE_DIR}/extension-firefox"

# Where earlier versions of this script staged them.
readonly LEGACY_EXT_DIRS=(
  "${SOURCE_DIR}/extension-chromium"
  "${DATA_DIR}/extension-firefox"
  "${DATA_DIR}/extension-chromium"
)

# Firefox keeps native messaging manifests outside XDG, in ~/.mozilla.
readonly FIREFOX_ALWAYS=(
  "${HOME}/.mozilla/native-messaging-hosts"
)
# Written only when the browser's own profile root already exists.
readonly FIREFOX_OPTIONAL=(
  "${HOME}/.librewolf|${HOME}/.librewolf/native-messaging-hosts"
  "${HOME}/.waterfox|${HOME}/.waterfox/native-messaging-hosts"
  "${HOME}/.floorp|${HOME}/.floorp/native-messaging-hosts"
  "${HOME}/.zen|${HOME}/.zen/native-messaging-hosts"
  "${HOME}/.var/app/org.mozilla.firefox|${HOME}/.var/app/org.mozilla.firefox/.mozilla/native-messaging-hosts"
)
readonly CHROMIUM_ALWAYS=(
  "${CONFIG_HOME}/google-chrome/NativeMessagingHosts"
  "${CONFIG_HOME}/chromium/NativeMessagingHosts"
)
readonly CHROMIUM_OPTIONAL=(
  "${CONFIG_HOME}/google-chrome-beta|${CONFIG_HOME}/google-chrome-beta/NativeMessagingHosts"
  "${CONFIG_HOME}/google-chrome-unstable|${CONFIG_HOME}/google-chrome-unstable/NativeMessagingHosts"
  "${CONFIG_HOME}/microsoft-edge|${CONFIG_HOME}/microsoft-edge/NativeMessagingHosts"
  "${CONFIG_HOME}/BraveSoftware/Brave-Browser|${CONFIG_HOME}/BraveSoftware/Brave-Browser/NativeMessagingHosts"
  "${CONFIG_HOME}/vivaldi|${CONFIG_HOME}/vivaldi/NativeMessagingHosts"
  "${CONFIG_HOME}/opera|${CONFIG_HOME}/opera/NativeMessagingHosts"
)

if [ -t 1 ]; then
  BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; YELLOW=$'\033[33m'
  GREEN=$'\033[32m'; RESET=$'\033[0m'
else
  BOLD=""; DIM=""; RED=""; YELLOW=""; GREEN=""; RESET=""
fi

step() { printf '%s==>%s %s\n' "${BOLD}" "${RESET}" "$*"; }
info() { printf '    %s\n' "$*"; }
note() { printf '    %s%s%s\n' "${DIM}" "$*" "${RESET}"; }
warn() { printf '%swarning:%s %s\n' "${YELLOW}" "${RESET}" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "${RED}" "${RESET}" "$*" >&2; exit 1; }

usage() {
  cat <<USAGE
Usage: ./install.sh [OPTIONS]

This builds Snatch from source and installs it for the current user only.
Most people want the one-liner instead, which installs a released package
system-wide with its dependencies:

  curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/snatch-dl/main/get.sh | sh

  --skip-build         Reuse an existing target/release build.
  --no-deps            Do not install any dependency. By default every missing
                       one is installed: distribution packages via your
                       package manager (you will be prompted for sudo), and
                       the standalone yt-dlp and gallery-dl binaries, verified
                       against their published SHA256 sums.
  --fetch-gallery-dl   Fetch only the gallery-dl standalone binary.
  --fetch-yt-dlp       Fetch only the yt-dlp standalone binary.
  --uninstall          Remove everything this script installed.
  -h, --help           Show this message.

Installs to:
  ${BIN_DIR}/snatch-gui, ${BIN_DIR}/snatch-nmh
  ${DATA_DIR}                 (aria2 session, history, IPC socket)
  ${SOURCE_DIR}/extension-firefox  (generated; Chromium loads ${SOURCE_DIR}/extension)
  ${APPS_DIR}/${DESKTOP_FILE_NAME}
  native messaging manifests for every browser detected
USAGE
}

require() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is required but was not found in PATH"
}

# ---------------------------------------------------------------------------
# Chromium extension identity
# ---------------------------------------------------------------------------

# Pull the committed public key out of the Chromium manifest.
manifest_key() {
  local manifest="${CHROMIUM_EXT_DIR}/manifest.json"
  [ -f "${manifest}" ] || die "${manifest} is missing"
  sed -n 's/^[[:space:]]*"key"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    "${manifest}" | head -n 1
}

# Chromium derives an unpacked extension's id from its public key: the first
# 16 bytes of the SHA-256 of the DER key, hex-encoded, then mapped a-p. The
# key is committed, so every install derives the same id and the native
# messaging manifest can name the exact extension allowed to talk to the host.
derive_chromium_id() {
  local pubkey_b64="$1"
  local digest
  if command -v sha256sum >/dev/null 2>&1; then
    digest="$(printf '%s' "${pubkey_b64}" | base64 -d | sha256sum | cut -d' ' -f1)"
  elif command -v openssl >/dev/null 2>&1; then
    digest="$(printf '%s' "${pubkey_b64}" | base64 -d \
      | openssl dgst -sha256 -hex | awk '{print $NF}')"
  else
    die "neither sha256sum nor openssl is available to derive the extension id"
  fi
  printf '%s' "${digest}" | cut -c1-32 | tr '0-9a-f' 'a-p'
}

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------

uninstall() {
  step "Removing binaries"
  local binary
  # gallery-dl is deliberately left alone: the user may rely on it elsewhere.
  for binary in snatch-gui snatch-nmh; do
    if [ -e "${BIN_DIR}/${binary}" ]; then
      rm -f "${BIN_DIR}/${binary}"
      info "removed ${BIN_DIR}/${binary}"
    fi
  done

  step "Removing native messaging manifests"
  local entry dir
  for entry in "${FIREFOX_ALWAYS[@]}" "${CHROMIUM_ALWAYS[@]}" \
               "${FIREFOX_OPTIONAL[@]}" "${CHROMIUM_OPTIONAL[@]}"; do
    dir="${entry#*|}"
    if [ -e "${dir}/${HOST_NAME}.json" ]; then
      rm -f "${dir}/${HOST_NAME}.json"
      info "removed ${dir}/${HOST_NAME}.json"
    fi
  done

  step "Removing icons"
  local size
  for size in 16 24 32 48 64 96 128 256 512; do
    rm -f "${ICONS_DIR}/${size}x${size}/apps/com.snatch.dl.png"
  done
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f "${ICONS_DIR}" 2>/dev/null || true
  fi
  info "removed icons from ${ICONS_DIR}"

  step "Removing desktop entry"
  if [ -e "${APPS_DIR}/${DESKTOP_FILE_NAME}" ]; then
    rm -f "${APPS_DIR}/${DESKTOP_FILE_NAME}"
    info "removed ${APPS_DIR}/${DESKTOP_FILE_NAME}"
    refresh_desktop_database
  fi

  step "Removing staged extensions"
  local staged
  # Only the generated ones: CHROMIUM_EXT_DIR is the source tree.
  for staged in "${FIREFOX_EXT_DIR}" "${LEGACY_EXT_DIRS[@]}"; do
    if [ -d "${staged}" ]; then
      rm -rf "${staged}"
      info "removed ${staged}"
    fi
  done

  printf '\n%sSnatch has been uninstalled.%s\n' "${GREEN}" "${RESET}"
  note "Kept ${DATA_DIR} (aria2 session, download history and settings)."
  note "Delete it with: rm -rf '${DATA_DIR}'"
  note "Remove the extension from your browser by hand."
}

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------

install_binaries() {
  local release_dir="${SOURCE_DIR}/target/release"
  local binary
  for binary in snatch-gui snatch-nmh; do
    [ -x "${release_dir}/${binary}" ] || die "${release_dir}/${binary} is missing; run without --skip-build"
    install -Dm755 "${release_dir}/${binary}" "${BIN_DIR}/${binary}"
    info "installed ${BIN_DIR}/${binary}"
  done
}

# Install the application icon into the hicolor theme, which is where the
# shell, the dock and the window manager all look it up by name.
install_icons() {
  local source="${SOURCE_DIR}/assets/icons"
  if [ ! -d "${source}" ]; then
    warn "no generated icons in ${source}; run assets/make-icons.sh"
    return
  fi
  local size
  for size in 16 24 32 48 64 96 128 256 512; do
    local file="${source}/com.snatch.dl-${size}.png"
    [ -f "${file}" ] || continue
    install -Dm644 "${file}" \
      "${ICONS_DIR}/${size}x${size}/apps/com.snatch.dl.png"
  done
  info "installed icons into ${ICONS_DIR}"
  # Without a refreshed cache the shell keeps showing the old icon.
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f "${ICONS_DIR}" 2>/dev/null || true
  fi
}

write_desktop_entry() {
  mkdir -p "${APPS_DIR}"
  cat > "${APPS_DIR}/${DESKTOP_FILE_NAME}" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Snatch
GenericName=Download Manager
Comment=Fast, resumable downloads powered by aria2
Exec=${BIN_DIR}/snatch-gui
Icon=com.snatch.dl
Terminal=false
Categories=Network;FileTransfer;
Keywords=download;manager;aria2;torrent;magnet;video;gallery;accelerator;
StartupNotify=true
StartupWMClass=com.snatch.dl
DESKTOP
  info "installed ${APPS_DIR}/${DESKTOP_FILE_NAME}"
  refresh_desktop_database
}

refresh_desktop_database() {
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "${APPS_DIR}" >/dev/null 2>&1 || true
  fi
}

# Build the Firefox manifest from the Chromium one by swapping the members
# that precede "manifest_version" -- exactly the browser-specific ones -- and
# keeping the shared tail verbatim, so the two can never drift apart.
compose_manifest() {
  local target="$1" members="$2"
  local base="${CHROMIUM_EXT_DIR}/manifest.json"
  local shared

  [ -f "${base}" ] || die "${base} is missing"
  shared="$(grep -n '^[[:space:]]*"manifest_version"' "${base}" | head -n 1 | cut -d: -f1)"
  [ -n "${shared}" ] || die "${base} has no \"manifest_version\" line to split on"

  {
    printf '{\n'
    printf '%s\n' "${members}"
    tail -n "+${shared}" "${base}"
  } > "${target}"
  chmod 644 "${target}"
}

stage_extensions() {
  local legacy

  # Clean up locations earlier versions of this script wrote to.
  for legacy in "${LEGACY_EXT_DIRS[@]}"; do
    if [ -d "${legacy}" ]; then
      rm -rf "${legacy}"
      note "removed the previously staged ${legacy}"
    fi
  done

  # Chromium needs nothing staged: extension/ is already the real thing.
  rm -rf "${FIREFOX_EXT_DIR}"
  mkdir -p "${FIREFOX_EXT_DIR}"
  install -Dm644 "${CHROMIUM_EXT_DIR}/background.js" "${FIREFOX_EXT_DIR}/background.js"
  # The button that appears on a video. Without it Firefox loads an
  # extension that captures downloads but draws nothing on a page.
  install -Dm644 "${CHROMIUM_EXT_DIR}/content.js" "${FIREFOX_EXT_DIR}/content.js"

  # Without these the browser shows a letter placeholder instead of the icon.
  local icon
  for icon in 16 24 32 48 128; do
    install -Dm644 "${CHROMIUM_EXT_DIR}/icons/icon-${icon}.png" \
      "${FIREFOX_EXT_DIR}/icons/icon-${icon}.png"
  done

  # Firefox: event-page background, identified by its gecko id, and without
  # the Chromium key -- Firefox rejects a manifest that carries one.
  compose_manifest "${FIREFOX_EXT_DIR}/manifest.json" "$(cat <<MEMBERS
  "background": {
    "scripts": [
      "background.js"
    ]
  },
  "browser_specific_settings": {
    "gecko": {
      "id": "${FIREFOX_EXT_ID}",
      "strict_min_version": "115.0"
    }
  },
MEMBERS
  )"

  verify_manifests
  info "staged ${FIREFOX_EXT_DIR}"
}

# Catch the mistakes that only surface as a browser load error.
verify_manifests() {
  local chromium="${CHROMIUM_EXT_DIR}/manifest.json"
  local firefox="${FIREFOX_EXT_DIR}/manifest.json"

  grep -q '"key"' "${chromium}" || die "the Chromium manifest lost its public key"
  grep -q '"service_worker"' "${chromium}" \
    || die "the Chromium manifest has no service_worker background"
  grep -q '"scripts"' "${firefox}" || die "the Firefox manifest has no background scripts"
  grep -q '"gecko"' "${firefox}" || die "the Firefox manifest has no gecko id"

  if grep -q '"scripts"' "${chromium}"; then
    die "the Chromium manifest uses background.scripts, which Manifest V3 rejects"
  fi
  if grep -q '"service_worker"' "${firefox}"; then
    die "the Firefox manifest uses a service worker, which Firefox does not support"
  fi
  if grep -q '"key"' "${firefox}"; then
    die "the Firefox manifest must not carry the Chromium signing key"
  fi

  # Best effort: a JSON parser is not a hard dependency of this script.
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import json,sys; [json.load(open(p)) for p in sys.argv[1:]]' \
      "${chromium}" "${firefox}" || die "generated a malformed manifest"
  fi
}

write_host_manifest() {
  local dir="$1" flavour="$2" allow="$3"
  mkdir -p "${dir}"
  cat > "${dir}/${HOST_NAME}.json" <<MANIFEST
{
  "name": "${HOST_NAME}",
  "description": "Snatch download manager native messaging host",
  "path": "${BIN_DIR}/snatch-nmh",
  "type": "stdio",
  "${flavour}": [
    "${allow}"
  ]
}
MANIFEST
  chmod 644 "${dir}/${HOST_NAME}.json"
  info "registered ${dir}/${HOST_NAME}.json"
}

register_hosts() {
  local chromium_id="$1"
  local entry probe dir

  for dir in "${FIREFOX_ALWAYS[@]}"; do
    write_host_manifest "${dir}" "allowed_extensions" "${FIREFOX_EXT_ID}"
  done
  for entry in "${FIREFOX_OPTIONAL[@]}"; do
    probe="${entry%%|*}"
    dir="${entry#*|}"
    if [ -d "${probe}" ]; then
      write_host_manifest "${dir}" "allowed_extensions" "${FIREFOX_EXT_ID}"
    fi
  done

  for dir in "${CHROMIUM_ALWAYS[@]}"; do
    write_host_manifest "${dir}" "allowed_origins" "chrome-extension://${chromium_id}/"
  done
  for entry in "${CHROMIUM_OPTIONAL[@]}"; do
    probe="${entry%%|*}"
    dir="${entry#*|}"
    if [ -d "${probe}" ]; then
      write_host_manifest "${dir}" "allowed_origins" "chrome-extension://${chromium_id}/"
    fi
  done
}

check_aria2() {
  if command -v aria2c >/dev/null 2>&1; then
    info "found $(aria2c --version 2>/dev/null | head -n1)"
    return
  fi
  warn "aria2c was not found; Snatch cannot download anything without it."
  info "  install it with: $(package_hint aria2)"
}

# ffmpeg and yt-dlp are packaged everywhere; gallery-dl generally is not.
check_optional_tools() {
  # Snatch prepends its managed directory to PATH at runtime; do the same here
  # so a self-installed tool is reported as present.
  PATH="${MANAGED_BIN}:${PATH}"

  if command -v ffmpeg >/dev/null 2>&1 && command -v ffprobe >/dev/null 2>&1; then
    info "found $(ffmpeg -version 2>/dev/null | head -n1 | cut -d" " -f1-3)"
  else
    warn "ffmpeg/ffprobe not found; post-processing will be unavailable."
    info "  install it with: $(package_hint ffmpeg)"
  fi

  if command -v yt-dlp >/dev/null 2>&1; then
    info "found yt-dlp $(yt-dlp --version 2>/dev/null)"
  else
    warn "yt-dlp not found; site video extraction will be unavailable."
    info "  install it with: $(package_hint yt-dlp)"
    info "  or fetch the standalone binary: ./install.sh --fetch-yt-dlp"
  fi

  if command -v gallery-dl >/dev/null 2>&1; then
    info "found gallery-dl $(gallery-dl --version 2>/dev/null)"
  else
    warn "gallery-dl not found; the Media Scraper will be unavailable."
    info "  most distributions do not package it. Fetch the standalone binary:"
    info "    ./install.sh --fetch-gallery-dl"
    info "  or download it by hand from ${GALLERY_DL_REPO}/releases"
  fi
}

# The install command for the running distribution.
package_hint() {
  local package="$1"
  if command -v dnf >/dev/null 2>&1; then
    printf "sudo dnf install %s" "${package}"
  elif command -v apt-get >/dev/null 2>&1; then
    printf "sudo apt install %s" "${package}"
  elif command -v pacman >/dev/null 2>&1; then
    printf "sudo pacman -S %s" "${package}"
  elif command -v zypper >/dev/null 2>&1; then
    printf "sudo zypper install %s" "${package}"
  else
    printf "your package manager's '%s' package" "${package}"
  fi
}

# Download a standalone binary and check it against its published SHA256.
#
# Explicitly opt-in: it pulls tens of megabytes over the network, which is not
# something an installer should do behind the user's back. Verified before it
# is made executable, and renamed into place only after it verifies.
fetch_standalone() {
  local tool="$1" api="$2" asset="$3" sums="$4" repo="$5"
  require curl
  require python3
  require sha256sum

  step "Fetching ${tool}"

  local metadata tag
  metadata="$(curl -fsSL "${api}")" || die "could not reach the ${tool} release API"
  tag="$(printf "%s" "${metadata}" | python3 -c \
    "import json,sys; print(json.load(sys.stdin).get('tag_name',''))")" \
    || die "could not parse the ${tool} release metadata"
  [ -n "${tag}" ] || die "the ${tool} release metadata carried no tag"
  info "latest release: ${tag}"

  local base="${repo}/releases/download/${tag}"
  local scratch
  scratch="$(mktemp -d)" || die "could not create a temporary directory"
  # shellcheck disable=SC2064
  trap "rm -rf '${scratch}'" RETURN

  # Large assets over HTTP/2 intermittently fail with PROTOCOL_ERROR against
  # GitHub's CDN, so retry and fall back to HTTP/1.1 before giving up.
  download_asset "${base}/${asset}" "${scratch}/${asset}" \
    || die "could not download ${asset}"
  download_asset "${base}/${sums}" "${scratch}/sums" \
    || die "could not download ${sums}"

  local expected actual
  # Match the asset name exactly: yt-dlp's manifest lists every platform, and
  # gallery-dl marks binary mode with a leading asterisk.
  expected="$(awk -v want="${asset}" \
    '{ name = $2; sub(/^\*/, "", name); if (name == want) { print $1; exit } }' \
    "${scratch}/sums")"
  [ -n "${expected}" ] || die "${sums} has no entry for ${asset}"
  actual="$(sha256sum "${scratch}/${asset}" | awk '{ print $1 }')"
  if [ "${expected}" != "${actual}" ]; then
    die "checksum mismatch for ${asset} (expected ${expected}, got ${actual})"
  fi
  info "sha256 verified: ${actual}"

  install -Dm755 "${scratch}/${asset}" "${MANAGED_BIN}/${tool}"
  info "installed ${MANAGED_BIN}/${tool}"
  info "version: $("${MANAGED_BIN}/${tool}" --version 2>/dev/null || echo unknown)"
}

# Fetch one URL, retrying and then downgrading to HTTP/1.1.
download_asset() {
  local url="$1" target="$2"
  if curl -fsSL --retry 3 --retry-delay 2 --retry-all-errors -o "${target}" "${url}"; then
    return 0
  fi
  note "retrying ${url##*/} over HTTP/1.1"
  curl -fsSL --http1.1 --retry 3 --retry-delay 2 --retry-all-errors -o "${target}" "${url}"
}

fetch_gallery_dl() {
  fetch_standalone gallery-dl "${GALLERY_DL_API}" gallery-dl.bin SHA256SUMS \
    "${GALLERY_DL_REPO}"
}

fetch_yt_dlp() {
  # Only these two architectures have published Linux builds.
  local asset
  case "$(uname -m)" in
    x86_64)  asset="yt-dlp_linux" ;;
    aarch64) asset="yt-dlp_linux_aarch64" ;;
    *)       warn "yt-dlp publishes no Linux binary for $(uname -m); skipping"; return 0 ;;
  esac
  fetch_standalone yt-dlp "${YT_DLP_API}" "${asset}" SHA2-256SUMS "${YT_DLP_REPO}"
}

# Install everything missing: distribution packages first, then standalones.
install_dependencies() {
  local wanted=()
  command -v aria2c >/dev/null 2>&1 || wanted+=("aria2")
  command -v ffmpeg >/dev/null 2>&1 || wanted+=("ffmpeg")

  if [ ${#wanted[@]} -gt 0 ]; then
    step "Installing distribution packages: ${wanted[*]}"
    if [ "$(id -u)" -eq 0 ]; then
      warn "running as root; Snatch itself should be installed as your own user"
    fi
    # These need root, so the user runs the command and sees the prompt.
    local command
    command="$(package_command "${wanted[@]}")"
    if [ -z "${command}" ]; then
      warn "unknown package manager; install these yourself: ${wanted[*]}"
    else
      info "running: ${command}"
      # shellcheck disable=SC2086
      eval "${command}" || warn "package installation failed; install manually: ${wanted[*]}"
    fi
  else
    step "Distribution packages already present"
  fi

  command -v yt-dlp >/dev/null 2>&1 || fetch_yt_dlp
  command -v gallery-dl >/dev/null 2>&1 || fetch_gallery_dl
}

# The install command for the running distribution, or empty if unknown.
package_command() {
  if command -v dnf >/dev/null 2>&1; then
    # Fedora keeps ffmpeg in RPM Fusion, so a bare install would not match it.
    printf 'sudo dnf install -y %s' "$*"
  elif command -v apt-get >/dev/null 2>&1; then
    printf 'sudo apt-get update && sudo apt-get install -y %s' "$*"
  elif command -v pacman >/dev/null 2>&1; then
    printf 'sudo pacman -S --needed --noconfirm %s' "$*"
  elif command -v zypper >/dev/null 2>&1; then
    printf 'sudo zypper install -y %s' "$*"
  elif command -v apk >/dev/null 2>&1; then
    printf 'sudo apk add %s' "$*"
  else
    printf ''
  fi
}

check_path() {
  case ":${PATH}:" in
    *":${BIN_DIR}:"*) ;;
    *)
      warn "${BIN_DIR} is not in your PATH."
      info "add this to ~/.bashrc or ~/.zshrc:"
      info "  export PATH=\"\${HOME}/.local/bin:\${PATH}\""
      ;;
  esac
}

main() {
  local skip_build=0
  local want_gallery_dl=0
  local want_yt_dlp=0
  # Dependencies come as standard; --no-deps opts out.
  local want_deps=1

  while [ $# -gt 0 ]; do
    case "$1" in
      --skip-build)       skip_build=1 ;;
      --no-deps)          want_deps=0 ;;
      # Accepted for anyone with the old flag in a script.
      --with-deps)        want_deps=1 ;;
      --fetch-gallery-dl) want_gallery_dl=1 ;;
      --fetch-yt-dlp)     want_yt_dlp=1 ;;
      --uninstall)        uninstall; exit 0 ;;
      -h|--help)          usage; exit 0 ;;
      *)                  usage >&2; die "unknown option: $1" ;;
    esac
    shift
  done

  require base64
  require awk
  require install

  [ -f "${SOURCE_DIR}/Cargo.toml" ] || die "run this script from the Snatch checkout"

  if [ "${skip_build}" -eq 0 ]; then
    require cargo
    step "Building the workspace (release)"
    cargo build --release --manifest-path "${SOURCE_DIR}/Cargo.toml"
  else
    step "Skipping the build as requested"
  fi

  step "Installing binaries"
  install_binaries

  step "Creating ${DATA_DIR}"
  mkdir -p "${DATA_DIR}"
  chmod 700 "${DATA_DIR}"
  info "ready"

  step "Resolving the Chromium extension identity"
  local pubkey_b64 chromium_id
  pubkey_b64="$(manifest_key)"
  [ -n "${pubkey_b64}" ] \
    || die "${CHROMIUM_EXT_DIR}/manifest.json has no \"key\" member"
  chromium_id="$(derive_chromium_id "${pubkey_b64}")"
  [ ${#chromium_id} -eq 32 ] || die "derived an invalid Chromium extension id: ${chromium_id}"
  info "extension id: ${chromium_id}"

  step "Staging the Firefox WebExtension"
  stage_extensions

  step "Registering the native messaging host"
  register_hosts "${chromium_id}"

  step "Installing the icon and desktop entry"
  install_icons
  write_desktop_entry

  if [ "${want_deps}" -eq 1 ]; then
    install_dependencies
  fi
  if [ "${want_gallery_dl}" -eq 1 ]; then
    fetch_gallery_dl
  fi
  if [ "${want_yt_dlp}" -eq 1 ]; then
    fetch_yt_dlp
  fi

  step "Checking dependencies"
  check_aria2
  check_optional_tools
  check_path

  cat <<SUMMARY

${GREEN}${BOLD}Snatch is installed.${RESET}

${BOLD}Load the extension${RESET}

  Firefox   about:debugging#/runtime/this-firefox
            "Load Temporary Add-on..." and pick
            ${FIREFOX_EXT_DIR}/manifest.json
            ${DIM}(temporary add-ons are cleared on restart; sign the extension
            or use Developer Edition with xpinstall.signatures.required=false
            to keep it permanently)${RESET}

  Chromium  chrome://extensions
            Enable "Developer mode", then "Load unpacked" and pick the
            ${BOLD}folder${RESET}
            ${CHROMIUM_EXT_DIR}
            ${DIM}Pick the folder itself, not a file inside it. The id is
            pinned to ${chromium_id} by the committed key, so the
            native host keeps working across reloads.${RESET}

${BOLD}Try it${RESET}

  ${BIN_DIR}/snatch-gui
  printf '{"url":"https://speed.hetzner.de/100MB.bin"}\\n' \\
    | nc -U "${DATA_DIR}/snatch.sock"

SUMMARY
}

main "$@"
