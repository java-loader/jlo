#!/usr/bin/env sh

# Is there a .jlorc at or above $PWD? Mirrors find_project_config() in
# src/conf.rs: the search stops after $HOME and after a VCS root, both
# inclusive. Kept in shell rather than delegated to jlo-bin because this runs
# on every cd, and a process spawn per directory change is not worth it.
#
# "${dir%/*}" walks up without forking a dirname; it yields "" at the last
# component, hence the "/" fixup.
jlo_find_jlorc() {
  _jlo_dir="$PWD"
  while :; do
    if [ -f "$_jlo_dir/.jlorc" ]; then
      unset _jlo_dir
      return 0
    fi
    if [ "$_jlo_dir" = "$HOME" ] || [ -e "$_jlo_dir/.git" ] || [ "$_jlo_dir" = "/" ]; then
      break
    fi
    _jlo_dir="${_jlo_dir%/*}"
    [ -n "$_jlo_dir" ] || _jlo_dir="/"
  done
  unset _jlo_dir
  return 1
}

# The guard is what keeps this to one run per directory rather than one per
# prompt: bash drives it from PROMPT_COMMAND, which fires before every prompt.
#
# --offline is the whole of "a cd must never start a download". jlo env would
# otherwise install on demand, so stepping into a project pinning a JDK you do
# not have would stall the shell on ~100MB. The binary decides that, not this
# file, so the rule cannot drift between the zsh and bash paths; when it
# declines it prints one line to stderr and leaves the environment alone.
#
# The explicit `return 0` keeps a declined lookup out of `$?`: PROMPT_COMMAND
# runs between the user's command and their prompt, and a hook that reported
# its own failure there would overwrite the status their prompt is showing.
jlo_after_cd() {
  [ "$PWD" = "${_JLO_LAST_DIR-}" ] && return 0
  _JLO_LAST_DIR="$PWD"
  jlo_find_jlorc && jlo env --offline
  return 0
}

if [ -n "${ZSH_VERSION-}" ]; then
  autoload -U add-zsh-hook
  add-zsh-hook chpwd jlo_after_cd
elif [ -n "${BASH_VERSION-}" ]; then
  # bash has no add-zsh-hook equivalent, so guard against stacking duplicates
  # when this file is sourced again (e.g. `source ~/.bashrc`).
  #
  # "${PROMPT_COMMAND[@]}" expands a scalar as a single element, so one loop
  # recognises an existing registration whether PROMPT_COMMAND is a scalar or
  # (bash 5.1+) an array. Matching the ";"-delimited token, not the bare name,
  # keeps `echo jlo_after_cd` from passing as the hook itself.
  #
  # The same loop counts the elements. "${#PROMPT_COMMAND[@]}" would be the
  # obvious way, but it has no unset-default form and so aborts a profile
  # running under `set -u`; the "-" in the expansion below makes the loop safe
  # there, at the cost of yielding one empty word for an unset or empty
  # PROMPT_COMMAND - which lands on the scalar branch, exactly where a count of
  # 0 did.
  _jlo_registered=
  _jlo_count=0
  for _jlo_cmd in "${PROMPT_COMMAND[@]-}"; do
    _jlo_count=$((_jlo_count + 1))
    case ";$_jlo_cmd;" in
      *";jlo_after_cd;"*) _jlo_registered=1 ;;
    esac
  done

  if [ -z "$_jlo_registered" ]; then
    if [ "$_jlo_count" -gt 1 ]; then
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
  unset _jlo_registered _jlo_count _jlo_cmd
fi

# Immediate call for fresh spawned shells. --offline for the same reason as in
# the hook, and more sharply: a download here delays every new terminal.
if jlo_find_jlorc || [ -f "${JLO_HOME-}/default.jlorc" ]; then
  _JLO_LAST_DIR="$PWD"
  # `|| :` for the same reason as the hook's `return 0`, one step earlier:
  # this runs while the profile is still being sourced, and jlo env now
  # propagates the binary's status, so an unsatisfiable --offline lookup
  # would abort a profile running under `set -e`.
  jlo env --offline || :
fi
