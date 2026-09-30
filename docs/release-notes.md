# Release notes

## 0.2.0, unreleased

### Transcription

Updated `parakeet-rs` to 0.3.8, including its NeMo-aligned audio preprocessing
and TDT timestamp corrections. Recordings shorter than one 10 ms feature
frame now produce a clear error instead of reaching the model with no
features; sub-frame live previews are skipped.

### Known dependency issue

Milevox still includes `paste` 1.0.15 through `parakeet-rs` 0.3.8 and
`tokenizers` 0.23.1. RustSec classifies `paste` as unmaintained in
[`RUSTSEC-2024-0436`](https://rustsec.org/advisories/RUSTSEC-2024-0436.html).
The advisory is informational. The audit configuration temporarily permits
only this advisory.

The `parakeet-rs` 0.3.8 update retains the locked `tokenizers` 0.23.1
dependency, which still depends on `paste`. The project does not use a
private fork. Remove the audit allowance when an upstream release removes
`paste` and the model transcription and release checks pass.

### Validation follow-ups

The Omarchy plugin passes the official validator locally with
`omarchy-dev` 4.0.0.r6579.g753d80c-1. Hosted GUI CI also downloads the official
validator from Omarchy commit
`753d80c8748cac1ecdf030eae7c463b28e71e359`, verifies its pinned SHA-256 digest,
and runs it after the QML behavior tests.

The repository pins the expected `cargo-audit` version and advisory policy,
and tests that the temporary `paste` allowance remains justified. Adding the
RustSec tool or action to pull-request and weekly CI still requires dependency
approval. Until then, release validation must run `make audit-rust` with the
pinned tool installed.
