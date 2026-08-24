# Milevox GUI for Omarchy

This integration adds a bar widget, keyboard-friendly settings panel, live
overlay, and Hyprland bindings. It requires Milevox on `PATH`, Omarchy Shell,
`hyprctl`, and `jq`.

## Install and remove

Install the GUI package and activate it:

```sh
sudo pacman -U ./milevox-omarchy-VERSION-1-any.pkg.tar.zst
milevox-omarchy install
```

From a checkout use `./guis/omarchy/install.sh`. Check conflicting shortcuts
first with `omarchy menu keybindings --print`; custom bindings are accepted via
`--toggle-key "SUPER + ALT + V"` and `--push-to-talk-key "F10"`.

Remove user integration **before** removing the package:

```sh
milevox-omarchy uninstall
sudo pacman -R milevox-omarchy
```

The source equivalent is `./guis/omarchy/uninstall.sh`. GUI removal preserves
the daemon, configuration, credentials, logs, and downloaded models. See
[privacy](../../docs/privacy.md) to remove those separately.

## Controls

- `SUPER + CTRL + X` toggles recording. Holding `F9` records until release.
- Left-click the bar icon to open the panel; right-click toggles recording.
- The primary button starts, stops, cancels active transcription/refinement, or
  restarts an unavailable service.
- Tab and Shift+Tab move focus. Enter or Space activates the focused button.
  Escape clears the token field, closes an idle panel, or cancels active work.

Stopping submits captured audio for transcription. Cancelling discards the
current recording or in-progress work. A completed warning remains visible in
the bar after the overlay closes. Clipboard fallback means delivery could not
type into the focused app: paste the copied transcript manually.

## Recovery

Use the panel's **Restart** action, or run:

```sh
systemctl --user restart milevox.service
journalctl --user -u milevox.service -n 100
```

If settings disappear, the daemon connection was lost; they return after a
successful reconnect. For deeper investigation see
[diagnostics](../../docs/diagnostics.md).

For source development, validate from the repository root with `make check-guis`
and restart the shell after changes (`omarchy restart shell`).
