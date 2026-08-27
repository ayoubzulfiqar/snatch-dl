#!/bin/sh
# Snatch — one-line installer.
#
#   curl -fsSL https://snatch.ayoubzulfiqar.com/get.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/snatch-dl/main/get.sh | sh
#
# Downloads the newest release and installs it with the system package
# manager, which is what pulls in aria2, GTK and the rest. Nothing is built
# from source and no toolchain is needed.
#
# POSIX sh on purpose: this runs before anything is installed, on whatever
# shell the machine happens to have.
#
# Options, passed after `-s --` when piping:
#   curl -fsSL … | sh -s -- --no-extras     skip ffmpeg, yt-dlp, gallery-dl
#   curl -fsSL … | sh -s -- --uninstall     remove Snatch again
#   curl -fsSL … | sh -s -- --version 2.6.9 install one specific release

set -eu

REPO="ayoubzulfiqar/snatch-dl"
API="https://api.github.com/repos/${REPO}/releases"
WANT_EXTRAS=1
WANT_VERSION=""
DO_UNINSTALL=0

# Colour only when a terminal is watching. Piped into `sh`, stdout is the
# terminal even though stdin is not, so this still works.
if [ -t 1 ]; then
  BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m'); RESET=$(printf '\033[0m')
  GREEN=$(printf '\033[32m'); YELLOW=$(printf '\033[33m'); RED=$(printf '\033[31m')
else
  BOLD=''; DIM=''; RESET=''; GREEN=''; YELLOW=''; RED=''
fi

say()  { printf '%s==>%s %s\n' "${BOLD}" "${RESET}" "$*"; }
info() { printf '    %s\n' "$*"; }
warn() { printf '%swarning:%s %s\n' "${YELLOW}" "${RESET}" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "${RED}" "${RESET}" "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --no-extras) WANT_EXTRAS=0 ;;
    --uninstall) DO_UNINSTALL=1 ;;
    --version)   shift; WANT_VERSION="${1:-}" ;;
    -h|--help)
      sed -n '2,26p' "$0" 2>/dev/null || printf 'See %s\n' "https://github.com/${REPO}"
      exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
  shift
done

# ---------------------------------------------------------------------------
# Environment
# ---------------------------------------------------------------------------

need() { command -v "$1" >/dev/null 2>&1; }

if need curl; then
  fetch()      { curl -fsSL "$1"; }
  fetch_file() { curl -fsSL -o "$2" "$1"; }
elif need wget; then
  fetch()      { wget -qO- "$1"; }
  fetch_file() { wget -qO "$2" "$1"; }
else
  die "neither curl nor wget is installed, so nothing can be downloaded"
fi

# Everything below writes outside $HOME, so it needs root. Re-using an
# existing sudo session means the password is asked for once at most.
if [ "$(id -u)" -eq 0 ]; then
  SUDO=""
elif need sudo; then
  SUDO="sudo"
elif need doas; then
  SUDO="doas"
else
  die "this installs system-wide and needs sudo, which is not available"
fi

case "$(uname -m)" in
  x86_64|amd64) ARCH="x86_64" ;;
  *) die "Snatch publishes x86_64 builds only; $(uname -m) is not one of them" ;;
esac

# Which package manager, which asset. Ordered so a derivative matches its
# base: Mint reports ID=linuxmint with ID_LIKE=ubuntu.
detect_family() {
  if need apt-get && [ -f /etc/debian_version ]; then echo debian; return; fi
  if need dnf;     then echo fedora;  return; fi
  if need zypper;  then echo suse;    return; fi
  if need pacman;  then echo arch;    return; fi
  if need apk;     then echo alpine;  return; fi
  echo other
}
FAMILY=$(detect_family)

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------

