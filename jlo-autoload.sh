#!/usr/bin/env sh

jlo_after_cd() {
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
  jlo env
fi
