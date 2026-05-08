<h1 align="center">
  <img alt="Kooha" src="data/icons/io.github.seadve.Kooha.svg" width="192" height="192"/>
  <br>
  Kooha — GPU-accelerated fork
</h1>

<p align="center">
  <strong>Elegantly record your screen — now with proper hardware encoding on Arch Linux.</strong>
</p>

<p align="center">
  <img src="data/screenshots/preview.png" alt="Preview"/>
</p>

This is a fork of [SeaDve/Kooha](https://github.com/SeaDve/Kooha) that adds
first-class hardware-accelerated encoding (VA-API for Intel/AMD, NVENC for
NVIDIA) and an Arch-Linux installer that handles every dependency for you,
detects your GPU, and pulls in the right driver packages so the new profiles
light up automatically.

If you're on Flatpak or another distro, use the upstream project — this fork
intentionally drops Flatpak packaging in favour of a clean source build.

## ✨ What's different in this fork

* 🎬 **Six new GPU profiles** added to the format picker — `va-h264`,
  `va-h265`, `va-av1` (VA-API) and `nvenc-h264`, `nvenc-h265`, `nvenc-av1`
  (NVENC). All shipped as first-class profiles, not buried behind the
  experimental flag.
* 🩻 **Hardware-aware UI.** Profiles whose GStreamer plugins aren't installed
  appear in the dropdown but are dimmed and unselectable, with a tooltip
  pointing at the missing element (e.g. *"Failed to parse videoenc bin: no
  element 'nvh264enc'"*) — no more silent "format unavailable" surprises.
* 🛠️ **Arch one-line installer** that detects your GPU via `lspci`, installs
  matching VA-API / NVENC runtime packages, and builds from source. No
  Flatpak, no manual dependency hunting.
* 🚫 **Flatpak packaging removed.** Source build only. If you want Flatpak,
  upstream is one fork-link away.

## 🚀 Install (Arch Linux)

```shell
bash <(curl -fsSL https://raw.githubusercontent.com/MurderFromMars/Kooha/main/install.sh)
```

The installer will:

1. Verify you're on Arch (or an Arch derivative).
2. Install base build + runtime dependencies via `pacman --needed`.
3. Detect Intel / AMD / NVIDIA GPUs and add the matching VA-API or NVENC
   driver packages. NVIDIA is only auto-handled if the proprietary kernel
   module is already loaded; otherwise it warns rather than reaching into
   your driver stack.
4. Configure meson, build, and `sudo meson install` to `/usr/local`.

Override the install prefix with `KOOHA_PREFIX=/opt`. Override the source
directory with `KOOHA_SRC_DIR=/path/to/checkout` (otherwise the script
clones a fresh copy to a tempdir, or builds the cwd if it's already a
Kooha checkout).

After install, run `kooha`. GPU profiles appear under **Preferences → Format**.

## 📋 Runtime requirements

The installer handles all of these, but for reference:

* `pipewire` + `gst-plugin-pipewire`
* `xdg-desktop-portal` + a backend (`xdg-desktop-portal-gtk`, `-kde`, `-wlr`)
* `gst-plugins-base`, `-good`, `-bad`, `-ugly` (the `-bad` plugin set
  contains `va*enc` and `nv*enc` for hardware encoding)
* GPU userspace driver matching your hardware (`intel-media-driver`,
  `libva-mesa-driver`, or `libva-nvidia-driver`)

## 🎚️ How GPU profile selection works

Open **Preferences → Format** to see the full profile list. Each profile is
backed by a GStreamer pipeline; if the corresponding encoder element is
missing on your system, the profile is shown but greyed out with a tooltip
explaining what's missing. Install the named plugin (or driver), restart
Kooha, and it lights up.

| Profile        | Container | Encoder element  | Typical hardware             |
| -------------- | --------- | ---------------- | ---------------------------- |
| `va-h264`      | MP4       | `vah264enc`      | Intel iGPU / AMD via Mesa    |
| `va-h265`      | MP4       | `vah265enc`      | Intel iGPU / AMD via Mesa    |
| `va-av1`       | Matroska  | `vaav1enc`       | Intel Arc / RDNA3+ AMD       |
| `nvenc-h264`   | MP4       | `nvh264enc`      | NVIDIA + proprietary driver  |
| `nvenc-h265`   | MP4       | `nvh265enc`      | NVIDIA + proprietary driver  |
| `nvenc-av1`    | Matroska  | `nvav1enc`       | NVIDIA Ada (40-series) +     |

The original CPU-encoded profiles (`webm-vp8`, `mp4`/`x264`, `matroska-h264`,
`gif`) are unchanged.

## ⚙️ Experimental features

Set `KOOHA_EXPERIMENTAL` to one or more of these keys to enable:

| Key                      | What it unlocks                                     |
| ------------------------ | --------------------------------------------------- |
| `all`                    | All experimental features                           |
| `experimental-formats`   | Software VP9 / AV1 (`webm-vp9`, `webm-av1`)         |
| `multiple-video-sources` | Recording multiple monitors or windows              |
| `window-recording`       | Recording a single window (flickers on some setups) |

```shell
KOOHA_EXPERIMENTAL=experimental-formats kooha
```

GPU-accelerated profiles are no longer gated by this flag in this fork —
they're always visible, with availability gating handled by the UI.

## 🏗️ Manual build (no installer)

If you'd rather drive meson by hand:

```shell
git clone https://github.com/MurderFromMars/Kooha.git
cd Kooha

# install build deps yourself, then:
meson setup _build --prefix=/usr/local --buildtype=release
meson compile -C _build
sudo meson install -C _build
```

Build dependencies: `meson`, `ninja`, `cargo`/`rust`, `pkgconf`, `glib2`,
`gtk4` (≥ 4.15.3), `libadwaita` (≥ 1.9), `gstreamer` (≥ 1.24),
`gst-plugins-base` (≥ 1.24), and the GStreamer plugin sets listed under
runtime requirements above. `appstream` is optional (used for AppData
validation).

## 😕 It doesn't work

* **Screencast portal can't find a backend** → install
  `xdg-desktop-portal-gtk`, `-kde`, or `-wlr` for your compositor and check
  the [xdg-desktop-portal-wlr troubleshooting checklist](https://github.com/emersion/xdg-desktop-portal-wlr/wiki/%22It-doesn't-work%22-Troubleshooting-Checklist).
* **GPU profile is dimmed** → hover for the missing-element tooltip. Most
  often you need `gst-plugins-bad` or the right userspace VA-API driver
  (`intel-media-driver`, `libva-mesa-driver`, `libva-nvidia-driver`).
  Check with `vainfo` after install.
* **NVENC profile dimmed despite NVIDIA GPU** → NVENC requires the
  proprietary `nvidia` (or `nvidia-dkms`) driver and `nvidia-utils`. The
  open-source nouveau stack does not expose NVENC.

## 💝 Credit

Kooha is the work of [Dave Patrick Caberto (@SeaDve)](https://github.com/SeaDve)
and [contributors](https://github.com/SeaDve/Kooha/graphs/contributors). This
fork is a thin layer over a great app — please [donate to upstream](https://seadve.github.io/donate/)
if you get value out of it.

Translations are maintained on upstream's [Weblate](https://hosted.weblate.org/engage/seadve/).
This fork inherits them; new strings introduced here are English-only for
now.

## 📄 License

GPL-3.0-or-later, same as upstream.
