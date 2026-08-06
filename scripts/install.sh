#!/bin/sh
set -eu

REPOSITORY="https://github.com/SiliconState/Dext"
REPOSITORY_SLUG="SiliconState/Dext"
API_URL="https://api.github.com/repos/$REPOSITORY_SLUG/releases/latest"
MAIN_COMMIT_URL="https://api.github.com/repos/$REPOSITORY_SLUG/git/ref/heads/main"
INSTALL_DIR=${DEXT_INSTALL_DIR:-}
REQUESTED_VERSION=${DEXT_VERSION:-latest}
SOURCE_FALLBACK=${DEXT_SOURCE_FALLBACK:-1}
REQUIRE_ATTESTATION=${DEXT_REQUIRE_ATTESTATION:-0}
TEMP_DIR=
STAGED_BINARY=
INSTALLED_VERSION=

say() {
    printf '%s\n' "dext-install: $*"
}

fail() {
    printf '%s\n' "dext-install: error: $*" >&2
    exit 1
}

cleanup() {
    if [ -n "$STAGED_BINARY" ]; then
        rm -f -- "$STAGED_BINARY"
    fi
    if [ -n "$TEMP_DIR" ]; then
        rm -rf -- "$TEMP_DIR"
    fi
}
trap cleanup EXIT HUP INT TERM

usage() {
    cat <<'EOF'
Install Dext for the current user.

Usage: install.sh [--version vX.Y.Z] [--install-dir DIR] [--no-source-fallback] [--require-attestation]

Environment:
  DEXT_VERSION              Release tag to install (default: latest)
  DEXT_INSTALL_DIR          Binary directory (default: ~/.local/bin)
  DEXT_SOURCE_FALLBACK      Set to 0 to fail when no tagged release exists
  DEXT_REQUIRE_ATTESTATION  Set to 1 to require GitHub CLI provenance verification
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || fail "--version requires vX.Y.Z"
            REQUESTED_VERSION=$2
            shift 2
            ;;
        --install-dir)
            [ "$#" -ge 2 ] || fail "--install-dir requires a directory"
            [ -n "$2" ] || fail "--install-dir requires a non-empty directory"
            INSTALL_DIR=$2
            shift 2
            ;;
        --no-source-fallback)
            SOURCE_FALLBACK=0
            shift
            ;;
        --require-attestation)
            REQUIRE_ATTESTATION=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown option: $1"
            ;;
    esac
done

case "$SOURCE_FALLBACK" in
    0|1) ;;
    *) fail "DEXT_SOURCE_FALLBACK must be 0 or 1" ;;
esac
case "$REQUIRE_ATTESTATION" in
    0|1) ;;
    *) fail "DEXT_REQUIRE_ATTESTATION must be 0 or 1" ;;
esac

if [ -z "$INSTALL_DIR" ]; then
    [ -n "${HOME:-}" ] || fail "HOME is not set; pass --install-dir or DEXT_INSTALL_DIR"
    INSTALL_DIR="$HOME/.local/bin"
fi
command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v mktemp >/dev/null 2>&1 || fail "mktemp is required"
TEMP_DIR=$(mktemp -d 2>/dev/null || mktemp -d -t dext-install)

validate_tag() {
    printf '%s\n' "$1" | awk '/^v[0-9]+\.[0-9]+\.[0-9]+$/ { valid = 1 } END { exit valid ? 0 : 1 }'
}

latest_tag() {
    response="$TEMP_DIR/latest.json"
    tag_file="$TEMP_DIR/latest-tag"
    if ! status=$(curl --proto '=https' --tlsv1.2 -sSL \
        -H 'Accept: application/vnd.github+json' \
        -H 'User-Agent: dext-installer' \
        -o "$response" -w '%{http_code}' "$API_URL"); then
        printf '%s\n' "dext-install: error: could not query the latest GitHub release" >&2
        return 2
    fi
    case "$status" in
        200)
            sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$response" \
                | awk 'NF { values[++count] = $0 } END { if (count != 1) exit 1; print values[1] }' \
                > "$tag_file" \
                || {
                    printf '%s\n' "dext-install: error: latest release response did not contain exactly one tag" >&2
                    return 2
                }
            [ -s "$tag_file" ] || {
                printf '%s\n' "dext-install: error: latest release response did not contain a tag" >&2
                return 2
            }
            ;;
        404)
            return 1
            ;;
        *)
            printf '%s\n' "dext-install: error: GitHub release lookup returned HTTP $status" >&2
            return 2
            ;;
    esac
}

