#!/usr/bin/env sh

set -eu

JLO_HOME="$HOME/.jlo"
JLO_BIN_DIR="$JLO_HOME/bin"
LOCAL_BIN_DIR="$HOME/.local/bin"
JLO_BASE_URL="https://github.com/java-loader/jlo/releases/latest/download"

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

# Expose a real 'jlo' on PATH for non-interactive shells (CI, scripts, agents).
# The interactive shell function from jlo-init.sh still shadows this symlink and
# keeps handling env/use, which must mutate the current shell.
mkdir -p "$LOCAL_BIN_DIR"
ln -sf "$JLO_BIN_DIR/jlo-bin" "$LOCAL_BIN_DIR/jlo"

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

# Only nudge about PATH when ~/.local/bin isn't already on it (usually the case
# on macOS; most Linux setups already include it).
case ":$PATH:" in
  *":$LOCAL_BIN_DIR:"*) ;;
  *)
    cat <<EOF

Also add '$LOCAL_BIN_DIR' to your PATH so 'jlo' works in non-interactive
shells (CI, scripts, AI agents) and for 'jlo home':

export PATH="\$HOME/.local/bin:\$PATH"
EOF
    ;;
esac
