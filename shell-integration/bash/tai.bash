# tai shell integration for bash, written for tai (not taken from Ghostty
# or Kitty, whose scripts are GPLv3). tai starts bash with --rcfile
# pointing here. It reads the startup files bash would have read, then
# reports the working directory to the terminal (OSC 7) at each prompt,
# and marks the prompt (OSC 133).
# With TAI_BASH_LOGIN set, tai would have started a login shell (macOS
# does, as Terminal.app does), which ignores --rcfile, so this file reads
# the login files itself.

if [[ -n "${TAI_BASH_LOGIN-}" ]]; then
    builtin unset TAI_BASH_LOGIN
    [[ -r /etc/profile ]] && builtin source /etc/profile
    for _tai_file in ~/.bash_profile ~/.bash_login ~/.profile; do
        if [[ -r "$_tai_file" ]]; then
            builtin source "$_tai_file"
            break
        fi
    done
    builtin unset _tai_file
else
    [[ -r ~/.bashrc ]] && builtin source ~/.bashrc
fi

# The path as is, without percent-encoding, as kitty-shell-cwd URLs carry
# it; control characters are dropped.
_tai_report_pwd() {
    builtin printf '\e]7;kitty-shell-cwd://%s%s\a' \
        "${HOSTNAME//[[:cntrl:]]}" "${PWD//[[:cntrl:]]}"
}

# Prompt marks (OSC 133), so tai knows when the shell is at its prompt
# (ADR 0013 decision 7): D when a command has ended and A as the prompt
# starts, B at the end of PS1 where input starts, C from PS0 when a
# command line has been read. PS0 needs bash 4.4; an older bash (macOS's
# /bin/bash is 3.2) would never say a command is running, so it sends no
# marks at all and tai falls back to its rule for shells without them.
if (( BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4) )); then
    _tai_mark_prompt() {
        builtin printf '\e]133;D\a\e]133;A\a'
    }
    # Runs after the user's own prompt hooks, which may set PS1 again
    # (starship, oh-my-bash).
    _tai_mark_input() {
        [[ "$PS1" == *'133;B'* ]] || PS1+='\[\e]133;B\a\]'
        [[ "${PS0-}" == *'133;C'* ]] || PS0+=$'\e]133;C\a'
    }
else
    _tai_mark_prompt() { :; }
    _tai_mark_input() { :; }
fi

_tai_precmd() {
    _tai_mark_prompt
    _tai_report_pwd
}
# Newlines, not ;, around the user's own: one ending in ; would make ;;.
_tai_nl=$'\n'
PROMPT_COMMAND="_tai_precmd$_tai_nl${PROMPT_COMMAND:+$PROMPT_COMMAND$_tai_nl}_tai_mark_input"
builtin unset _tai_nl
# bash-preexec (which iTerm2's and other integrations load) takes
# PROMPT_COMMAND over at the first prompt and runs precmd_functions
# instead, so the hooks go there too. A second report or mark is harmless.
precmd_functions+=(_tai_precmd _tai_mark_input)
