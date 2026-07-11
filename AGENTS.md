# AGENTS.md

## Purpose

This repository is a strict native fast path for a useful subset of
`ccstatusline`, not an independent status-line design. Its compatibility target
is pinned in `src/lib.rs` and is currently `ccstatusline@2.2.22`.

The central invariant is:

> Render natively only when every output-affecting configuration value is
> implemented and tested; otherwise preserve stdin, delegate to the pinned
> reference implementation, and keep diagnostics off stdout.

Never accept a setting merely because the current fixture appears unaffected.
Unknown configuration is unsupported until its semantics are understood.

## Repository map

- `src/config.rs`: configuration parsing and fast-path eligibility.
- `src/status.rs`: Claude Code status-input interpretation.
- `src/widgets.rs`: widget values.
- `src/render.rs` and `src/ansi.rs`: layout, Powerline styling, width, and
  truncation.
- `src/terminal.rs`: width overrides, direct TTY probing, ancestor discovery,
  and true-headless behavior.
- `src/git.rs` and `src/effort.rs`: data providers used by widgets.
- `src/fallback.rs`: pinned reference invocation and stdin/stdout preservation.
- `src/app.rs`: CLI dispatch, warnings, TUI delegation, and renderer selection.
- `tests/fixtures/`: checked-in nonsensitive configs, status inputs, and golden
  outputs.
- `.github/workflows/`: Ubuntu/macOS checks and release publication.

Keep generated packages, downloaded upstream sources, benchmark output, and
other reproducible artifacts out of the repository.

## Supported surface

Keep the detailed table in `README.md` synchronized with the validator and
implementation. The initial widgets are:

- `vim-mode`
- `context-bar`
- `flex-separator`
- `model`
- `thinking-effort`
- `current-working-dir`
- `git-branch`

Support is option-specific. A widget name being present in the list does not
make every field or metadata value valid. In particular, do not weaken checks
for generic bold/dim/merge/hide/custom-command behavior or unknown fields until
the corresponding rendering path is implemented.

## Adding a widget or option

Use this sequence for every compatibility addition:

1. Run `ccstatusline-native --support-report` and retain the reported JSON
   paths as the scope of the change.
2. Create the smallest nonsensitive version 3 config and status-input fixture
   that exercises those paths. Include terminal width, working directory, and
   relevant environment assumptions with the fixture or test.
3. Query the pinned behavior oracle. Prefer an installed reference, then:

   ```sh
   bunx -y ccstatusline@2.2.22 --config tests/fixtures/example-settings.json
   # or
   npx --yes ccstatusline@2.2.22 --config tests/fixtures/example-settings.json
   ```

   Capture raw stdout bytes. Exercise absent/null/empty values, raw and styled
   variants, wide and narrow widths, and multiple lines when those cases can
   affect the feature.
4. If outputs cannot distinguish the semantics, fetch the exact published
   package on demand into a temporary directory—for example with
   `npm pack ccstatusline@2.2.22 --pack-destination "$TMPDIR"`—and inspect the
   smallest relevant source area. Do not commit the archive or extracted code,
   and do not add ccstatusline as a submodule, subtree, or runtime source
   dependency.
5. Implement the smallest complete behavior. Add parsing/validation first so
   partially supported variants continue to fall back.
6. Add focused unit tests, checked-in golden bytes or hashes, and differential
   coverage against the oracle observations. Automated tests must not require
   network access or Bun/npm. Use `scripts/compare-reference.sh` for a manual
   raw-byte comparison before updating an oracle hash.
7. Update the supported-surface documentation and the copyable report wording
   when necessary.

If matching requires a large cross-cutting subsystem, keep the setting
unsupported and document the reason rather than shipping a plausible-looking
approximation.

## Output and fallback rules

- stdout is a protocol channel. A successful render contains status-line bytes
  only; never log, warn, or add a trailing explanation there.
- Normal fallback warnings go to stderr. Interactive TUI warnings are printed
  after the delegated TUI exits.
- Buffer stdin before validation and replay exactly those bytes to fallback.
- Capture fallback stdout and publish it only when the child succeeds.
- Invoke fallback with an argument vector, not through a shell.
- Keep the package version pinned in every fallback path and in compatibility
  reports.
- Never invent a missing runtime datum. Implement the pinned reference's tested
  absent-data behavior when it has one; otherwise delegate. Terminal width is a
  defined example: after every probe fails, render with no effective width,
  one-space Powerline flex separators, and no width truncation.
- Unicode text that `ansi::requires_reference_width` classifies as divergent
  must delegate unless differential tests prove and implement matching width.

Terminal-width changes must preserve this order: valid `CCSTATUSLINE_WIDTH`,
valid exported `COLUMNS`, direct standard-stream or `/dev/tty` ioctl, up to
eight ancestor TTYs, then `tput`. Invalid overrides fall through. Never place a
TTY name into a shell command. Keep PTY integration coverage for piped stdin,
all-piped stdio with a TTY-owning ancestor, and a genuinely headless process.

## Verification

Before committing:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
```

For rendering changes, also compare exact output against the pinned reference.
ANSI escape sequences, non-breaking spaces, reset placement, final newlines,
Unicode display width, truncation, and line-to-line color indexes are observable
behavior. Prefer byte comparisons and hashes over visual inspection.

Do not put user transcripts, repository paths containing sensitive information,
or private configuration values in fixtures. Synthetic status JSON is enough.

## Releases

The release workflow is serialized because both nightly and tagged builds can
mutate GitHub release state. Its checks job must succeed before any archive is
built. Archives include the binary, `README.md`, `LICENSE`, and
`THIRD_PARTY_NOTICES.md`.

The Homebrew formula tracks the `nightly` release produced from `main`. A
workflow in `zhyu/homebrew-tap` owns formula generation and verifies the public
archive against its published SHA-256 before committing. Keep this repository
free of cross-repository credentials; changes to artifact names or layout must
be coordinated with the tap-owned updater.
