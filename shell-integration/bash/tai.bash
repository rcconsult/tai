# tai shell integration for bash, written for tai (not taken from Ghostty
# or Kitty, whose scripts are GPLv3). tai starts bash with --rcfile
# pointing here. It reads the startup files bash would have read, then
# reports the working directory to the terminal (OSC 7) at each prompt.
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
PROMPT_COMMAND="_tai_report_pwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
# bash-preexec (which iTerm2's and other integrations load) takes
# PROMPT_COMMAND over at the first prompt and runs precmd_functions
# instead, so the hook goes there too. A second report is harmless.
precmd_functions+=(_tai_report_pwd)
