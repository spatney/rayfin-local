#!/bin/sh
# Installs rayfin-local from its GitHub releases, on macOS and Linux:
#
#   curl -fsSL https://raw.githubusercontent.com/spatney/rayfin-local/main/install.sh | sh
#
# Environment variables:
#   RAYFIN_LOCAL_VERSION      the version to install, such as 0.1.0 (default: the latest release)
#   RAYFIN_LOCAL_INSTALL_DIR  the folder for the binary (default: ~/.local/bin)
#   RAYFIN_LOCAL_REPO         the GitHub repository to install from (default: spatney/rayfin-local)
#
# The download is checked against the release's SHA256SUMS.txt. Run the script again to update.

set -eu

REPO="${RAYFIN_LOCAL_REPO:-spatney/rayfin-local}"
VERSION="${RAYFIN_LOCAL_VERSION:-}"
INSTALL_DIR="${RAYFIN_LOCAL_INSTALL_DIR:-}"

say() {
    printf '%s\n' "$*"
}

fail() {
    printf 'rayfin-local install: %s\n' "$*" >&2
    exit 1
}

has() {
    command -v "$1" >/dev/null 2>&1
}

from_source() {
    printf 'build it from source instead: cargo install --locked --git https://github.com/%s rayfin-local' "$REPO"
}

# download URL FILE
download() {
    if has curl; then
        curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$2" "$1"
    else
        wget -q -O "$2" "$1"
    fi
}

latest_version() {
    # /releases/latest redirects to /releases/tag/v<version>; following it avoids the API's rate limit.
    if has curl; then
        url="$(curl -fsSIL --proto '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' \
            "https://github.com/$REPO/releases/latest" 2>/dev/null)" || url=""
        case "$url" in
            */releases/tag/v*)
                printf '%s\n' "${url##*/releases/tag/v}"
                return 0
                ;;
        esac
    fi
    json="$TMP_DIR/latest.json"
    download "https://api.github.com/repos/$REPO/releases/latest" "$json" 2>/dev/null ||
        fail "could not find the latest release of $REPO (https://github.com/$REPO/releases)"
    tag="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' "$json" | head -n 1)"
    [ -n "$tag" ] || fail "could not read the latest release of $REPO"
    printf '%s\n' "$tag"
}

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Linux) os=unknown-linux-gnu ;;
        Darwin) os=apple-darwin ;;
        MINGW* | MSYS* | CYGWIN*)
            fail "on Windows, install with PowerShell: irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex"
            ;;
        *) fail "there is no prebuilt rayfin-local for $os; $(from_source)" ;;
    esac
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        arm64 | aarch64) arch=aarch64 ;;
        *) fail "there is no prebuilt rayfin-local for $arch processors; $(from_source)" ;;
    esac
    # A shell running under Rosetta 2 reports x86_64 on Apple silicon: install the native binary.
    if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] &&
        [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
        arch=aarch64
    fi
    # The Linux binaries need glibc; musl-based distributions such as Alpine can't run them.
    if [ "$os" = unknown-linux-gnu ] && ldd --version 2>&1 | grep -qi musl; then
        fail "the prebuilt rayfin-local needs glibc, and this system uses musl; $(from_source)"
    fi
    printf '%s-%s\n' "$arch" "$os"
}

# verify FILE CHECKSUMS
verify() {
    name="$(basename "$1")"
    expected="$(awk -v f="$name" '$2 == f || $2 == "*" f { print $1; exit }' "$2")"
    [ -n "$expected" ] || fail "SHA256SUMS.txt has no checksum for $name"
    if has sha256sum; then
        actual="$(sha256sum "$1" | awk '{ print $1 }')"
    elif has shasum; then
        actual="$(shasum -a 256 "$1" | awk '{ print $1 }')"
    elif has openssl; then
        actual="$(openssl dgst -sha256 "$1" | awk '{ print $NF }')"
    else
        fail "sha256sum, shasum or openssl is needed to check the download"
    fi
    [ "$actual" = "$expected" ] || fail "the checksum of $name doesn't match SHA256SUMS.txt (expected $expected, got $actual)"
}

path_hint() {
    case ":${PATH:-}:" in
        *":$INSTALL_DIR:"*) return 0 ;;
    esac
    say ""
    say "$INSTALL_DIR is not on your PATH. Add it with:"
    case "${SHELL:-}" in
        */zsh) say "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc && source ~/.zshrc" ;;
        */bash) say "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.bashrc && source ~/.bashrc" ;;
        */fish) say "  fish_add_path $INSTALL_DIR" ;;
        *) say "  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
    esac
}

main() {
    has curl || has wget || fail "curl or wget is needed to download rayfin-local"
    for tool in uname tar awk sed mktemp; do
        has "$tool" || fail "$tool is needed to install rayfin-local"
    done
    if [ -z "$INSTALL_DIR" ]; then
        [ -n "${HOME:-}" ] || fail "HOME is not set; set RAYFIN_LOCAL_INSTALL_DIR to the folder for the binary"
        INSTALL_DIR="$HOME/.local/bin"
    fi

    target="$(detect_target)"
    TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t rayfin-local)"
    trap 'rm -rf "$TMP_DIR"; rm -f "$INSTALL_DIR/.rayfin-local.new"' EXIT
    trap 'exit 130' INT TERM

    [ -n "$VERSION" ] || VERSION="$(latest_version)"
    VERSION="${VERSION#v}"
    archive="rayfin-local-$VERSION-$target.tar.gz"
    base="https://github.com/$REPO/releases/download/v$VERSION"

    say "Downloading rayfin-local $VERSION for $target..."
    download "$base/$archive" "$TMP_DIR/$archive" ||
        fail "could not download $base/$archive; check that version $VERSION exists at https://github.com/$REPO/releases"
    download "$base/SHA256SUMS.txt" "$TMP_DIR/SHA256SUMS.txt" ||
        fail "could not download $base/SHA256SUMS.txt"
    verify "$TMP_DIR/$archive" "$TMP_DIR/SHA256SUMS.txt"

    tar -xzf "$TMP_DIR/$archive" -C "$TMP_DIR"
    binary="$TMP_DIR/rayfin-local-$VERSION-$target/rayfin-local"
    [ -f "$binary" ] || fail "$archive has no rayfin-local binary"

    mkdir -p "$INSTALL_DIR"
    new="$INSTALL_DIR/.rayfin-local.new"
    cp "$binary" "$new"
    chmod 755 "$new"
    # Run the new binary before it replaces anything, so a system that can't run it keeps what it
    # has. It runs from the install folder because temporary folders may forbid running programs.
    if ! installed="$("$new" --version 2>&1)"; then
        rm -f "$new"
        fail "the downloaded rayfin-local doesn't run on this system ($installed). On Linux it needs glibc 2.34 or later; $(from_source)"
    fi
    # Renaming over the old binary is atomic, and a running rayfin-local keeps working.
    mv -f "$new" "$INSTALL_DIR/rayfin-local"
    say "Installed $installed to $INSTALL_DIR/rayfin-local"
    path_hint
    say ""
    say "Next, in a Rayfin project: npm install, then rayfin-local serve"
}

main "$@"
