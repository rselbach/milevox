# Milevox GUIs

This directory contains optional user interfaces for Milevox. Milevox itself
lives at the repository root and does not depend on any GUI.

Each GUI belongs in its own directory with its assets, installer, uninstaller,
and documentation. GUIs control Milevox through its command-line interface and
consume the JSON stream from `milevox status --follow`.

## Available GUIs

- [Omarchy](omarchy/README.md): bar widget, settings panel, live transcript
  overlay, and Hyprland keybindings
