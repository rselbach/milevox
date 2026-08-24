# Configuration

Milevox reads `$XDG_CONFIG_HOME/milevox/config.toml`, normally
`~/.config/milevox/config.toml`. All sections are optional:

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

A custom local model can be set with `[transcription] model_path = "/path"`.
After editing the file, run `systemctl --user restart milevox.service`.

Settings can be changed live:

```sh
milevox settings show
milevox settings set --enabled true
milevox settings set --provider openrouter
milevox settings set --model MODEL
milevox settings models
milevox settings models --provider opencode_zen
```

The GUI obtains provider and model choices from the daemon rather than keeping
its own catalog.

## Provider tokens

Add or replace a token interactively (it is read from standard input and is not
placed in shell history):

```sh
milevox settings token --provider openrouter
```

Remove only a token stored by Milevox with:

```sh
milevox settings token remove --provider openrouter
```

A provider token supplied through the service environment remains controlled
by that environment and cannot be removed by this command. Remove it from the
environment source and restart the service. Environment credentials take
effect according to the daemon's reported token source.

Cloud cleanup is off by default. Before enabling it, read the
[privacy notes](privacy.md): transcript text is sent to the selected provider,
while recorded audio remains local.

`--config FILE` applies only when starting the daemon, for example
`milevox --config ./other.toml daemon`. Client commands reject it so a setting
is never silently applied to the wrong daemon.

## Recording and command limits

One recording can run for at most 10 minutes. Running Toggle while recording
submits it; running Toggle while transcription or cleanup is active cancels
that generation and prevents its late result from being delivered. `record
stop` waits for transcription, cleanup, and delivery and reports any terminal
failure. IPC commands are limited to 64 KiB, 16 concurrent clients, and a
five-second client I/O timeout.
