# Milevox

Milevox is a Linux speech-to-text daemon and command-line tool. Transcription
runs locally; optional cloud post-processing can clean up the resulting text.

## Install

Release packages are available for `x86_64` and `aarch64`. Generic Linux
binaries require glibc 2.35 or newer and `GLIBCXX_3.4.30` or newer. Milevox
also needs PipeWire and, on Wayland, `wtype` and `wl-copy` for delivery.

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
to stop. Milevox transcribes and types into the focused application. If typing
is unavailable and clipboard fallback is enabled, the text is copied instead;
paste it manually. Omarchy users can use `SUPER + CTRL + X` or hold `F9`.

See [configuration](docs/configuration.md), [diagnostics](docs/diagnostics.md),
[privacy](docs/privacy.md), and the [Omarchy GUI guide](guis/omarchy/README.md).
