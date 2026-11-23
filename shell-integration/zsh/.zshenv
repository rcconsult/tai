# tai shell integration for zsh, written for tai (not taken from Ghostty or
# Kitty, whose scripts are GPLv3). tai starts zsh with ZDOTDIR pointing
# here, so zsh reads this file first. It puts the user's ZDOTDIR back, so
# every other startup file is the user's own, sources the user's .zshenv,
# and in an interactive shell reports the working directory to the
# terminal (OSC 7) at each prompt and on each cd, and marks the prompt
# (OSC 133).

if [[ -n "${TAI_ZSH_ZDOTDIR+X}" ]]; then
    'builtin' 'export' ZDOTDIR="$TAI_ZSH_ZDOTDIR"
    'builtin' 'unset' 'TAI_ZSH_ZDOTDIR'
else
    'builtin' 'unset' 'ZDOTDIR'
fi

{
    'builtin' 'typeset' _tai_file="${ZDOTDIR-$HOME}/.zshenv"
    [[ ! -r "$_tai_file" ]] || 'builtin' 'source' '--' "$_tai_file"
} always {
    'builtin' 'unset' '_tai_file'
    if [[ -o 'interactive' ]]; then
        # The path as is, without percent-encoding, as kitty-shell-cwd
        # URLs carry it; control characters are dropped.
        _tai_report_pwd() {
            'builtin' 'printf' '\e]7;kitty-shell-cwd://%s%s\a' \
                "${HOST//[[:cntrl:]]}" "${PWD//[[:cntrl:]]}"
        }
        # Prompt marks (OSC 133), so tai knows when the shell is at its
        # prompt (ADR 0013 decision 7): D when a command has ended and A
        # as the prompt starts, B at the end of PS1 where input starts,
        # and C before a command runs.
        _tai_precmd() {
            'builtin' 'printf' '\e]133;D\a\e]133;A\a'
            # The user's prompt hooks, added after this file, may set PS1
            # again (powerlevel10k, starship): the mark goes on last.
            precmd_functions=(${precmd_functions:#_tai_mark_input} _tai_mark_input)
        }
        _tai_mark_input() {
            [[ "$PS1" == *'133;B'* ]] || PS1+=$'%{\e]133;B\a%}'
        }
        _tai_preexec() {
            'builtin' 'printf' '\e]133;C\a'
        }
        'builtin' 'typeset' -ag precmd_functions chpwd_functions preexec_functions
        precmd_functions+=(_tai_report_pwd _tai_precmd _tai_mark_input)
        chpwd_functions+=(_tai_report_pwd)
        preexec_functions+=(_tai_preexec)
    fi
}
