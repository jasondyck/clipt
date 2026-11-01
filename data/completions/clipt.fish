# fish completion for clipt.
#
# Install as ~/.config/fish/completions/clipt.fish.

# Positional arguments before the cursor, ignoring options and their values.
function __fish_clipt_positionals
    set -l tokens (commandline -opc)
    if set -q tokens[1]
        set -e tokens[1]
    end
    set -l words
    set -l skip 0
    for token in $tokens
        if test $skip -eq 1
            set skip 0
            continue
        end
        switch $token
            case -o --output -g --geometry -d --delay -b --backend --fps
                set skip 1
            case '-?*'
                # A flag, or an option with an attached value.
            case '*'
                set words $words $token
        end
    end
    if test (count $words) -gt 0
        printf '%s\n' $words
    end
end

function __fish_clipt_use_command
    test (count (__fish_clipt_positionals)) -eq 0
end

function __fish_clipt_wants_record_mode
    set -l args (__fish_clipt_positionals)
    if test (count $args) -eq 1; and test "$args[1]" = record
        return 0
    end
    return 1
end

# True where an output name is expected: `clipt screen NAME` and
# `clipt record screen NAME`.
function __fish_clipt_wants_output
    set -l args (__fish_clipt_positionals)
    if test (count $args) -eq 1; and test "$args[1]" = screen
        return 0
    end
    if test (count $args) -eq 2; and test "$args[1]" = record; and test "$args[2]" = screen
        return 0
    end
    return 1
end

# Output names from `clipt list`: first tab-separated column of each line.
function __fish_clipt_outputs
    command clipt list 2>/dev/null | string match -r '^[^\t]+\t' | string replace -r '\t.*' ''
end

complete -c clipt -f

# Commands.
complete -c clipt -n __fish_clipt_use_command -a gui -d 'Select a region and annotate it (default)'
complete -c clipt -n __fish_clipt_use_command -a full -d 'Capture the whole desktop'
complete -c clipt -n __fish_clipt_use_command -a screen -d 'Capture one output (default: the first)'
complete -c clipt -n __fish_clipt_use_command -a list -d 'Show outputs and available capture backends'
complete -c clipt -n __fish_clipt_use_command -a record -d 'Pick an area to record (Enter starts)'
complete -c clipt -n __fish_clipt_use_command -a tray -d 'Sit in the system tray'
complete -c clipt -n __fish_clipt_use_command -a update -d 'Update to the latest release'

# Record modes.
complete -c clipt -n __fish_clipt_wants_record_mode -f -a screen -d 'Record one output (default: the first)'
complete -c clipt -n __fish_clipt_wants_record_mode -f -a region -d 'Pick an area to record'
complete -c clipt -n __fish_clipt_wants_record_mode -f -a window -d 'Record a window'

# Output names for `clipt screen` and `clipt record screen`.
complete -c clipt -n __fish_clipt_wants_output -f -a '(__fish_clipt_outputs)' -d 'Output'

# Options.
complete -c clipt -s o -l output -r -F -d 'Write the PNG to PATH (- for stdout)'
complete -c clipt -s c -l clipboard -d 'Copy to the clipboard (full/screen)'
complete -c clipt -s g -l geometry -x -d 'Crop to "X,Y WxH" in logical pixels (slurp format)'
complete -c clipt -s d -l delay -x -d 'Wait before capturing'
complete -c clipt -s b -l backend -x -a 'ext wlr kwin portal x11' -d 'Force a backend'
complete -c clipt -l cursor -d 'Include the pointer'
complete -c clipt -l fps -x -n '__fish_seen_subcommand_from record' -d 'Recording frame rate (default 30)'
complete -c clipt -l check -n '__fish_seen_subcommand_from update' -d 'Only check for updates'
complete -c clipt -l stop -n '__fish_seen_subcommand_from record' -d 'Stop the running recording'
complete -c clipt -l pause -n '__fish_seen_subcommand_from record' -d 'Pause the running recording'
complete -c clipt -l resume -n '__fish_seen_subcommand_from record' -d 'Resume the running recording'
complete -c clipt -l toggle-pause -n '__fish_seen_subcommand_from record' -d 'Toggle pausing the recording'
complete -c clipt -s h -l help -d 'Show help'
complete -c clipt -s V -l version -d 'Show the version'
