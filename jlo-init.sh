#!/usr/bin/env bash
jlo() {
  J="$JLO_HOME/bin/jlo-bin"
  case "$1" in
    env|use)
      # The branch below *sources* stdout, so help and version output - which
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
      # shellcheck disable=SC1090
      . <("$J" "$@")
      ;;
    selfupdate)
      echo -n "Version before update: "
      "$J" version
      /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/java-loader/jlo/refs/heads/main/install.sh)"
      echo -n "Version after update: "
      "$J" version
      ;;
    *)
      "$J" "$@"
      ;;
  esac
}
