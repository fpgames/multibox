#!/bin/sh
# Installs git-multibox on macOS:
#
#   curl -fsSL https://raw.githubusercontent.com/fpgames/multibox/main/install.sh | sh
#
# It downloads the latest release, checks it against SHA256SUMS and puts it in
# ~/.local/bin. MULTIBOX_VERSION picks another release, such as v0.1.1, and
# MULTIBOX_INSTALL_DIR another directory.
set -eu

repo=fpgames/multibox
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) target=aarch64-apple-darwin ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    *)
        echo "There is no prebuilt git-multibox for $(uname -s) $(uname -m) yet. With Rust installed:" >&2
        echo "  cargo install --locked --git https://github.com/$repo git-multibox" >&2
        exit 1
        ;;
esac

version=${MULTIBOX_VERSION:-}
if [ -z "$version" ]; then
    version=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" |
        sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
fi
if [ -z "$version" ]; then
    echo "Could not find the latest release of $repo." >&2
    exit 1
fi
dir=${MULTIBOX_INSTALL_DIR:-$HOME/.local/bin}
name="git-multibox-$version-$target"
base="https://github.com/$repo/releases/download/$version"

temp=$(mktemp -d)
trap 'rm -rf "$temp"' EXIT
curl -fsSL -o "$temp/$name.tar.gz" "$base/$name.tar.gz"
curl -fsSL -o "$temp/SHA256SUMS" "$base/SHA256SUMS"
if ! (cd "$temp" && grep "  $name.tar.gz\$" SHA256SUMS | shasum -a 256 -c - >/dev/null 2>&1); then
    echo "$name.tar.gz does not match SHA256SUMS; nothing was installed." >&2
    exit 1
fi
tar -xzf "$temp/$name.tar.gz" -C "$temp"
mkdir -p "$dir"
install -m 755 "$temp/$name/git-multibox" "$dir/git-multibox"

echo "Installed $("$dir/git-multibox" --version) in $dir"
case ":$PATH:" in
    *":$dir:"*) ;;
    *)
        echo "Put $dir on PATH so that git finds it as 'git multibox', for example:"
        echo "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.zshrc && exec zsh"
        ;;
esac
found=$(command -v git-multibox || true)
if [ -n "$found" ] && [ "$found" != "$dir/git-multibox" ]; then
    echo "Note: $found comes first on PATH, so git will run that one instead."
fi
echo "Next, inside a repository: git multibox doctor"
