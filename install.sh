#!/usr/bin/env sh

set -eu

# Honour a JLO_HOME the user has already exported (from an earlier install, or
# just for this run) instead of installing somewhere they are not sourcing from.
# Note: JLO_HOME holds J'Lo's own files, not the JDKs - those go to the IntelliJ
# IDEA directory and are not configurable.
if [ -z "${JLO_HOME-}" ]; then
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

# Everything the profile needs lives in generated entry files rather than in
# lines the user pastes. Two reasons. A pasted block restates internal paths, so
# it goes stale the moment the layout changes and every existing user has to
# re-paste. And on a re-install the block is usually sourced already, which used
# to make 'JLO_HOME is exported' look like 'the profile exports JLO_HOME' - it
# was the old block's own export, which the paste then replaced, leaving
# JLO_HOME empty and every source line below it a silent no-op.
#
# So: the paths are baked in here, once, and the profile only sources them.
#
# Baked paths are single-quoted so the shell sourcing them treats a '$' or a
# backtick in a path as data. An apostrophe would still close the quote early
# and produce a file that does not parse, so jlo_squote emits the POSIX
# escape - close the quote, an escaped apostrophe, reopen - and returns the
# surrounding quotes with it.
jlo_squote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# Writes stdin to "$1" via a temp file, and *reports* failure: a caller that
# cannot write the required entry file must not let the install claim success.
jlo_write() {
  jlo_dest="$1"
  if cat > "$jlo_dest.tmp" && mv -f "$jlo_dest.tmp" "$jlo_dest"; then
    return 0
  fi
  rm -f "$jlo_dest.tmp"
  return 1
}

JLO_HOME_Q="$(jlo_squote "$JLO_HOME")"
JLO_INIT_Q="$(jlo_squote "$JLO_BIN_DIR/jlo-init.sh")"
JLO_AUTOLOAD_Q="$(jlo_squote "$JLO_BIN_DIR/jlo-autoload.sh")"
JLO_COMP_BASH_Q="$(jlo_squote "$JLO_COMPLETION_DIR/jlo.bash")"
JLO_COMP_ZSH_Q="$(jlo_squote "$JLO_COMPLETION_DIR/_jlo")"

# The one file a user cannot skip: the wrapper function, and the JLO_HOME the
# rest of the layout hangs off.
jlo_write "$JLO_HOME/jlo.sh" <<EOF || {
# Generated by J'Lo's installer - edits are lost on the next install or
# 'jlo selfupdate'. Source this from your shell profile.
export JLO_HOME=$JLO_HOME_Q
if [ -s $JLO_INIT_Q ]; then
  . $JLO_INIT_Q
fi
EOF
  echo "Failed to write '$JLO_HOME/jlo.sh'; J'Lo cannot be loaded from your profile." >&2
  exit 1
}

# Optional. Guarded on the wrapper existing rather than on JLO_HOME, because
# what jlo_after_cd actually calls is the 'jlo' function: sourcing this without
# jlo.sh has to be inert, not a stream of errors on every cd. 'typeset -f' is
# the test that needs no subshell; under a shell that lacks it the guard fails
# closed, which is the right answer there anyway.
jlo_write "$JLO_HOME/autoload.sh" <<EOF ||
# Generated by J'Lo's installer - edits are lost on the next install or
# 'jlo selfupdate'. Optional: switches JDK on cd when a .jlorc is in scope.
# Source this after jlo.sh.
if typeset -f jlo >/dev/null 2>&1 && [ -s $JLO_AUTOLOAD_Q ]; then
  . $JLO_AUTOLOAD_Q
fi
EOF
  echo "Warning: could not write '$JLO_HOME/autoload.sh'; cd autoloading is unavailable." >&2

# Optional. The zsh half is wrapped in 'eval' so this file still parses under a
# POSIX sh: '(( ... ))' is a syntax error there, and a profile that sources this
# unconditionally would die on the parse before the $ZSH_VERSION test ever ran.
jlo_write "$JLO_HOME/completions.sh" <<EOF ||
# Generated by J'Lo's installer - edits are lost on the next install or
# 'jlo selfupdate'. Optional: tab completion for the 'jlo' command.
if [ -n "\${BASH_VERSION-}" ] && [ -s $JLO_COMP_BASH_Q ]; then
  . $JLO_COMP_BASH_Q
fi
if [ -n "\${ZSH_VERSION-}" ] && [ -s $JLO_COMP_ZSH_Q ]; then
  eval 'if (( ! \$+functions[compdef] )); then autoload -Uz compinit && compinit -i; fi'
  . $JLO_COMP_ZSH_Q
fi
EOF
  echo "Warning: could not write '$JLO_HOME/completions.sh'; tab completion is unavailable." >&2

# A default install prints "$HOME/.jlo" rather than the expanded path, so the
# profile line stays portable across machines and users - the generated files
# it points at hold the real paths. "$HOME" is the one thing here meant to be
# expanded by the reader's shell; a custom JLO_HOME is quoted like any other
# baked path, so a '$' in it stays a '$'.
jlo_snippet_path() {
  if [ "$JLO_HOME" = "$HOME/.jlo" ]; then
    # shellcheck disable=SC2016 # literal on purpose: the user's shell expands it.
    printf '"$HOME/.jlo/%s"' "$1"
  else
    jlo_squote "$JLO_HOME/$1"
  fi
}
JLO_SNIPPET_MAIN="$(jlo_snippet_path jlo.sh)"
JLO_SNIPPET_AUTOLOAD="$(jlo_snippet_path autoload.sh)"
JLO_SNIPPET_COMPLETIONS="$(jlo_snippet_path completions.sh)"

cat <<EOF
Successfully installed J'Lo to $JLO_HOME.

*** IMPORTANT ***

Add this to the end of your shell profile (e.g. ~/.bashrc, ~/.zshrc):

[ -s $JLO_SNIPPET_MAIN ] && . $JLO_SNIPPET_MAIN

Optional, add either or both:

[ -s $JLO_SNIPPET_AUTOLOAD ] && . $JLO_SNIPPET_AUTOLOAD          # switch JDK on cd
[ -s $JLO_SNIPPET_COMPLETIONS ] && . $JLO_SNIPPET_COMPLETIONS    # tab completion

Then restart your terminal, or run those lines in your current shell.

After that, you can use the 'jlo' command to manage your Java environments.
These lines never change: upgrades regenerate the files they point at.
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
