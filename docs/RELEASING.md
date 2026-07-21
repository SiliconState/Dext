# Releasing Dext

Releases are owner-triggered by an immutable version tag. The workflow does not publish from branches or manual dispatch.

## Dry review

1. Set the intended package version in `Cargo.toml`, update `Cargo.lock`, and confirm the future tag will be exactly `vX.Y.Z`.
2. Review the complete diff and ignored/untracked files for generated state, credentials, or accidental artifacts.
3. Run the local release gate:

   ```bash
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

   Run this gate directly in a trusted host terminal, not through a Dext `bash` tool using the default `workspace-write` sandbox. That profile intentionally blocks shared `/tmp`, arbitrary PTYs (`/dev/ptmx` and `/dev/pts`), and Cargo metadata writes such as `~/.cargo/.crates.toml`. Running Dext's complete filesystem and TUI suites inside that confinement can therefore produce cascading temporary-directory failures, deny every PTY smoke test, and prevent installation even when the code is sound. These denials validate the sandbox boundary; they do not satisfy the release gate.

   If an agent must orchestrate the gate, start a separate trusted Dext process with `dext --sandbox-profile danger-full-access --approval always` and use it only in a controlled checkout. Changing `DEXT_SANDBOX_PROFILE` inside an already-confined shell does not remove the parent process's kernel sandbox. Do not weaken `workspace-write` or grant shared temp, PTY, or Cargo-home access merely to make self-hosted tests pass.

4. Confirm required branch CI is green on Linux, macOS, and Windows. Windows CI includes the native Job Object descendant-lifecycle test; Linux CI compiles Criterion benchmarks. Confirm the scheduled security workflow passes both vulnerability auditing and the dependency-license policy. If terminal dependencies or `src/tui.rs` changed, apply the renderer contract and live-terminal checks in [`TUI.md`](TUI.md). Review `.github/workflows/release.yml`, especially its full action commit pins, quality gate, four-target matrix, tag/version check, and publish-job permissions.
5. Confirm the tag and release do not already exist. Release artifacts are immutable; never replace bytes under an existing tag or checksum.

## First-release evidence

The publication workflow has not yet completed a version tag. Before treating it as proven, record the first successful tag run here:

- [ ] Tag/version validation passed.
- [ ] Four platform archives and `dext.cdx.json` were published and listed in `SHA256SUMS`.
- [ ] Provenance verification passed for every checksummed asset.
- [ ] Packaged binaries passed the workflow smoke checks.

After the first successful release, replace these unchecked items with the tag and workflow URL.

## Publish

After the reviewed release commit is on `main`, create and push one annotated tag:

```bash
git tag -a vX.Y.Z -m "Dext vX.Y.Z"
git push origin vX.Y.Z
```

The tag workflow must:

- reject a tag that differs from the `Cargo.toml` package version;
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
