#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d 2>/dev/null || mktemp -d -t dext-install-test)
PHASE=setup
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    if [ "$status" -ne 0 ]; then
        printf '%s\n' "Unix installer tests failed during: $PHASE" >&2
    fi
    rm -rf "$WORK"
    exit "$status"
}
trap cleanup EXIT HUP INT TERM
export COPYFILE_DISABLE=1
unset DEXT_INSTALL_DIR DEXT_VERSION DEXT_SOURCE_FALLBACK DEXT_REQUIRE_ATTESTATION
unset MOCK_MODE MOCK_CHECKSUMS MOCK_CARGO_ARGS

mkdir -p "$WORK/bin" "$WORK/release" "$WORK/package" "$WORK/wrong-package"
cat > "$WORK/package/dext" <<'EOF'
#!/bin/sh
printf '%s\n' 'dext 1.2.3'
EOF
cat > "$WORK/wrong-package/dext" <<'EOF'
#!/bin/sh
printf '%s\n' 'dext 1.2.4'
EOF
chmod 755 "$WORK/package/dext" "$WORK/wrong-package/dext"
tar -C "$WORK/package" -czf "$WORK/release/dext-v1.2.3-x86_64-unknown-linux-gnu.tar.gz" dext
tar -C "$WORK/wrong-package" -czf "$WORK/release/dext-v9.9.9-x86_64-unknown-linux-gnu.tar.gz" dext
ARCHIVE="$WORK/release/dext-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"
if command -v sha256sum >/dev/null 2>&1; then
    DIGEST=$(sha256sum "$ARCHIVE" | awk '{print toupper($1)}')
    WRONG_DIGEST=$(sha256sum "$WORK/release/dext-v9.9.9-x86_64-unknown-linux-gnu.tar.gz" | awk '{print toupper($1)}')
else
    DIGEST=$(shasum -a 256 "$ARCHIVE" | awk '{print toupper($1)}')
    WRONG_DIGEST=$(shasum -a 256 "$WORK/release/dext-v9.9.9-x86_64-unknown-linux-gnu.tar.gz" | awk '{print toupper($1)}')
fi
printf '%s  %s\n' "$DIGEST" "dext-v1.2.3-x86_64-unknown-linux-gnu.tar.gz" > "$WORK/release/SHA256SUMS"
printf '%s  %s\n' "$WRONG_DIGEST" "dext-v9.9.9-x86_64-unknown-linux-gnu.tar.gz" >> "$WORK/release/SHA256SUMS"

cat > "$WORK/bin/curl" <<'EOF'
#!/bin/sh
set -eu
out=
write_status=0
url=
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) out=$2; shift 2 ;;
        -w) write_status=1; shift 2 ;;
        -H|--retry|--retry-delay|--proto) shift 2 ;;
        --tlsv1.2|-sSL|-fL) shift ;;
        *) url=$1; shift ;;
    esac
done
status=200
case "$url" in
    */releases/latest)
        case "${MOCK_MODE:-release}" in
            source|bad-ref|attested-source)
                status=404
                printf '%s\n' '{"message":"Not Found"}' > "$out"
                ;;
            malformed-release)
                printf '%s\n' '{"name":"missing tag"}' > "$out"
                ;;
            wrong-version)
                printf '%s\n' '{"tag_name":"v9.9.9"}' > "$out"
                ;;
            *)
                printf '%s\n' '{"tag_name":"v1.2.3"}' > "$out"
                ;;
        esac
        ;;
    */git/ref/heads/main)
        if [ "${MOCK_MODE:-release}" = bad-ref ]; then
            printf '%s\n' '{"ref":"refs/heads/main","object":{"type":"tag","sha":"0123456789abcdef0123456789abcdef01234567"}}' > "$out"
        else
            printf '%s\n' '{"ref":"refs/heads/main","object":{"type":"commit","sha":"0123456789ABCDEF0123456789ABCDEF01234567"}}' > "$out"
        fi
        ;;
    */dext-v*-x86_64-unknown-linux-gnu.tar.gz)
        name=${url##*/}
        cp "$MOCK_RELEASE/$name" "$out"
        ;;
    */SHA256SUMS)
        cp "$MOCK_CHECKSUMS" "$out"
        ;;
    *)
        printf '%s\n' "unexpected URL: $url" >&2
        exit 1
        ;;
esac
if [ "$write_status" -eq 1 ]; then
    printf '%s' "$status"
fi
EOF

cat > "$WORK/bin/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
    -s) printf '%s\n' Linux ;;
    -m) printf '%s\n' x86_64 ;;
    *) printf '%s\n' Linux ;;
esac
EOF

cat > "$WORK/bin/cargo" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" > "$MOCK_CARGO_ARGS"
root=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --root) root=$2; shift 2 ;;
        *) shift ;;
    esac
done
mkdir -p "$root/bin"
cat > "$root/bin/dext" <<'BIN'
#!/bin/sh
printf '%s\n' 'dext 9.9.9'
BIN
chmod 755 "$root/bin/dext"
EOF
chmod 755 "$WORK/bin/curl" "$WORK/bin/uname" "$WORK/bin/cargo"

