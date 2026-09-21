#!/usr/bin/env bash
jlo() {
  local J arg out
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
      printf '%s' "Before update: "
      "$J" --version
      /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/java-loader/jlo/refs/heads/main/install.sh)"
      printf '%s' "After update: "
      "$J" --version
      ;;
    *)
      "$J" "$@"
      ;;
  esac
}
