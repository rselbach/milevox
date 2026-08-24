# Privacy and data retention

Audio transcription runs locally. Audio stays on the machine and is not sent
to a post-processing provider. Cloud cleanup is disabled by default; when
enabled, the complete transcript text is sent to the selected provider and is
then subject to that provider's privacy and retention terms.

Milevox 0.2.0 changes the default from automatic typing to copying completed
transcripts to the clipboard. A clipboard manager can retain copied text
according to its own history and retention settings. To avoid clipboard
exposure, explicitly configure `mode = "type"`; Milevox then types only after
verifying that the output target is unchanged. Automatic clipboard fallback is
disabled by default. Setting
`clipboard_fallback = true` opts in to copying a transcript when type delivery
fails.

Tokens entered through Milevox are stored in a private mode-`0600` TOML file at
`$XDG_CONFIG_HOME/milevox/credentials.toml` (normally
`~/.config/milevox/credentials.toml`).
Tokens can instead come from the service environment. Remove a stored token
with `milevox settings token remove [--provider PROVIDER]`; an environment token
must be removed from its environment source. Do not claim removal until
`milevox settings show` reports the expected token source.

Configuration, credentials, logs, debug transcripts, and downloaded models are
user data. Uninstalling only the Omarchy integration preserves all of them.
Uninstall the GUI integration before its package. Then run `milevox-teardown`
as the desktop user before removing the daemon package with the package
manager. Raw-archive users run the corresponding per-user `uninstall.sh`.
Finally, if desired, manually remove the Milevox config,
credential, data/model, cache, and log locations shown by your installation.
Review them before deletion; package removal intentionally does not erase user
data.
