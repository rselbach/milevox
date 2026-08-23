# Milevox GUI for Omarchy

This optional integration adds a bar widget, settings panel, live transcript
overlay, and Hyprland keybindings to Omarchy. Milevox remains a separate CLI
and daemon.

## Requirements

- A working Milevox installation
- `milevox` available on `PATH`
- Omarchy and Omarchy Shell
- `hyprctl` and `jq`

## Install

From the Milevox source checkout, run:

```sh
./guis/omarchy/install.sh
```

The installer copies this directory's plugin files to
`~/.config/omarchy/plugins/io.github.rselbach.milevox`, enables the widget, and
adds these default bindings:

- `SUPER + CTRL + X` toggles dictation.
- Holding `F9` starts dictation and releasing it stops dictation.

Pass different bindings when those keys are already assigned:

```sh
./guis/omarchy/install.sh \
  --toggle-key "SUPER + ALT + V" \
  --push-to-talk-key "F10"
```

Check current assignments before choosing keys:

```sh
omarchy menu keybindings --print
```

The overlay appears near the bottom of the screen while Milevox records,
transcribes, and refines. It shows microphone activity and a stabilized live
transcript, then briefly shows the final text. Left-click the microphone icon
to open settings. Right-click it to toggle recording.

## Remove

```sh
./guis/omarchy/uninstall.sh
```

This removes only the Omarchy plugin and its keybindings. It preserves the
Milevox CLI, daemon, configuration, credentials, logs, and models.

## Develop

Validate the plugin from the repository root:

```sh
make check-guis
```

For live development, link this directory to
`~/.config/omarchy/plugins/io.github.rselbach.milevox` and run
`omarchy restart shell` after QML changes. Omarchy's file watcher does not
follow the development symlink.
