#!/usr/bin/env sh

set -eu

# Everything lives in a function, and the only top-level statement is the
# 'main "$@"' at the very bottom. This script is fetched over the network and
# piped straight into a shell, and a connection dropped mid-transfer yields a
# *truncated but non-empty* body that 'curl -f' cannot catch. Truncated inside
# main, the unterminated function is a parse error and nothing runs; truncated
# before the last line, the file parses and still does nothing. Either way the
# half a script never executes.
#
# This is a bootstrap, and nothing more. It detects the platform, downloads one
# file, verifies it, unpacks it, and hands over to the binary. The layout under
# $JLO_HOME - the entry files, the per-dialect wrappers, the completions, the
# symlink and the receipt - belongs to 'jlo-bin' itself, which carries the shell
# sources compiled in. Two copies of that knowledge is what this script used to
# be, and what made an installer change go stale in every existing profile.

# Prints the SHA256 of "$1" as bare hex, or fails when neither tool is here.
# macOS ships shasum, GNU userlands ship sha256sum, and a stripped container
# may have neither.
#
# Read from stdin rather than passing the path: both tools escape a file name
# containing a backslash or a newline, and announce it by prefixing the *digest
# line* with a backslash. A JLO_HOME with a backslash in it would otherwise
# yield "\<hex>" here and fail every comparison.
jlo_sha256() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 < "$1" | cut -d ' ' -f 1
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum < "$1" | cut -d ' ' -f 1
  else
    return 1
  fi
}

