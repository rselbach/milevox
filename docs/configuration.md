# Configuration

Milevox reads `$XDG_CONFIG_HOME/milevox/config.toml`, normally
`~/.config/milevox/config.toml`. All sections are optional:

```toml
[post_processing]
enabled = false
provider = "openrouter"

[output]
mode = "clipboard"
clipboard_fallback = false

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

## Transcript delivery

Milevox 0.2.0 changes the default from automatic typing to copying completed
transcripts to the clipboard. Clipboard managers can retain copied text after
Milevox exits, according to the manager's own history and retention settings.

To type into the application that was focused when recording began, opt in to
type mode:

```toml
[output]
mode = "type"
```

Type mode is intended for compositors where Milevox can verify that the output
target has not changed. A failed type delivery does not copy the transcript
unless you also opt in to clipboard fallback:

```toml
[output]
mode = "type"
clipboard_fallback = true
```

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

### Service environment file

`milevox-setup` creates `$XDG_CONFIG_HOME/milevox/environment`, normally
`~/.config/milevox/environment`, with mode `0600`. The containing Milevox
configuration directory has mode `0700`. The installer preserves an existing
environment file byte-for-byte.

Write one `NAME=value` assignment per line. Do not add `export`. Milevox reads
these names:

- `OPENROUTER_API_KEY`
- `OPENCODE_ZEN_API_KEY`

A token saved with `milevox settings token` takes precedence over a token in
the service environment file. The environment token remains controlled by the
file and cannot be removed with `milevox settings token remove`. After you edit
the file, run:

```sh
systemctl --user restart milevox.service
```

Run `milevox settings show` to confirm the token source.

Cloud cleanup is off by default. Before enabling it, read the
[privacy notes](privacy.md): transcript text is sent to the selected provider,
while recorded audio remains local.

`--config FILE` applies only when starting the daemon, for example
`milevox --config ./other.toml daemon`. Client commands reject it so a setting
is never silently applied to the wrong daemon.

## Recording and command limits

One recording can run for at most 10 minutes. At the limit, Milevox
automatically stops capture and submits the first 10 minutes. Running Toggle
while recording submits it; running Toggle while transcription or cleanup is
active cancels that generation and prevents its late result from being
delivered. `record stop` waits for transcription, cleanup, and delivery and
reports any terminal failure. IPC commands are limited to 64 KiB, 16 concurrent
clients, and a five-second client I/O timeout.