run_installer() {
    MOCK_MODE="${MOCK_MODE:-release}" \
    MOCK_RELEASE="$WORK/release" \
    MOCK_CHECKSUMS="${MOCK_CHECKSUMS:-$WORK/release/SHA256SUMS}" \
    MOCK_CARGO_ARGS="$WORK/cargo.args" \
    DEXT_INSTALL_DIR="${DEXT_INSTALL_DIR:-}" \
    DEXT_SOURCE_FALLBACK="${DEXT_SOURCE_FALLBACK:-1}" \
    DEXT_REQUIRE_ATTESTATION="${DEXT_REQUIRE_ATTESTATION:-0}" \
    PATH="$WORK/bin:$PATH" \
    sh "$ROOT/scripts/install.sh" "$@"
}

PHASE='release install'
release_out=$(DEXT_INSTALL_DIR="$WORK/install release" run_installer)
printf '%s\n' "$release_out" | grep -F 'verified and installed release v1.2.3' >/dev/null
test "$("$WORK/install release/dext" --version)" = 'dext 1.2.3'

installed_digest=$(sha256_file="$WORK/install release/dext"; if command -v sha256sum >/dev/null 2>&1; then sha256sum "$sha256_file" | awk '{print $1}'; else shasum -a 256 "$sha256_file" | awk '{print $1}'; fi)

PHASE='checksum rejection'
printf '%064d  %s\n' 0 'dext-v1.2.3-x86_64-unknown-linux-gnu.tar.gz' > "$WORK/release/BADSUMS"
if DEXT_INSTALL_DIR="$WORK/install release" MOCK_CHECKSUMS="$WORK/release/BADSUMS" \
    run_installer > "$WORK/bad.out" 2> "$WORK/bad.err"; then
    printf '%s\n' 'checksum mismatch unexpectedly succeeded' >&2
    exit 1
fi
grep -F 'checksum verification failed' "$WORK/bad.err" >/dev/null
test "$("$WORK/install release/dext" --version)" = 'dext 1.2.3'
current_digest=$(sha256_file="$WORK/install release/dext"; if command -v sha256sum >/dev/null 2>&1; then sha256sum "$sha256_file" | awk '{print $1}'; else shasum -a 256 "$sha256_file" | awk '{print $1}'; fi)
test "$current_digest" = "$installed_digest"

PHASE='source fallback'
source_out=$(MOCK_MODE=source DEXT_INSTALL_DIR="$WORK/install-source" run_installer)
printf '%s\n' "$source_out" \
    | grep -F 'main commit 0123456789abcdef0123456789abcdef01234567' >/dev/null
grep -F -- '--rev 0123456789abcdef0123456789abcdef01234567 --locked' \
    "$WORK/cargo.args" >/dev/null
test "$("$WORK/install-source/dext" --version)" = 'dext 9.9.9'

PHASE='disabled source fallback'
if MOCK_MODE=source DEXT_INSTALL_DIR="$WORK/install-no-fallback" DEXT_SOURCE_FALLBACK=0 \
    run_installer > "$WORK/no-fallback.out" 2> "$WORK/no-fallback.err"; then
    printf '%s\n' 'disabled source fallback unexpectedly succeeded' >&2
    exit 1
fi
grep -F 'no tagged Dext release exists yet' "$WORK/no-fallback.err" >/dev/null

PHASE='attestation source rejection'
if MOCK_MODE=source DEXT_INSTALL_DIR="$WORK/install-attested" DEXT_REQUIRE_ATTESTATION=1 \
    run_installer > "$WORK/attested.out" 2> "$WORK/attested.err"; then
    printf '%s\n' 'attestation-required source fallback unexpectedly succeeded' >&2
    exit 1
fi
grep -F 'attestation verification requires a tagged release' "$WORK/attested.err" >/dev/null

PHASE='malformed API and version rejection'
for case in malformed-release bad-ref wrong-version; do
    if MOCK_MODE=$case DEXT_INSTALL_DIR="$WORK/install release" \
        run_installer > "$WORK/$case.out" 2> "$WORK/$case.err"; then
        printf '%s\n' "$case unexpectedly succeeded" >&2
        exit 1
    fi
done
grep -F 'latest release response did not contain' "$WORK/malformed-release.err" >/dev/null
grep -F 'main ref does not point to a commit' "$WORK/bad-ref.err" >/dev/null
grep -F 'release binary reported' "$WORK/wrong-version.err" >/dev/null
test "$("$WORK/install release/dext" --version)" = 'dext 1.2.3'
current_digest=$(sha256_file="$WORK/install release/dext"; if command -v sha256sum >/dev/null 2>&1; then sha256sum "$sha256_file" | awk '{print $1}'; else shasum -a 256 "$sha256_file" | awk '{print $1}'; fi)
test "$current_digest" = "$installed_digest"

PHASE='invalid environment toggle'
if MOCK_MODE=source DEXT_INSTALL_DIR="$WORK/install-invalid" DEXT_SOURCE_FALLBACK=yes \
    run_installer > "$WORK/invalid.out" 2> "$WORK/invalid.err"; then
    printf '%s\n' 'invalid source fallback toggle unexpectedly succeeded' >&2
    exit 1
fi
grep -F 'DEXT_SOURCE_FALLBACK must be 0 or 1' "$WORK/invalid.err" >/dev/null

PHASE='empty install directory rejection'
if run_installer --install-dir "" > "$WORK/empty-dir.out" 2> "$WORK/empty-dir.err"; then
    printf '%s\n' 'empty install directory unexpectedly succeeded' >&2
    exit 1
fi
grep -F -- '--install-dir requires a non-empty directory' "$WORK/empty-dir.err" >/dev/null

printf '%s\n' 'Unix installer tests passed'