main() {
  # Honour a JLO_HOME the user has already exported (from an earlier install, or
  # just for this run) instead of installing somewhere they are not sourcing from.
  # Note: JLO_HOME holds J'Lo's own files, not the JDKs - those go to the IntelliJ
  # IDEA directory and are not configurable.
  if [ -z "${JLO_HOME-}" ]; then
    JLO_HOME="$HOME/.jlo"
  fi
  # Exported, not merely set: the binary below reads it out of the environment
  # to decide where to write the layout.
  export JLO_HOME
  JLO_BIN_DIR="$JLO_HOME/bin"
  # Overridable so the download half of this script can be pointed at a local
  # server, the way JLO_ADOPTIUM_API_URL and JLO_RELEASE_API_URL are for the
  # binary's two remotes. Without it the only testable part of an install is
  # the layout the binary writes afterwards.
  JLO_BASE_URL="${JLO_INSTALL_BASE_URL:-https://github.com/java-loader/jlo/releases/latest/download}"

  OS="$(uname | tr '[:upper:]' '[:lower:]')"
  ARCH="$(uname -m)"

  if [ "$OS" = "linux" ]; then
    JLO_PACKAGE="jlo-linux-$ARCH.tar.gz"
  elif [ "$OS" = "darwin" ]; then
    JLO_PACKAGE="jlo-macos-$ARCH.tar.gz"
  else
    echo "Unsupported OS: $OS" >&2
    exit 1
  fi
  JLO_URL="$JLO_BASE_URL/$JLO_PACKAGE"

  mkdir -p "$JLO_BIN_DIR"

  # Everything transient lives in one directory, and that directory is a
  # *sibling* of the file it will become: the binary is published by a rename,
  # which is atomic only within one filesystem, so $TMPDIR is not an option.
  #
  # Nothing here writes $JLO_BIN_DIR/jlo-bin. That write belongs to the install
  # verb, which takes the publication lock first - an installer that unpacked
  # over the live binary would be a second publisher outside the lock, and on
  # Linux would hit ETXTBSY against a jlo that is still running.
  JLO_STAGE="$JLO_BIN_DIR/.jlo-install-$$"
  rm -rf "$JLO_STAGE"
  mkdir -p "$JLO_STAGE"
  # While the shell owns the staging directory, the shell removes it - on the
  # explicit failures below, on an unguarded command failing under `set -e`,
  # and on an interrupt. The successful `exec` hands that responsibility over:
  # it replaces this process image, so no trap of ours can run afterwards, and
  # from that point the binary sweeps its own staging directory. An `exec` that
  # *fails* - a package built for another architecture, a missing loader -
  # leaves this shell running, and the trap still fires.
  trap 'rm -rf "$JLO_STAGE"' EXIT HUP INT TERM

  FQ_JLO_BUNDLE="$JLO_STAGE/jlo.tar.gz"
  FQ_JLO_SUM="$FQ_JLO_BUNDLE.sha256"
  if ! curl -fsSL "$JLO_URL" -o "$FQ_JLO_BUNDLE"; then
    echo "Failed to download jlo binary from $JLO_URL" >&2
    rm -rf "$JLO_STAGE"
    exit 1
  fi

  # The checksum is published beside the tarball, so it shares an origin with
  # it: it catches a corrupt or truncated download, not a compromised release.
  # That is also why a *missing* one is a warning rather than a refusal - it is
  # absent only on releases that predate it, and failing closed there would
  # strand users on a download TLS already protected. A checksum that is
  # present and does not match is a different thing entirely, and fatal.
  if curl -fsSL "$JLO_URL.sha256" -o "$FQ_JLO_SUM" 2>/dev/null; then
    # The first line only. Without it a multi-line file yields a multi-line
    # JLO_EXPECTED, and the checks below both slip: the glob counts a newline
    # as one of its 64 characters, and the command substitution strips the
    # newline tr leaves behind, so "32 hex chars, newline, 31 hex chars" reads
    # as a well-formed digest.
    JLO_EXPECTED="$(head -n 1 "$FQ_JLO_SUM" | cut -d ' ' -f 1)"
    rm -f "$FQ_JLO_SUM"
    # An empty or malformed expectation must never pass for a match, and it
    # must not reach the "no tool here" branch either - that branch is for a
    # missing shasum, not for a checksum file we could not read. Two tests,
    # because neither alone is enough: the glob fixes the length (POSIX case
    # patterns cannot count repetitions) and the tr fixes the alphabet.
    JLO_NOT_HEX="$(printf '%s' "$JLO_EXPECTED" | tr -d '0-9a-fA-F')"
    case "$JLO_EXPECTED" in
      ????????????????????????????????????????????????????????????????)
        JLO_WELL_FORMED=1 ;;
      *)
        JLO_WELL_FORMED=0 ;;
    esac
    if [ "$JLO_WELL_FORMED" = 0 ] || [ -n "$JLO_NOT_HEX" ]; then
      echo "Checksum file for $JLO_PACKAGE is not a SHA256; refusing to install." >&2
      rm -rf "$JLO_STAGE"
      exit 1
    fi
    if JLO_ACTUAL="$(jlo_sha256 "$FQ_JLO_BUNDLE")"; then
      # Case-insensitive: the tool that wrote the file and the tool reading it
      # need not agree on the case of the hex.
      JLO_EXPECTED="$(printf '%s' "$JLO_EXPECTED" | tr 'A-F' 'a-f')"
      JLO_ACTUAL="$(printf '%s' "$JLO_ACTUAL" | tr 'A-F' 'a-f')"
      if [ "$JLO_ACTUAL" != "$JLO_EXPECTED" ]; then
        echo "Checksum mismatch for $JLO_PACKAGE" >&2
        echo "  expected $JLO_EXPECTED" >&2
        echo "  got      $JLO_ACTUAL" >&2
        rm -rf "$JLO_STAGE"
        exit 1
      fi
    else
      echo "Warning: neither shasum nor sha256sum is available; skipping checksum verification." >&2
    fi
  else
    rm -f "$FQ_JLO_SUM"
    echo "Warning: no published checksum for $JLO_PACKAGE; skipping verification." >&2
  fi

  # The tarball carries exactly one file, 'jlo-bin'. The shell code used to
  # travel beside it and could fall out of step with it; it is compiled in now.
  #
  # cd into the directory instead of passing paths to tar. GNU tar unquotes
  # file names by default, so a backslash escape in a name - a JLO_HOME
  # containing a literal \n, say - is turned into the character it denotes
  # before tar ever looks for it, and the extraction fails on a path that does
  # exist. That applies to the argument of -f as much as to -C, so neither may
  # carry a user-supplied path. bsdtar does not unquote, which is why this only
  # ever showed up on Linux. cd is a shell builtin and does no such thing, and
  # the subshell keeps the working directory change local.
  if ! (cd "$JLO_STAGE" && tar -xzf jlo.tar.gz); then
    echo "Failed to extract jlo binary from $FQ_JLO_BUNDLE" >&2
    rm -rf "$JLO_STAGE"
    exit 1
  fi
  # Removed before the hand-over, which never returns here: what is left in the
  # staging directory afterwards is the binary alone, so the install verb can
  # take the directory away with the rename that empties it.
  rm -f "$FQ_JLO_BUNDLE"

  JLO_TARGET="$JLO_STAGE/jlo-bin"
  if [ ! -x "$JLO_TARGET" ]; then
    echo "The downloaded archive did not contain an executable jlo-bin." >&2
    rm -rf "$JLO_STAGE"
    exit 1
  fi

  # Hand over. 'exec' rather than a call: the binary owns the rest of the
  # install and its exit status is the installer's, with no line of this script
  # left to run after it and get that wrong.
  #
  # --publish-self is what moves the staged binary to $JLO_BIN_DIR/jlo-bin, and
  # it happens under the lock the verb takes. It also owns the staging
  # directory from here on: no line of this script runs after the exec, so the
  # binary is the only thing left that can remove it - which it does whether
  # the publication succeeds or fails.
  exec "$JLO_TARGET" __install --publish-self
}

main "$@"