main_commit() {
    response="$TEMP_DIR/main-commit.json"
    commit_file="$TEMP_DIR/main-commit"
    type_file="$TEMP_DIR/main-type"
    if ! status=$(curl --proto '=https' --tlsv1.2 -sSL \
        -H 'Accept: application/vnd.github+json' \
        -H 'User-Agent: dext-installer' \
        -o "$response" -w '%{http_code}' "$MAIN_COMMIT_URL"); then
        fail "could not resolve the current main commit"
    fi
    [ "$status" = 200 ] || fail "main commit lookup returned HTTP $status"
    sed -n 's/.*"type"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$response" \
        | awk 'NF { values[++count] = $0 } END { if (count != 1) exit 1; print values[1] }' \
        > "$type_file" \
        || fail "main ref response is malformed"
    [ "$(cat "$type_file")" = commit ] || fail "main ref does not point to a commit"
    sed -n 's/.*"sha"[[:space:]]*:[[:space:]]*"\([0-9A-Fa-f]*\)".*/\1/p' "$response" \
        | awk 'NF { values[++count] = tolower($0) } END { if (count != 1) exit 1; print values[1] }' \
        > "$commit_file" \
        || fail "main commit response is malformed"
    commit=$(cat "$commit_file")
    case "$commit" in
        *[!0-9a-f]*) fail "main commit response is malformed" ;;
    esac
    [ "${#commit}" -eq 40 ] || fail "main commit response is malformed"
    printf '%s\n' "$commit"
}

release_target() {
    os=$(uname -s 2>/dev/null || printf unknown)
    arch=$(uname -m 2>/dev/null || printf unknown)
    case "$os:$arch" in
        Linux:x86_64|Linux:amd64)
            printf '%s\n' x86_64-unknown-linux-gnu
            ;;
        Darwin:x86_64|Darwin:amd64)
            printf '%s\n' x86_64-apple-darwin
            ;;
        Darwin:arm64|Darwin:aarch64)
            printf '%s\n' aarch64-apple-darwin
            ;;
        *)
            return 1
            ;;
    esac
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        fail "sha256sum or shasum is required to verify a release"
    fi
}

verify_binary() {
    source_binary=$1
    expected_version=${2:-}
    [ -f "$source_binary" ] && [ ! -L "$source_binary" ] \
        || fail "installer did not produce a regular Dext binary"
    reported=$("$source_binary" --version) \
        || fail "installed Dext candidate did not start"
    case "$reported" in
        "dext "*)
            version=${reported#"dext "}
            case "$version" in
                ""|*[[:space:]]*) fail "installed Dext candidate returned an unexpected version string" ;;
            esac
            ;;
        *) fail "installed Dext candidate returned an unexpected version string" ;;
    esac
    if [ -n "$expected_version" ] && [ "$reported" != "dext $expected_version" ]; then
        fail "release binary reported '$reported', expected 'dext $expected_version'"
    fi
    printf '%s\n' "$reported"
}

install_binary() {
    source_binary=$1
    expected_version=${2:-}
    [ -f "$source_binary" ] && [ ! -L "$source_binary" ] \
        || fail "installer did not produce a regular Dext binary"
    mkdir -p "$INSTALL_DIR"
    STAGED_BINARY=$(mktemp "$INSTALL_DIR/.dext-install.XXXXXX")
    cp "$source_binary" "$STAGED_BINARY"
    chmod 755 "$STAGED_BINARY"
    verify_binary "$STAGED_BINARY" "$expected_version" >/dev/null
    mv -f "$STAGED_BINARY" "$INSTALL_DIR/dext"
    STAGED_BINARY=
}

