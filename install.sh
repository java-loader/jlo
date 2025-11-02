#!/usr/bin/env sh

set -eu

JLO_HOME="$HOME/.jlo"
JLO_BIN_DIR="$JLO_HOME/bin"
JLO_BASE_URL="https://github.com/java-loader/jlo/releases/download/stable"

OS="$(uname | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

if [ "$OS" = "linux" ]; then
  JLO_URL="$JLO_BASE_URL/jlo-linux-$ARCH.tar.gz"
elif [ "$OS" = "darwin" ]; then
  JLO_URL="$JLO_BASE_URL/jlo-macos-$ARCH.tar.gz"
else
  echo "Unsupported OS: $OS" >&2
  exit 1
fi

mkdir -p "$JLO_BIN_DIR"

JLO_BUNDLE="jlo.tar.gz"
FQ_JLO_BUNDLE="$JLO_BIN_DIR/$JLO_BUNDLE"
if ! curl -fsSL "$JLO_URL" -o "$FQ_JLO_BUNDLE"; then
  echo "Failed to download jlo binary from $JLO_URL" >&2
  rm -f "$FQ_JLO_BUNDLE"
  exit 1
fi

if ! tar -xzf "$FQ_JLO_BUNDLE" -C "$JLO_BIN_DIR"; then
  echo "Failed to extract jlo binary from $FQ_JLO_BUNDLE" >&2
  rm -f "$FQ_JLO_BUNDLE"
  exit 1
fi
rm -f "$FQ_JLO_BUNDLE"

cat <<EOF
Successfully installed J'Lo to $JLO_HOME.

*** IMPORTANT ***

Add the following lines to the end of your shell profile file (e.g., ~/.bashrc, ~/.zshrc):

export JLO_HOME="\$HOME/.jlo"
[[ -s "\$JLO_HOME/bin/jlo-init.sh" ]] && source "\$JLO_HOME/bin/jlo-init.sh"
[[ -s "\$JLO_HOME/bin/jlo-autoload.sh" ]] && source "\$JLO_HOME/bin/jlo-autoload.sh"

Then restart your terminal or execute the above lines in your current shell session.

After that, you can use the 'jlo' command to manage your Java environments.
EOF
