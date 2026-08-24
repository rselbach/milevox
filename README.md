# Milevox

Milevox is a Linux speech-to-text daemon and command-line tool. Transcription
runs locally; optional cloud post-processing can clean up the resulting text.

## Install

Release packages are available for `x86_64` and `aarch64`. Generic Linux
binaries require glibc 2.35 or newer and `GLIBCXX_3.4.30` or newer. Milevox
also needs PipeWire and, on Wayland, `wtype` and `wl-copy` for delivery.

The pinned Parakeet model recognizes English. Initial setup downloads about
660 MB of model data. Keep at least 1 GB free in your XDG data directory before
installation. The default model path is
`$XDG_DATA_HOME/milevox/models/parakeet-tdt-0.6b-v2-int8`, normally
`~/.local/share/milevox/models/parakeet-tdt-0.6b-v2-int8`.

On an Arch-based system, download the package for your architecture and run:

```sh
sudo pacman -U ./milevox-VERSION-1-ARCHITECTURE.pkg.tar.zst
milevox-setup
```

For Omarchy, install the separate GUI package, then activate it (this also runs
daemon setup):

```sh
sudo pacman -U ./milevox-omarchy-VERSION-1-any.pkg.tar.zst
milevox-omarchy install
```

Release archives additionally contain a per-user `install.sh` installer.

## First dictation

Start dictation with `milevox record toggle`, speak, then run the command again
to stop. Milevox transcribes and copies the result to the clipboard. You can
explicitly opt in to typing into a verified output target. Omarchy users can
use `SUPER + CTRL + X` or hold `F9`.

See [configuration](docs/configuration.md), [diagnostics](docs/diagnostics.md),
[privacy](docs/privacy.md), [release notes](docs/release-notes.md), and the
[Omarchy GUI guide](https://github.com/rselbach/milevox/blob/main/guis/omarchy/README.md).
