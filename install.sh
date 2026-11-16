#!/bin/sh
# shouji のインストーラー（GitHub Releases からバイナリを入れる）．
#
#   curl -fsSL https://raw.githubusercontent.com/arata/shouji/master/install.sh | sh
#
# 環境変数で変えられる:
#   SHOUJI_VERSION      入れるバージョン（例: v0.1.0．省略時は最新）
#   SHOUJI_INSTALL_DIR  置き場所（省略時は /usr/local/bin．書き込めなければ sudo を使う）
#   SHOUJI_BASE_URL     ダウンロード元（ミラーや手元での試験用．省略時は GitHub Releases）
#
# POSIX sh で書いている（bash でなくても動く）．全体を main にまとめ，
# 最後まで読み込んでから実行するので，ダウンロードが途中で切れても中途半端には動かない．

set -eu

REPO="arata/shouji"

say() {
    printf 'shouji-install: %s\n' "$*"
}

err() {
    printf 'shouji-install: error: %s\n' "$*" >&2
    exit 1
}

has() {
    command -v "$1" >/dev/null 2>&1
}

need() {
    has "$1" || err "this script needs '$1'"
}

# URL をファイルに保存する（curl か wget）．
download() {
    if has curl; then
        curl -fsSL --retry 3 -o "$2" "$1" || err "download failed: $1"
    elif has wget; then
        wget -q -O "$2" "$1" || err "download failed: $1"
    else
        err "this script needs curl or wget"
    fi
}

# チェックサムを確かめる（sha256sum か shasum．どちらもなければ省いて警告する）．
verify() {
    dir=$1
    file=$2
    if has sha256sum; then
        (cd "$dir" && sha256sum -c "$file.sha256" >/dev/null 2>&1) || err "checksum mismatch for $file"
    elif has shasum; then
        (cd "$dir" && shasum -a 256 -c "$file.sha256" >/dev/null 2>&1) || err "checksum mismatch for $file"
    else
        say "warning: no sha256sum or shasum, skipping the checksum"
    fi
}

main() {
    need uname
    need tar
    need mktemp

    version=${SHOUJI_VERSION:-latest}
    install_dir=${SHOUJI_INSTALL_DIR:-/usr/local/bin}
    base_url=${SHOUJI_BASE_URL:-}

    os=$(uname -s)
    arch=$(uname -m)
    if [ "$os" != "Linux" ]; then
        err "prebuilt binaries are only for Linux (this is $os). Build from source: cargo install --git https://github.com/$REPO"
    fi
    case $arch in
        x86_64 | amd64) target=x86_64-unknown-linux-musl ;;
        aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
        *) err "no prebuilt binary for this CPU ($arch). Build from source: cargo install --git https://github.com/$REPO" ;;
    esac

    if [ -z "$base_url" ]; then
        if [ "$version" = "latest" ]; then
            base_url="https://github.com/$REPO/releases/latest/download"
        else
            case $version in
                v*) ;;
                *) version="v$version" ;;
            esac
            base_url="https://github.com/$REPO/releases/download/$version"
        fi
    fi

    asset="shouji-$target.tar.gz"
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    trap 'rm -rf "$tmp"; exit 1' INT TERM

    say "downloading $asset ($version)"
    download "$base_url/$asset" "$tmp/$asset"
    download "$base_url/$asset.sha256" "$tmp/$asset.sha256"
    verify "$tmp" "$asset"

    tar -xzf "$tmp/$asset" -C "$tmp"
    bin="$tmp/shouji-$target/shouji"
    [ -f "$bin" ] || err "shouji is missing in $asset"

    # 置き場所に書き込めなければ sudo を使う
    sudo=""
    if ! { [ -d "$install_dir" ] && [ -w "$install_dir" ]; } && ! { [ ! -e "$install_dir" ] && mkdir -p "$install_dir" 2>/dev/null; }; then
        if [ "$(id -u)" != "0" ] && has sudo; then
            say "$install_dir needs root: using sudo"
            sudo=sudo
        else
            err "cannot write to $install_dir (try SHOUJI_INSTALL_DIR=\$HOME/.local/bin)"
        fi
    fi
    $sudo mkdir -p "$install_dir"
    # 一時ファイルに置いてから入れ替える（動いている shouji があっても壊さない）
    $sudo cp "$bin" "$install_dir/.shouji.tmp"
    $sudo chmod 755 "$install_dir/.shouji.tmp"
    $sudo mv -f "$install_dir/.shouji.tmp" "$install_dir/shouji"

    installed=$("$install_dir/shouji" --version 2>/dev/null) || err "installed, but $install_dir/shouji does not run"
    say "installed $installed to $install_dir/shouji"

    case ":$PATH:" in
        *":$install_dir:"*) ;;
        *) say "note: $install_dir is not in your PATH. Add this to your shell's rc file:"
           say "  export PATH=\"$install_dir:\$PATH\"" ;;
    esac
    say "get started: shouji --help"
}

main "$@"