install_release() {
    tag=$1
    target=$2
    archive="dext-${tag}-${target}.tar.gz"
    base="$REPOSITORY/releases/download/$tag"
    say "downloading $archive"
    curl --proto '=https' --tlsv1.2 -fL --retry 3 --retry-delay 1 \
        -o "$TEMP_DIR/$archive" "$base/$archive"
    curl --proto '=https' --tlsv1.2 -fL --retry 3 --retry-delay 1 \
        -o "$TEMP_DIR/SHA256SUMS" "$base/SHA256SUMS"

    expected=$(awk -v name="$archive" '
        $2 == name { digest = tolower($1); matches++ }
        END { if (matches != 1) exit 1; print digest }
    ' "$TEMP_DIR/SHA256SUMS") || fail "SHA256SUMS does not contain exactly one entry for $archive"
    case "$expected" in
        *[!0-9A-Fa-f]*) fail "release checksum is malformed" ;;
    esac
    [ "${#expected}" -eq 64 ] || fail "release checksum is malformed"
    actual=$(sha256_file "$TEMP_DIR/$archive")
    [ "$actual" = "$expected" ] || fail "checksum verification failed for $archive"
    if [ "$REQUIRE_ATTESTATION" = 1 ]; then
        command -v gh >/dev/null 2>&1 \
            || fail "GitHub CLI is required by DEXT_REQUIRE_ATTESTATION=1"
        gh attestation verify "$TEMP_DIR/$archive" --repo "$REPOSITORY_SLUG" >/dev/null \
            || fail "GitHub build-provenance verification failed for $archive"
        say "verified release checksum and GitHub build provenance"
    else
        say "verified release checksum"
    fi

    command -v tar >/dev/null 2>&1 || fail "tar is required to unpack a release"
    mkdir "$TEMP_DIR/unpacked"
    tar -tzf "$TEMP_DIR/$archive" > "$TEMP_DIR/archive-list"
    awk '
        /^\// || /(^|\/)\.\.(\/|$)/ { unsafe = 1 }
        $0 == "dext" { binaries++ }
        END { if (unsafe || binaries != 1) exit 1 }
    ' "$TEMP_DIR/archive-list" || fail "release archive has an unsafe or unexpected layout"
    tar -xzf "$TEMP_DIR/$archive" -C "$TEMP_DIR/unpacked" dext
    INSTALLED_VERSION=${tag#v}
    install_binary "$TEMP_DIR/unpacked/dext" "$INSTALLED_VERSION"
    say "verified and installed release $tag"
}

install_source() {
    [ "$REQUIRE_ATTESTATION" != 1 ] \
        || fail "attestation verification requires a tagged release; source fallback is disabled"
    command -v cargo >/dev/null 2>&1 \
        || fail "no tagged release exists yet and Rust/Cargo is unavailable; install Rust or set DEXT_VERSION to an existing release"
    commit=$(main_commit)
    say "building Dext from main commit $commit with Cargo"
    cargo install --git "$REPOSITORY.git" --rev "$commit" --locked --root "$TEMP_DIR/cargo-root" dext
    install_binary "$TEMP_DIR/cargo-root/bin/dext"
}

TAG=
if [ "$REQUESTED_VERSION" = latest ]; then
    if latest_tag; then
        TAG=$(cat "$TEMP_DIR/latest-tag")
    else
        latest_status=$?
        if [ "$latest_status" -ne 1 ]; then
            exit "$latest_status"
        fi
        if [ "$SOURCE_FALLBACK" = 1 ]; then
            install_source
        else
            fail "no tagged Dext release exists yet"
        fi
    fi
else
    case "$REQUESTED_VERSION" in
        v*) TAG=$REQUESTED_VERSION ;;
        *) TAG="v$REQUESTED_VERSION" ;;
    esac
fi

if [ -n "$TAG" ]; then
    validate_tag "$TAG" || fail "release version must have form vX.Y.Z"
    if TARGET=$(release_target); then
        install_release "$TAG" "$TARGET"
    elif [ "$REQUESTED_VERSION" = latest ] && [ "$SOURCE_FALLBACK" = 1 ]; then
        say "no prebuilt archive matches this platform; falling back to a source build"
        install_source
    else
        fail "no Dext release archive matches $(uname -s 2>/dev/null || printf unknown)/$(uname -m 2>/dev/null || printf unknown)"
    fi
fi

verify_binary "$INSTALL_DIR/dext" "$INSTALLED_VERSION"
case ":${PATH:-}:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        say "add $INSTALL_DIR to PATH to run dext from a new shell"
        ;;
esac
say "done"
