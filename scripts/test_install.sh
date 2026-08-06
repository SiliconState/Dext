#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d 2>/dev/null || mktemp -d -t dext-install-test)
trap 'rm -rf "$WORK"' EXIT HUP INT TERM
unset DEXT_INSTALL_DIR DEXT_VERSION DEXT_SOURCE_FALLBACK DEXT_REQUIRE_ATTESTATION
unset MOCK_MODE MOCK_CHECKSUMS MOCK_CARGO_ARGS

mkdir -p "$WORK/bin" "$WORK/release" "$WORK/package"
cat > "$WORK/package/dext" <<'EOF'
#!/bin/sh
printf '%s\n' 'dext 1.2.3'
EOF
chmod 755 "$WORK/package/dext"
tar -C "$WORK/package" -czf "$WORK/release/dext-v1.2.3-x86_64-unknown-linux-gnu.tar.gz" dext
ARCHIVE="$WORK/release/dext-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"
if command -v sha256sum >/dev/null 2>&1; then
    DIGEST=$(sha256sum "$ARCHIVE" | awk '{print toupper($1)}')
else
    DIGEST=$(shasum -a 256 "$ARCHIVE" | awk '{print toupper($1)}')
fi
printf '%s  %s\n' "$DIGEST" "dext-v1.2.3-x86_64-unknown-linux-gnu.tar.gz" > "$WORK/release/SHA256SUMS"

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
        if [ "${MOCK_MODE:-release}" = source ]; then
            status=404
            printf '%s\n' '{"message":"Not Found"}' > "$out"
        else
            printf '%s\n' '{"tag_name":"v1.2.3"}' > "$out"
        fi
        ;;
    */git/ref/heads/main)
        printf '%s\n' '{"ref":"refs/heads/main","object":{"type":"commit","sha":"0123456789ABCDEF0123456789ABCDEF01234567"}}' > "$out"
        ;;
    */dext-v1.2.3-x86_64-unknown-linux-gnu.tar.gz)
        cp "$MOCK_RELEASE/dext-v1.2.3-x86_64-unknown-linux-gnu.tar.gz" "$out"
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
    MOCK_RELEASE="$WORK/release" \
    MOCK_CHECKSUMS="${MOCK_CHECKSUMS:-$WORK/release/SHA256SUMS}" \
    MOCK_CARGO_ARGS="$WORK/cargo.args" \
    PATH="$WORK/bin:$PATH" \
    sh "$ROOT/scripts/install.sh" "$@"
}

release_out=$(DEXT_INSTALL_DIR="$WORK/install release" run_installer)
printf '%s\n' "$release_out" | grep -F 'verified and installed release v1.2.3' >/dev/null
test "$("$WORK/install release/dext" --version)" = 'dext 1.2.3'

printf '%064d  %s\n' 0 'dext-v1.2.3-x86_64-unknown-linux-gnu.tar.gz' > "$WORK/release/BADSUMS"
if DEXT_INSTALL_DIR="$WORK/install-bad" MOCK_CHECKSUMS="$WORK/release/BADSUMS" \
    run_installer > "$WORK/bad.out" 2> "$WORK/bad.err"; then
    printf '%s\n' 'checksum mismatch unexpectedly succeeded' >&2
    exit 1
fi
grep -F 'checksum verification failed' "$WORK/bad.err" >/dev/null
test ! -e "$WORK/install-bad/dext"

source_out=$(MOCK_MODE=source DEXT_INSTALL_DIR="$WORK/install-source" run_installer)
printf '%s\n' "$source_out" \
    | grep -F 'main commit 0123456789abcdef0123456789abcdef01234567' >/dev/null
grep -F -- '--rev 0123456789abcdef0123456789abcdef01234567 --locked' \
    "$WORK/cargo.args" >/dev/null
test "$("$WORK/install-source/dext" --version)" = 'dext 9.9.9'

if MOCK_MODE=source DEXT_INSTALL_DIR="$WORK/install-no-fallback" DEXT_SOURCE_FALLBACK=0 \
    run_installer > "$WORK/no-fallback.out" 2> "$WORK/no-fallback.err"; then
    printf '%s\n' 'disabled source fallback unexpectedly succeeded' >&2
    exit 1
fi
grep -F 'no tagged Dext release exists yet' "$WORK/no-fallback.err" >/dev/null

if MOCK_MODE=source DEXT_INSTALL_DIR="$WORK/install-attested" DEXT_REQUIRE_ATTESTATION=1 \
    run_installer > "$WORK/attested.out" 2> "$WORK/attested.err"; then
    printf '%s\n' 'attestation-required source fallback unexpectedly succeeded' >&2
    exit 1
fi
grep -F 'attestation verification requires a tagged release' "$WORK/attested.err" >/dev/null

if MOCK_MODE=source DEXT_INSTALL_DIR="$WORK/install-invalid" DEXT_SOURCE_FALLBACK=yes \
    run_installer > "$WORK/invalid.out" 2> "$WORK/invalid.err"; then
    printf '%s\n' 'invalid source fallback toggle unexpectedly succeeded' >&2
    exit 1
fi
grep -F 'DEXT_SOURCE_FALLBACK must be 0 or 1' "$WORK/invalid.err" >/dev/null

printf '%s\n' 'Unix installer tests passed'
