# tai shell integration for zsh, written for tai (not taken from Ghostty or
# Kitty, whose scripts are GPLv3). tai starts zsh with ZDOTDIR pointing
# here, so zsh reads this file first. It puts the user's ZDOTDIR back, so
# every other startup file is the user's own, sources the user's .zshenv,
# and in an interactive shell reports the working directory to the
# terminal (OSC 7) at each prompt and on each cd.

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
        'builtin' 'typeset' -ag precmd_functions chpwd_functions
        precmd_functions+=(_tai_report_pwd)
        chpwd_functions+=(_tai_report_pwd)
    fi
}
