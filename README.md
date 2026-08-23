# Milevox

Milevox is a speech-to-text command-line tool and daemon. It is based on a tool
for macOS called [Jabber](https://github.com/rselbach/jabber) I wrote a while ago.

It does audio transcription and adds optional post-processing where it sends the
transcription to an LLM with instructions to clean it up as well as recognize
some base commands like "new paragraph" or converting "fire emoji" to the
appropriate emoji.

This project is comprised of a core daemon and optional GUIs.

## Requirements

- Linux on `x86_64` or `aarch64`
- PipeWire
- A Wayland session with `wtype` and `wl-copy` for text delivery
- `curl` and GNU core utilities
- A systemd user session when using the installer

Milevox uses `parakeet-rs` and ONNX Runtime for local inference. It doesn't require a
separate transcription executable.

## Configuration

Milevox reads `$XDG_CONFIG_HOME/milevox/config.toml`, or
`~/.config/milevox/config.toml`. Every section is
optional. These values show the defaults:

```toml
[post_processing]
enabled = false
provider = "openrouter"

[output]
mode = "type"
clipboard_fallback = true

[debug]
enabled = false
```

You can also specify a custom location for the local model:

```toml
[transcription]
model_path = "/path/to/parakeet-tdt-model"
```

Cloud post-processing is disabled by default. Enable OpenRouter with:

```toml
[post_processing]
enabled = true
provider = "openrouter"
model = "~openai/gpt-mini-latest"
```

You can change the same settings without restarting the daemon:

```sh
milevox settings set --enabled true
milevox settings set --provider opencode_zen
milevox settings set --model glm-5.2
milevox settings show
```

After you edit `config.toml`, restart the daemon:

```sh
systemctl --user restart milevox.service
```
