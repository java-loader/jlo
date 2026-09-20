#!/usr/bin/env sh

jlo_after_cd() {
  [ "$PWD" = "$_JLO_LAST_DIR" ] && return
  _JLO_LAST_DIR="$PWD"
  [ -f ".jlorc" ] && jlo env
}

if [ -n "$ZSH_VERSION" ]; then
  autoload -U add-zsh-hook
  add-zsh-hook chpwd jlo_after_cd
elif [ -n "$BASH_VERSION" ]; then
  # bash has no add-zsh-hook equivalent, so guard against stacking duplicates
  # when this file is sourced again (e.g. `source ~/.bashrc`).
  #
  # "${PROMPT_COMMAND[@]}" expands a scalar as a single element, so one loop
  # recognises an existing registration whether PROMPT_COMMAND is a scalar or
  # (bash 5.1+) an array. Matching the ";"-delimited token, not the bare name,
  # keeps `echo jlo_after_cd` from passing as the hook itself.
  _jlo_registered=
  for _jlo_cmd in "${PROMPT_COMMAND[@]-}"; do
    case ";$_jlo_cmd;" in
      *";jlo_after_cd;"*) _jlo_registered=1 ;;
    esac
  done

  if [ -z "$_jlo_registered" ]; then
    if [ "${#PROMPT_COMMAND[@]}" -gt 1 ]; then
      # bash 5.1+ runs each array element as its own command; splicing into
      # element 0 would glue our hook onto the user's first one. `eval` keeps
      # the array syntax out of reach of POSIX sh parsers reading this file.
      eval 'PROMPT_COMMAND=(jlo_after_cd "${PROMPT_COMMAND[@]}")'
    else
      # Scalar, unset, or single-element array: a plain assignment adds the
      # hook while preserving the variable's type and export flag.
      PROMPT_COMMAND="jlo_after_cd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
    fi
  fi
  unset _jlo_registered _jlo_cmd
fi

# Immediate call for fresh spawned shells
if [ -f ".jlorc" ] || [ -f "$JLO_HOME/default.jlorc" ]; then
  _JLO_LAST_DIR="$PWD"
  jlo env
fi
