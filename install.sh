#!/usr/bin/env bash
# Kooha installer — Arch Linux only.
# Installs build/runtime dependencies, detects the GPU and pulls in the
# matching VA-API/NVENC packages, then builds and installs Kooha via meson.
#
# Run from inside a checkout, or one-shot via curl:
#   bash <(curl -fsSL https://raw.githubusercontent.com/MurderFromMars/Kooha/main/install.sh)

set -euo pipefail

readonly REPO_URL="https://github.com/MurderFromMars/Kooha.git"
readonly PREFIX="${KOOHA_PREFIX:-/usr/local}"

note()  { printf '\033[1;36m::\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
die()   { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }

# 1. Distro check
[[ -r /etc/os-release ]] || die "/etc/os-release missing — can't detect distro."
# shellcheck disable=SC1091
. /etc/os-release
if [[ "${ID:-}" != "arch" && "${ID_LIKE:-}" != *arch* ]]; then
  die "This installer supports Arch Linux only (detected ID=${ID:-unknown})."
fi
command -v pacman >/dev/null || die "pacman not found in PATH."
command -v sudo   >/dev/null || die "sudo not found in PATH."

# 2. Base build + runtime dependencies (matches meson.build)
BASE_PKGS=(
  base-devel git rust meson ninja pkgconf
  glib2 gtk4 libadwaita
  gstreamer gst-plugins-base gst-plugins-good
  gst-plugins-bad gst-plugins-ugly gst-plugin-pipewire
  pipewire pipewire-pulse
  xdg-desktop-portal xdg-desktop-portal-gtk
  desktop-file-utils appstream
)

# 3. GPU detection → encoder runtime deps
GPU_PKGS=()
GPU_INFO="$(lspci -nn 2>/dev/null | grep -Ei 'vga|3d controller|display controller' || true)"

if [[ -z "$GPU_INFO" ]]; then
  warn "No GPU detected via lspci — only software encoders will be available."
else
  note "Detected GPUs:"
  printf '   %s\n' "$GPU_INFO"

  if grep -qi 'intel' <<<"$GPU_INFO"; then
    note "→ Intel: adding intel-media-driver (iHD, Gen8+) for VA-API"
    GPU_PKGS+=(intel-media-driver libva-utils)
  fi

  if grep -qiE 'amd|ati|radeon' <<<"$GPU_INFO"; then
    note "→ AMD: adding libva-mesa-driver for VA-API"
    GPU_PKGS+=(libva-mesa-driver libva-utils)
  fi

  if grep -qi 'nvidia' <<<"$GPU_INFO"; then
    if lsmod 2>/dev/null | grep -q '^nvidia'; then
      note "→ NVIDIA (proprietary driver loaded): adding libva-nvidia-driver"
      GPU_PKGS+=(libva-nvidia-driver libva-utils)
    else
      warn "NVIDIA GPU detected but the proprietary 'nvidia' kernel module isn't loaded."
      warn "NVENC profiles need the proprietary driver — install 'nvidia' or 'nvidia-dkms' and reboot."
    fi
  fi
fi

# 4. Install
note "Installing ${#BASE_PKGS[@]} base + ${#GPU_PKGS[@]} GPU package(s) via pacman..."
sudo pacman -S --needed --noconfirm "${BASE_PKGS[@]}" "${GPU_PKGS[@]}"

# 5. Resolve source dir
SRC_DIR="${KOOHA_SRC_DIR:-}"
if [[ -z "$SRC_DIR" ]]; then
  if [[ -f "$PWD/meson.build" ]] && grep -q "'kooha'" "$PWD/meson.build" 2>/dev/null; then
    SRC_DIR="$PWD"
    note "Building from current checkout: $SRC_DIR"
  else
    SRC_DIR="${TMPDIR:-/tmp}/kooha-build-$$"
    note "Cloning Kooha into $SRC_DIR ..."
    git clone --depth 1 "$REPO_URL" "$SRC_DIR"
    trap 'rm -rf "$SRC_DIR"' EXIT
  fi
fi

# 6. Build & install
cd "$SRC_DIR"
note "Configuring meson (prefix=$PREFIX, release)..."
if [[ -d _build ]]; then
  meson setup --reconfigure _build --prefix="$PREFIX" --buildtype=release
else
  meson setup _build --prefix="$PREFIX" --buildtype=release
fi

note "Compiling..."
meson compile -C _build

note "Installing to $PREFIX (sudo)..."
sudo meson install -C _build

cat <<EOF

$(printf '\033[1;32m✓\033[0m') Kooha installed to $PREFIX. Run: kooha

Tips:
  • GPU profiles appear in Preferences → Format. Unavailable ones are
    greyed out with a tooltip explaining what's missing.
  • For VP9 / software AV1 profiles, launch with:
        KOOHA_EXPERIMENTAL=experimental-formats kooha
EOF
