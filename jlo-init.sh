#!/usr/bin/env bash
jlo() {
  local J arg out url tmp rc
  J="${JLO_HOME-}/bin/jlo-bin"
  case "$1" in
    env|use)
      # The branch below *evaluates* stdout, so help and version output - which
      # clap prints to stdout - must never reach it. Scoped to env/use on
      # purpose: a wrapper-wide scan would hijack a child's flags in
      # 'jlo exec -- ./gradlew --help'. env/use take at most a version.
      for arg in "$@"; do
        case "$arg" in
          -h|--help|-V|--version)
            "$J" "$@"
            return
            ;;
        esac
      done
      # Capture, then eval - deliberately not `. <("$J" "$@")`. bash 3.2, which
      # macOS still ships as /bin/bash, cannot `source` the /dev/fd/N of a
      # process substitution: it reads nothing and returns 0, so 'jlo env' was
      # a silent no-op there. Separately, `. <(cmd)` discards cmd's exit status
      # in every shell, so the wrapper could not tell success from failure.
      #
      # The assignment is its own statement: `local out="$(...)"` would report
      # `local`'s status, not the binary's. `|| return` then propagates the
      # failure instead of eval-ing a half-written environment - see the
      # explicit `return 0` in jlo_after_cd, which keeps that status out of the
      # user's prompt.
      out="$("$J" "$@")" || return
      eval "$out"
      ;;
    selfupdate)
      # Everything here is progress, not machine output, so it goes to stderr -
      # including the installer's own stdout. This branch has nothing to say on
      # the environment channel, and keeping it silent there is what lets a
      # later version print an eval-able reload line without ambiguity.
      url='https://raw.githubusercontent.com/java-loader/jlo/refs/heads/main/install.sh'
      printf 'Before update: ' >&2
      "$J" --version >&2
      # Downloaded to a file, not run as `sh -c "$(curl ...)"`. Two failures
      # that form hides. An HTTP error makes `curl -f` write *nothing*, so the
      # substitution yields "" and `sh -c ""` exits 0 - a failed update that
      # reports success. And a connection dropped mid-transfer yields a
      # truncated but non-empty body, which `-f` cannot catch at all; the file
      # keeps the bytes where they can be checked, and install.sh holds every
      # statement inside a function it calls only on its last line, so half of
      # it parses without doing anything.
      tmp="$(mktemp "${TMPDIR:-/tmp}/jlo-install.XXXXXX")" || {
        echo "jlo: could not create a temporary file for the installer" >&2
        return 1
      }
      if ! curl -fsSL "$url" -o "$tmp"; then
        rm -f "$tmp"
        echo "jlo: could not download the installer from $url" >&2
        return 1
      fi
      if [ ! -s "$tmp" ]; then
        rm -f "$tmp"
        echo "jlo: the installer downloaded from $url was empty" >&2
        return 1
      fi
      # install.sh is '#!/usr/bin/env sh' and written to POSIX; running it under
      # /bin/bash implied a dependency it never had.
      sh "$tmp" >&2
      rc=$?
      rm -f "$tmp"
      if [ "$rc" -ne 0 ]; then
        echo "jlo: the installer failed (exit $rc); jlo was not updated" >&2
        return "$rc"
      fi
      printf 'After update: ' >&2
      "$J" --version >&2
      ;;
    *)
      "$J" "$@"
      ;;
  esac
}
