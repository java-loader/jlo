
# What follows is common to both dialects; the binary appends it to each.

# Is there a .jlorc at or above $PWD? Mirrors find_project_config() in
# src/conf.rs: the search stops after $HOME and after a VCS root, both
# inclusive. Kept in shell rather than delegated to jlo-bin because this runs
# on every cd, and a process spawn per directory change is not worth it.
#
# "${dir%/*}" walks up without forking a dirname; it yields "" at the last
# component, hence the "/" fixup.
jlo_find_jlorc() {
  local dir="$PWD"
  # "${HOME%/}" so a HOME carrying a trailing slash still stops the walk. The
  # Rust side compares Paths, which are component-wise and so already ignore
  # it; a string compare here would never match, and the two halves of one
  # rule would disagree about where home is - the hook firing on a .jlorc
  # above HOME that the binary then refuses to read.
  local home="${HOME%/}"
  while :; do
    if [ -f "$dir/.jlorc" ]; then
      return 0
    fi
    if [ "$dir" = "$home" ] || [ -e "$dir/.git" ] || [ "$dir" = "/" ]; then
      return 1
    fi
    dir="${dir%/*}"
    [ -n "$dir" ] || dir="/"
  done
}

# The $PWD guard is what keeps this to one run per directory rather than one
# per prompt: bash drives it from PROMPT_COMMAND, which fires before every
# prompt, not only after a cd. zsh's chpwd only fires on an actual directory
# change, so there the guard is redundant - kept so the function behaves
# identically when called by hand, and so the two dialects answer the same way.
#
# --offline is the whole of "a cd must never start a download". jlo env would
# otherwise install on demand, so stepping into a project pinning a JDK you do
# not have would stall the shell on ~100MB. The binary decides that, not this
# file, so the rule cannot drift between the zsh and bash paths; when it
# declines it prints one line to stderr and leaves the environment alone.
#
# The explicit `return 0` keeps a declined lookup out of `$?`: bash's
# PROMPT_COMMAND and zsh's chpwd hooks both run between the user's command and
# their prompt, and a hook that reported its own failure there would overwrite
# the status their prompt is showing.
#
# `|| :` is what makes that `return 0` reachable. POSIX exempts every command
# of an AND-OR list from `set -e` *except the last*, so the older
# `jlo_find_jlorc && jlo env --offline` exited the shell outright when the
# lookup succeeded and the offline install did not - measured in bash 3.2 and
# zsh alike. A profile that cds after sourcing this file is enough to hit it.
jlo_after_cd() {
  [ "$PWD" = "${_JLO_LAST_DIR-}" ] && return 0
  _JLO_LAST_DIR="$PWD"
  if jlo_find_jlorc; then
    jlo env --offline || :
  fi
  return 0
}

# Immediate call for freshly spawned shells. --offline for the same reason as
# in the hook, and more sharply: a download here delays every new terminal.
if jlo_find_jlorc || [ -f "${JLO_HOME-}/default.jlorc" ]; then
  _JLO_LAST_DIR="$PWD"
  # `|| :` for the same reason as the hook's `return 0`, one step earlier:
  # this runs while the profile is still being sourced, and jlo env now
  # propagates the binary's status, so an unsatisfiable --offline lookup
  # would abort a profile running under `set -e`.
  jlo env --offline || :
fi