if [ "${DO_UNINSTALL}" -eq 1 ]; then
  say "Removing Snatch"
  case "${FAMILY}" in
    debian) ${SUDO} apt-get remove -y snatch-dl || true ;;
    fedora) ${SUDO} dnf remove -y snatch-dl || true ;;
    suse)   ${SUDO} zypper --non-interactive remove snatch-dl || true ;;
    arch)   ${SUDO} pacman -R --noconfirm snatch-dl || true ;;
    *)
      for path in /usr/bin/snatch-gui /usr/bin/snatch-nmh \
                  /usr/share/applications/com.snatch.dl.desktop \
                  /etc/opt/chrome/native-messaging-hosts/com.snatch.dl.nmh.json \
                  /etc/chromium/native-messaging-hosts/com.snatch.dl.nmh.json \
                  /usr/lib/mozilla/native-messaging-hosts/com.snatch.dl.nmh.json; do
        if [ -e "${path}" ]; then
          ${SUDO} rm -f "${path}"
        fi
      done
      ${SUDO} rm -rf /usr/share/snatch-dl
      ;;
  esac
  info "Your downloads and settings in ~/.local/share/snatch-dl were left alone."
  printf '\n%sSnatch has been removed.%s\n' "${GREEN}" "${RESET}"
  exit 0
fi

# ---------------------------------------------------------------------------
# Find the release
# ---------------------------------------------------------------------------

say "Looking up the latest release"
if [ -n "${WANT_VERSION}" ]; then
  META=$(fetch "${API}/tags/v${WANT_VERSION}") || die "there is no release v${WANT_VERSION}"
else
  META=$(fetch "${API}/latest") || die "could not reach GitHub to find a release"
fi

TAG=$(printf '%s' "${META}" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
[ -n "${TAG}" ] || die "the release listing had no tag in it"
VERSION=${TAG#v}
info "found ${TAG}"

# Pick the asset this system can actually install.
case "${FAMILY}" in
  debian) SUFFIX="_amd64.deb" ;;
  fedora|suse) SUFFIX=".${ARCH}.rpm" ;;
  arch)   SUFFIX="-${ARCH}.pkg.tar.zst" ;;
  *)      SUFFIX="-${ARCH}-linux.tar.gz" ;;
esac

ASSET=$(printf '%s' "${META}" \
  | tr ',' '\n' \
  | sed -n 's/.*"browser_download_url": *"\([^"]*\)".*/\1/p' \
  | grep -- "${SUFFIX}$" \
  | head -n 1)
[ -n "${ASSET}" ] || die "release ${TAG} has no asset ending in ${SUFFIX}"

WORK=$(mktemp -d)
# Runs on every exit path, including a failure part-way through.
trap 'rm -rf "${WORK}"' EXIT INT TERM
FILE="${WORK}/$(basename "${ASSET}")"

say "Downloading $(basename "${ASSET}")"
fetch_file "${ASSET}" "${FILE}" || die "could not download ${ASSET}"

# Verify against the checksums published beside it. A tampered mirror is the
# whole reason this file exists, so a mismatch stops the install.
SUMS=$(printf '%s' "${META}" | tr ',' '\n' \
  | sed -n 's/.*"browser_download_url": *"\([^"]*SHA256SUMS\)".*/\1/p' | head -n 1)
if [ -n "${SUMS}" ] && need sha256sum; then
  fetch_file "${SUMS}" "${WORK}/SHA256SUMS" || warn "could not fetch SHA256SUMS"
  if [ -f "${WORK}/SHA256SUMS" ]; then
    EXPECTED=$(grep " $(basename "${FILE}")\$" "${WORK}/SHA256SUMS" | awk '{print $1}')
    ACTUAL=$(sha256sum "${FILE}" | awk '{print $1}')
    if [ -n "${EXPECTED}" ] && [ "${EXPECTED}" != "${ACTUAL}" ]; then
      die "checksum mismatch — refusing to install $(basename "${FILE}")"
    fi
    info "checksum verified"
  fi
else
  warn "no checksum available; installing unverified"
fi

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------

