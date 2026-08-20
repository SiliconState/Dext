# Releasing Dext

Releases are owner-triggered by an annotated version tag. The workflow does not publish from branches or manual dispatch and rejects tag commits outside `origin/main`. Active `v*` rules prevent updates and deletion, published releases are immutable, and initial tag creation remains a trusted maintainer action guarded by workflow validation.

## Dry review

1. Set the intended package version in `Cargo.toml`, update `Cargo.lock`, and confirm the future tag will be exactly `vX.Y.Z`.
2. Review the complete diff and ignored/untracked files for generated state, credentials, or accidental artifacts.
3. Run the installer checks and local release gate:

   ```bash
   sh -n scripts/install.sh
   sh -n scripts/test_install.sh
   scripts/install.sh --help
   sh scripts/test_install.sh
   python3 scripts/validate_pages.py docs
   cargo fmt --all -- --check
   cargo clippy -p dext --all-targets --all-features --locked --no-deps -- -D warnings
   cargo audit --deny warnings
   cargo deny check licenses
   cargo test -p ratatui-core --lib --locked
   cargo bench --no-run --locked
   cargo build --release --locked
   cargo test --release --locked
   cargo test --release --locked --test tui_smoke -- --nocapture
   cargo install --path . --force --locked
   dext --version
   ```

   Run this gate directly in a trusted host terminal or through the default `danger-full-access` Dext process. If you explicitly select `workspace-write` or `read-only`, shared `/tmp`, arbitrary PTYs (`/dev/ptmx` and `/dev/pts`), and Cargo metadata writes such as `~/.cargo/.crates.toml` may be blocked. Running Dext's complete filesystem and TUI suites inside optional confinement can therefore produce cascading temporary-directory failures, deny every PTY smoke test, and prevent installation even when the code is sound. These denials validate the selected sandbox boundary; they do not satisfy the release gate.

   An agent orchestrating the gate may use `dext --sandbox-profile danger-full-access --approval always` in a controlled checkout; that is the current default but spelling it explicitly records intent. Changing `DEXT_SANDBOX_PROFILE` inside an already-confined shell does not remove the parent process's kernel sandbox. Do not weaken `workspace-write` or grant shared temp, PTY, or Cargo-home access merely to make self-hosted tests pass.

4. Confirm branch CI is green on Linux, macOS, and Windows; do not rely only on the currently configured required checks. Windows CI includes the native Job Object descendant-lifecycle test and parses/executes the complete installer harness under both inbox Windows PowerShell 5.1 and PowerShell 7. Each engine evaluates `install.ps1` from in-memory text through `Invoke-Expression`, matching the public `irm | iex` path, and checks native architecture detection without nullable modern-.NET metadata. Linux and macOS execute the Unix installer harness. These offline tests cover successful replacement, checksum failure without clobbering the existing binary, exact-revision source fallback, malformed release/ref responses, strict tag parsing, unsafe destination refusal, version mismatch, fallback disablement, and attestation-required refusal of source fallback. Windows additionally forces unsupported `File.Replace`, successful fallback, rollback, and retained-backup recovery paths. Linux compiles Criterion benchmarks. Confirm the scheduled security workflow passes both vulnerability auditing and the dependency-license policy. If terminal dependencies or `src/tui.rs` changed, apply the renderer contract and live-terminal checks in [`TUI.md`](TUI.md). Review `.github/workflows/release.yml`, especially its full action commit pins, quality gate, four-target matrix, annotated-tag/origin-`main` ancestry checks, and publish-job permissions.
5. Before every public release, recheck the owner-controlled GitHub settings: require Windows CI alongside Ubuntu/macOS, protect `v*` tags from update and deletion, and keep immutable releases, private vulnerability reporting, vulnerability alerts, and Dependabot security updates enabled. Confirm the tag and release do not already exist. Initial release-tag creation remains a trusted maintainer action because this personal repository does not currently enforce a creation-only rule with an owner bypass; the workflow must still reject lightweight tags, commits outside `origin/main`, and tag/package version mismatches.

## First-release evidence

The publication path first completed for [`v0.1.0`](https://github.com/SiliconState/Dext/releases/tag/v0.1.0) in [release run `31139795179`](https://github.com/SiliconState/Dext/actions/runs/31139795179) from commit `9a48eb9a8f7065a2dd71041527e0b276a7444876`:

- [x] Annotated-tag, `origin/main` ancestry, and package-version validation passed.
- [x] Four platform archives and `dext.cdx.json` were published and listed in `SHA256SUMS`.
- [x] Provenance verification passed for every checksummed asset in the workflow and an independent post-publication download.
- [x] Packaged binaries passed the workflow smoke checks.

The README/usage installers now consume the published archives automatically, so normal installation does not require Rust. Their exact-revision locked source fallback remains available only when no tagged release exists. Attestation-required mode refuses that fallback because a local source build has no release attestation.

## Publish

After the reviewed release commit is on `main`, create and push one annotated tag:

```bash
git tag -a vX.Y.Z -m "Dext vX.Y.Z"
git push origin vX.Y.Z
```

The tag workflow must:

- reject a lightweight tag, a tag commit not contained in `origin/main`, or a tag/version that differs from the `Cargo.toml` package version;
- run the Linux quality gate: formatting, Clippy with warnings denied, vendored `ratatui-core` tests, benchmark compilation, vulnerability auditing, and dependency-license checks;
- build and test Linux x86_64 GNU, macOS x86_64, macOS arm64, and Windows x86_64 MSVC with `--release --locked`;
- run each packaged binary with `--version`;
- publish four archives, one CycloneDX JSON SBOM (`dext.cdx.json`), and one sorted, verified `SHA256SUMS` covering every asset;
- generate and verify GitHub build-provenance attestations for every checksummed asset before creating the release.

Monitor every matrix job and inspect the release assets. If publication or later verification fails, mark the release affected or withdraw it and publish a new patch version. Do not move the tag or overwrite release assets.

## Verify published assets

Download the archive for the current platform, the SBOM, and `SHA256SUMS`, then verify both assets:

```bash
set -euo pipefail
version=vX.Y.Z
archive="dext-${version}-x86_64-unknown-linux-gnu.tar.gz"
gh release download "$version" --repo SiliconState/Dext \
  --pattern "$archive" --pattern dext.cdx.json --pattern SHA256SUMS
awk -v archive="$archive" '
  $2 == archive { print; archive_found++ }
  $2 == "dext.cdx.json" { print; sbom_found++ }
  END { if (archive_found != 1 || sbom_found != 1) exit 1 }
' SHA256SUMS > selected-SHA256SUMS
sha256sum --check selected-SHA256SUMS
rm selected-SHA256SUMS
gh attestation verify "$archive" --repo SiliconState/Dext
gh attestation verify dext.cdx.json --repo SiliconState/Dext
```

On macOS, use `shasum -a 256 -c selected-SHA256SUMS` in place of `sha256sum --check selected-SHA256SUMS`. Windows users can run `Get-FileHash -Algorithm SHA256` and compare both asset values with `SHA256SUMS` before running the attestation commands.

GitHub artifact attestations are available for public repositories on current GitHub plans. Attestations for private or internal repositories require GitHub Enterprise Cloud. Verification requires a recent GitHub CLI with `gh attestation verify` support.
