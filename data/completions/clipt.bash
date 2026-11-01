# bash completion for clipt.
#
# Install as /usr/share/bash-completion/completions/clipt, or source this file
# from ~/.bashrc.

_clipt_commands="gui full screen list record tray update"
_clipt_common_opts="-o --output -c --clipboard -g --geometry -d --delay -b --backend --cursor --fps -h --help -V --version"
_clipt_record_opts="--stop --pause --resume --toggle-pause"
_clipt_record_modes="screen region window"
_clipt_backends="ext wlr kwin portal x11"

# Output names from `clipt list`: one per line, first tab-separated column.
_clipt_outputs() {
    command clipt list 2>/dev/null | awk -F '\t' 'NF > 1 { print $1 }'
}

_clipt() {
    local cur prev
    COMPREPLY=()
    cur=${COMP_WORDS[COMP_CWORD]}
    prev=${COMP_WORDS[COMP_CWORD - 1]}

    # Values of options that take one.
    case $prev in
        -o|--output)
            if declare -F _filedir >/dev/null; then
                _filedir
            else
                COMPREPLY=($(compgen -f -- "$cur"))
            fi
            return 0
            ;;
        -b|--backend)
            COMPREPLY=($(compgen -W "$_clipt_backends" -- "$cur"))
            return 0
            ;;
        -g|--geometry|-d|--delay|--fps)
            return 0
            ;;
    esac

    case $cur in
        --backend=*)
            COMPREPLY=($(compgen -W "$_clipt_backends" -P '--backend=' -- "${cur#*=}"))
            return 0
            ;;
    esac

    # Positional arguments before the cursor, ignoring options and their values.
    local -a args=()
    local i
    for ((i = 1; i < COMP_CWORD; i++)); do
        case ${COMP_WORDS[i]} in
            -o|--output|-g|--geometry|-d|--delay|-b|--backend|--fps)
                ((i++))
                ;;
            --output=*|--geometry=*|--delay=*|--backend=*|--fps=*)
                ;;
            -*)
                ;;
            *)
                args+=("${COMP_WORDS[i]}")
                ;;
        esac
    done
    local cmd=${args[0]:-} mode=${args[1]:-}

    # Options, including the record controls when they can be used.
    if [[ $cur == -* ]]; then
        local opts=$_clipt_common_opts
        if [[ -z $cmd || $cmd == record ]]; then
            opts+=" $_clipt_record_opts"
        fi
        if [[ -z $cmd || $cmd == update ]]; then
            opts+=" --check"
        fi
        COMPREPLY=($(compgen -W "$opts" -- "$cur"))
        return 0
    fi

    if [[ -z $cmd ]]; then
        COMPREPLY=($(compgen -W "$_clipt_commands" -- "$cur"))
        return 0
    fi

    if [[ $cmd == screen && ${#args[@]} -eq 1 ]]; then
        COMPREPLY=($(compgen -W "$(_clipt_outputs)" -- "$cur"))
        return 0
    fi

    if [[ $cmd == record ]]; then
        if [[ ${#args[@]} -eq 1 ]]; then
            COMPREPLY=($(compgen -W "$_clipt_record_modes" -- "$cur"))
        elif [[ $mode == screen && ${#args[@]} -eq 2 ]]; then
            COMPREPLY=($(compgen -W "$(_clipt_outputs)" -- "$cur"))
        fi
        return 0
    fi
}

complete -F _clipt clipt
