# Diagnostics and recovery

Check and restart the user service:

```sh
systemctl --user status milevox.service
systemctl --user restart milevox.service
journalctl --user -u milevox.service -n 100
```

Follow logs with `journalctl --user -u milevox.service -f`. Service logs are the
first place to investigate an unavailable GUI, microphone errors, model-load
failures, or delivery failures.

Milevox retains diagnostics for the latest transcription attempt in memory:

```sh
milevox debug last
```

This entry clears on daemon restart. Persistent transcript diagnostics are
explicitly controlled with `milevox debug enable` and `milevox debug disable`.
Persistent debug output may contain transcript text; disable it after diagnosis
and protect logs accordingly.

The persistent log is `$XDG_STATE_HOME/milevox/debug.log` (normally
`~/.local/state/milevox/debug.log`). Milevox appends across restarts, rotates at
5 MiB, and retains one `debug.log.1` backup; both files use mode `0600`. Remove
both retained logs with:

```sh
milevox debug clear
```

When debug logging is disabled, Milevox does not create or touch the state
directory. A log-write failure is reported as a warning and never discards an
otherwise delivered transcript.

If a completed event says clipboard fallback was used, typing failed but the
transcript was copied successfully. Focus the intended application and paste.
If recording or processing is stuck, cancel it with `milevox record cancel`,
then restart the service if it does not return to idle.