say "Installing Snatch ${VERSION}"
case "${FAMILY}" in
  debian)
    # `apt install ./file.deb` resolves the dependencies the package
    # declares, which is what brings in aria2 and the GTK stack.
    ${SUDO} apt-get update -qq || true
    ${SUDO} apt-get install -y "${FILE}"
    ;;
  fedora)
    ${SUDO} dnf install -y "${FILE}"
    ;;
  suse)
    ${SUDO} zypper --non-interactive install --allow-unsigned-rpm "${FILE}"
    ;;
  arch)
    ${SUDO} pacman -U --noconfirm "${FILE}"
    ;;
  *)
    warn "unrecognised distribution; installing the portable build by hand"
    tar -xzf "${FILE}" -C "${WORK}"
    ROOT=$(find "${WORK}" -maxdepth 1 -type d -name 'snatch-*' | head -n 1)
    [ -n "${ROOT}" ] || die "the tarball did not contain what was expected"
    ${SUDO} install -Dm755 "${ROOT}/snatch-gui" /usr/bin/snatch-gui
    ${SUDO} install -Dm755 "${ROOT}/snatch-nmh" /usr/bin/snatch-nmh
    ${SUDO} install -Dm644 "${ROOT}/extension/manifest.json" \
      /usr/share/snatch-dl/extension/manifest.json
    ${SUDO} install -Dm644 "${ROOT}/extension/background.js" \
      /usr/share/snatch-dl/extension/background.js
    if [ -d "${ROOT}/packaging/native-messaging" ]; then
      ${SUDO} install -Dm644 "${ROOT}/packaging/native-messaging/chromium.json" \
        /etc/opt/chrome/native-messaging-hosts/com.snatch.dl.nmh.json
      ${SUDO} install -Dm644 "${ROOT}/packaging/native-messaging/chromium.json" \
        /etc/chromium/native-messaging-hosts/com.snatch.dl.nmh.json
      ${SUDO} install -Dm644 "${ROOT}/packaging/native-messaging/firefox.json" \
        /usr/lib/mozilla/native-messaging-hosts/com.snatch.dl.nmh.json
    fi
    warn "aria2 is required and was not installed for you; install it with your package manager"
    ;;
esac

# ---------------------------------------------------------------------------
# The tools each feature needs
# ---------------------------------------------------------------------------

if [ "${WANT_EXTRAS}" -eq 1 ]; then
  say "Installing the tools the optional features use"
  info "${DIM}ffmpeg, yt-dlp, gallery-dl, wget2 and 7-Zip. Pass --no-extras to skip.${RESET}"
  case "${FAMILY}" in
    debian) ${SUDO} apt-get install -y ffmpeg yt-dlp gallery-dl wget2 p7zip-full || \
              warn "some optional tools are not in your repositories" ;;
    fedora) ${SUDO} dnf install -y ffmpeg-free yt-dlp wget2 p7zip || \
              warn "some optional tools are not in your repositories" ;;
    suse)   ${SUDO} zypper --non-interactive install yt-dlp wget2 p7zip || \
              warn "some optional tools are not in your repositories" ;;
    arch)   ${SUDO} pacman -S --needed --noconfirm ffmpeg yt-dlp gallery-dl wget2 p7zip || \
              warn "some optional tools are not in your repositories" ;;
    *)      warn "install ffmpeg, yt-dlp and gallery-dl with your package manager" ;;
  esac
  # Snatch fetches whatever is still missing on its own, from the Dependencies
  # page, so a gap here is not the end of the road.
fi

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------

if need snatch-gui; then
  INSTALLED="$(command -v snatch-gui)"
else
  INSTALLED="/usr/bin/snatch-gui"
fi

cat <<SUMMARY

${GREEN}${BOLD}Snatch ${VERSION} is installed.${RESET}

  Run it from your applications menu, or with:  ${BOLD}snatch-gui${RESET}
  ${DIM}${INSTALLED}${RESET}

${BOLD}Add the browser extension${RESET}

  ${BOLD}Chrome, Chromium, Brave, Edge, Opera${RESET}
    1. Open ${BOLD}chrome://extensions${RESET}
    2. Turn on ${BOLD}Developer mode${RESET} (top right)
    3. Click ${BOLD}Load unpacked${RESET} and choose this folder:
       ${BOLD}/usr/share/snatch-dl/extension${RESET}

  ${BOLD}Firefox${RESET}
    1. Open ${BOLD}about:debugging#/runtime/this-firefox${RESET}
    2. Click ${BOLD}Load Temporary Add-on…${RESET}
    3. Choose ${BOLD}/usr/share/snatch-dl/extension-firefox/manifest.json${RESET}
       ${DIM}(Firefox clears temporary add-ons when it restarts)${RESET}

  ${DIM}Native messaging is already registered system-wide, so the extension
  finds Snatch as soon as it loads. No further setup.${RESET}

${BOLD}Uninstall${RESET}

  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/get.sh | sh -s -- --uninstall

SUMMARY
