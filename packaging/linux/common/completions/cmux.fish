# fish completion for cmux
#
# Mirrors the noun-first public grammar from cmux-tui/spec/cli.md. Selectors
# complete only as `current`; resolving live IDs would need a running session.

function __cmux_no_subcommand
    set -l cmd (commandline -opc)
    set -e cmd[1]
    for token in $cmd
        string match -q -- '-*' $token; and continue
        return 1
    end
    return 0
end

function __cmux_using_root -a root
    set -l cmd (commandline -opc)
    set -e cmd[1]
    for token in $cmd
        string match -q -- '-*' $token; and continue
        test "$token" = "$root"; and return 0
        return 1
    end
    return 1
end

# Process modes
complete -c cmux -n __cmux_no_subcommand -f -a attach -d 'Open the complete session TUI'
complete -c cmux -n __cmux_no_subcommand -f -a relay -d 'Copy protocol bytes between stdio and a session socket'
complete -c cmux -n __cmux_no_subcommand -f -a machine-agent -d 'Share a local session over one outbound SSH registration'

# Resource roots
complete -c cmux -n __cmux_no_subcommand -f -a machine -d 'Machines'
complete -c cmux -n __cmux_no_subcommand -f -a session -d 'Sessions'
complete -c cmux -n __cmux_no_subcommand -f -a client -d 'Attached clients'
complete -c cmux -n __cmux_no_subcommand -f -a workspace -d 'Workspaces'
complete -c cmux -n __cmux_no_subcommand -f -a screen -d 'Screens'
complete -c cmux -n __cmux_no_subcommand -f -a pane -d 'Panes'
complete -c cmux -n __cmux_no_subcommand -f -a tab -d 'Tabs'
complete -c cmux -n __cmux_no_subcommand -f -a terminal -d 'PTY terminals'
complete -c cmux -n __cmux_no_subcommand -f -a browser -d 'Browser panes'
complete -c cmux -n __cmux_no_subcommand -f -a notification -d 'Notifications'
complete -c cmux -n __cmux_no_subcommand -f -a agent -d 'Agents'
complete -c cmux -n __cmux_no_subcommand -f -a sidebar -d 'Sidebar views and plugins'
complete -c cmux -n __cmux_no_subcommand -f -a pairing -d 'Pairing requests'
complete -c cmux -n __cmux_no_subcommand -f -a projection -d 'Projections'
complete -c cmux -n __cmux_no_subcommand -f -a provider -d 'Provider administration'
complete -c cmux -n __cmux_no_subcommand -f -a raw -d 'Raw protocol escape hatch'

# Start options
complete -c cmux -n __cmux_no_subcommand -l session -r -d 'Session name (default: main)'
complete -c cmux -n __cmux_no_subcommand -l socket -r -F -d 'Explicit socket path'
complete -c cmux -n __cmux_no_subcommand -l headless -d 'Run without attaching a TUI'
complete -c cmux -n __cmux_no_subcommand -l term -r -d 'TERM for child PTYs'
complete -c cmux -n __cmux_no_subcommand -l cloud -d 'Compose local targets with the Cloud catalog'

# Global options on resource commands
complete -c cmux -n 'not __cmux_no_subcommand' -l machine -r -d 'Routing default: machine'
complete -c cmux -n 'not __cmux_no_subcommand' -l session -r -d 'Routing default: session'
complete -c cmux -n 'not __cmux_no_subcommand' -l socket -r -F -d 'Exact local socket'
complete -c cmux -n 'not __cmux_no_subcommand' -l json -d 'One JSON result or structured error'
complete -c cmux -n 'not __cmux_no_subcommand' -l jsonl -d 'One JSON value per result or stream item'
complete -c cmux -n 'not __cmux_no_subcommand' -l quiet -d 'No successful output'
complete -c cmux -n 'not __cmux_no_subcommand' -l idempotency-key -r -d 'Explicit mutation idempotency key'
complete -c cmux -n 'not __cmux_no_subcommand' -l expected-revision -r -d 'Optimistic concurrency revision'
complete -c cmux -n 'not __cmux_no_subcommand' -l correlation-key -r -d 'Creation correlation key'

# Actions per resource root
complete -c cmux -n '__cmux_using_root machine' -f -a 'list show session current'
complete -c cmux -n '__cmux_using_root session' -f -a 'list open show snapshot events ping shutdown creation config window terminal current'
complete -c cmux -n '__cmux_using_root client' -f -a 'list show detach metadata sizing cell current'
complete -c cmux -n '__cmux_using_root workspace' -f -a 'list create show rename move focus close run layout screen current'
complete -c cmux -n '__cmux_using_root screen' -f -a 'list create show rename focus close layout pane current'
complete -c cmux -n '__cmux_using_root pane' -f -a 'list create show rename focus close split neighbor swap zoom run viewport tab current'
complete -c cmux -n '__cmux_using_root tab' -f -a 'list create show rename move focus close terminal browser current'
complete -c cmux -n '__cmux_using_root terminal' -f -a 'list show write keys mouse copy move attach close focus screen state history process viewport current'
complete -c cmux -n '__cmux_using_root browser' -f -a 'list show navigate back forward reload activate key text attach close mouse wheel current'
complete -c cmux -n '__cmux_using_root notification' -f -a 'list create'
complete -c cmux -n '__cmux_using_root agent' -f -a 'list report'
complete -c cmux -n '__cmux_using_root sidebar' -f -a 'view plugin'
complete -c cmux -n '__cmux_using_root pairing' -f -a 'request'
complete -c cmux -n '__cmux_using_root projection' -f -a 'show put'
complete -c cmux -n '__cmux_using_root provider' -f -a 'authority'
complete -c cmux -n '__cmux_using_root raw' -f -a 'operation command'

# Process-mode options
complete -c cmux -n '__cmux_using_root attach' -l terminal -r -d 'Attach one terminal by exact ID'
complete -c cmux -n '__cmux_using_root attach' -l session -r -d 'Session name'
complete -c cmux -n '__cmux_using_root attach' -l socket -r -F -d 'Explicit socket path'
complete -c cmux -n '__cmux_using_root relay' -l session -r -d 'Session name'
complete -c cmux -n '__cmux_using_root machine-agent' -l session -r -d 'Session name'
