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
  PROMPT_COMMAND="jlo_after_cd; $PROMPT_COMMAND"
fi

# Immediate call for fresh spawned shells
if [ -f ".jlorc" ] || [ -f "$JLO_HOME/default.jlorc" ]; then
  _JLO_LAST_DIR="$PWD"
  jlo env
fi
