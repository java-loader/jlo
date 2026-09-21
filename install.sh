#!/usr/bin/env sh

set -eu

# Honour a JLO_HOME the user has already exported (from an earlier install, or
# just for this run) instead of installing somewhere they are not sourcing from.
# Note: JLO_HOME holds J'Lo's own files, not the JDKs - those go to the IntelliJ
# IDEA directory and are not configurable.
if [ -n "${JLO_HOME-}" ]; then
  JLO_HOME_PRESET=1
else
  JLO_HOME_PRESET=0
  JLO_HOME="$HOME/.jlo"
fi
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
#
# This is an optional convenience, so any failure here is a warning, not a fatal
# error. We only ever create or refresh a symlink that already points at our own
# binary; an unrelated file/dir/symlink at that path is left untouched.
JLO_TARGET="$JLO_BIN_DIR/jlo-bin"
JLO_LINK="$LOCAL_BIN_DIR/jlo"
JLO_LINKED=0
if [ ! -e "$JLO_LINK" ] && [ ! -L "$JLO_LINK" ]; then
  JLO_MAY_LINK=1
elif [ -L "$JLO_LINK" ] && [ "$(readlink "$JLO_LINK")" = "$JLO_TARGET" ]; then
  JLO_MAY_LINK=1
else
  JLO_MAY_LINK=0
  echo "Warning: '$JLO_LINK' already exists and is not managed by J'Lo; leaving it untouched." >&2
  echo "         To put 'jlo' on PATH yourself: ln -s '$JLO_TARGET' '$JLO_LINK'" >&2
fi
if [ "$JLO_MAY_LINK" = 1 ]; then
  if mkdir -p "$LOCAL_BIN_DIR" 2>/dev/null && ln -sf "$JLO_TARGET" "$JLO_LINK" 2>/dev/null; then
    JLO_LINKED=1
  else
    echo "Warning: could not create '$JLO_LINK'; 'jlo' may not be available in non-interactive shells." >&2
  fi
fi

# Shell completions. Generated once at install time rather than via
# 'source <(jlo completions bash)' in the profile, so shell startup costs no
# subprocess. A failure here is a convenience lost, not a broken install.
JLO_COMPLETION_DIR="$JLO_HOME/completions"
if mkdir -p "$JLO_COMPLETION_DIR" 2>/dev/null; then
  if "$JLO_TARGET" completions bash > "$JLO_COMPLETION_DIR/jlo.bash.tmp"; then
    mv -f "$JLO_COMPLETION_DIR/jlo.bash.tmp" "$JLO_COMPLETION_DIR/jlo.bash"
  else
    rm -f "$JLO_COMPLETION_DIR/jlo.bash.tmp"
    echo "Warning: could not generate bash completions." >&2
  fi
  if "$JLO_TARGET" completions zsh > "$JLO_COMPLETION_DIR/_jlo.tmp"; then
    mv -f "$JLO_COMPLETION_DIR/_jlo.tmp" "$JLO_COMPLETION_DIR/_jlo"
  else
    rm -f "$JLO_COMPLETION_DIR/_jlo.tmp"
    echo "Warning: could not generate zsh completions." >&2
  fi
else
  echo "Warning: could not create '$JLO_COMPLETION_DIR'; shell completions are unavailable." >&2
fi

# An already-exported JLO_HOME is the user's own profile line; telling them to
# add ours on top would either duplicate it or silently point elsewhere.
if [ "$JLO_HOME_PRESET" = 1 ]; then
  JLO_HOME_LINE="# JLO_HOME is already set to '$JLO_HOME' - keep your existing export."
else
  JLO_HOME_LINE="export JLO_HOME=\"\$HOME/.jlo\""
fi

cat <<EOF
Successfully installed J'Lo to $JLO_HOME.

*** IMPORTANT ***

Add the following lines to the end of your shell profile file (e.g., ~/.bashrc, ~/.zshrc):

$JLO_HOME_LINE
[[ -s "\$JLO_HOME/bin/jlo-init.sh" ]] && source "\$JLO_HOME/bin/jlo-init.sh"
[[ -s "\$JLO_HOME/bin/jlo-autoload.sh" ]] && source "\$JLO_HOME/bin/jlo-autoload.sh"

# Shell completions:
if [ -n "\$BASH_VERSION" ]; then
  [[ -s "\$JLO_HOME/completions/jlo.bash" ]] && source "\$JLO_HOME/completions/jlo.bash"
fi
if [ -n "\$ZSH_VERSION" ]; then
  (( \$+functions[compdef] )) || { autoload -Uz compinit && compinit -i; }
  [[ -s "\$JLO_HOME/completions/_jlo" ]] && source "\$JLO_HOME/completions/_jlo"
fi

Then restart your terminal or execute the above lines in your current shell session.

After that, you can use the 'jlo' command to manage your Java environments.
EOF

# Only nudge about PATH when we actually created the symlink and ~/.local/bin
# isn't already on PATH (usually the case on macOS; most Linux setups include it).
if [ "$JLO_LINKED" = 1 ]; then
  case ":${PATH-}:" in
    *":$LOCAL_BIN_DIR:"*) ;;
    *)
      cat <<EOF

Also add '$LOCAL_BIN_DIR' to your PATH so 'jlo' works in non-interactive
shells (CI, scripts, AI agents) and for 'jlo home':

export PATH="\$HOME/.local/bin:\$PATH"
EOF
      ;;
  esac
fi
