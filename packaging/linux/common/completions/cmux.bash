# bash completion for cmux
#
# Covers the noun-first public grammar from cmux-tui/spec/cli.md: process
# modes, resource roots, and each root's action verbs. Selectors are completed
# only as `current`, since resolving real IDs would require talking to a live
# session on every <TAB>.

_cmux_modes="attach relay machine-agent"

_cmux_roots="machine session client workspace screen pane tab terminal browser
notification agent sidebar pairing projection provider raw"

_cmux_global_opts="--machine --session --socket --json --jsonl --quiet --help"
_cmux_start_opts="--session --socket --headless --term --cloud --help"

_cmux_actions_for() {
  case "$1" in
    machine)      echo "list show session" ;;
    session)      echo "list open show snapshot events ping shutdown creation config window terminal" ;;
    client)       echo "list show detach metadata sizing cell" ;;
    workspace)    echo "list create show rename move focus close run layout screen" ;;
    screen)       echo "list create show rename focus close layout pane" ;;
    pane)         echo "list create show rename focus close split neighbor swap zoom run viewport tab" ;;
    tab)          echo "list create show rename move focus close terminal browser" ;;
    terminal)     echo "list show write keys mouse copy move attach close focus screen state history process viewport" ;;
    browser)      echo "list show navigate back forward reload activate key text attach close mouse wheel" ;;
    notification) echo "list create" ;;
    agent)        echo "list report" ;;
    sidebar)      echo "view plugin" ;;
    pairing)      echo "request" ;;
    projection)   echo "show put" ;;
    provider)     echo "authority" ;;
    raw)          echo "operation command" ;;
  esac
}

_cmux() {
  local cur prev words cword
  _init_completion -n : 2>/dev/null || {
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    words=("${COMP_WORDS[@]}")
    cword=$COMP_CWORD
  }

  # Option arguments that take a value we cannot usefully enumerate.
  case "$prev" in
    --socket)
      _filedir
      return
      ;;
    --session|--machine|--term|--terminal|--idempotency-key|--expected-revision|--correlation-key|--params-json|--request-json|--name)
      return
      ;;
  esac

  # Find the first non-option word after `cmux`; that is the mode or resource.
  local i root="" action=""
  for ((i = 1; i < cword; i++)); do
    case "${words[i]}" in
      -*) continue ;;
    esac
    if [ -z "$root" ]; then
      root="${words[i]}"
    elif [ -z "$action" ]; then
      action="${words[i]}"
    fi
  done

  if [ -z "$root" ]; then
    if [[ "$cur" == -* ]]; then
      COMPREPLY=($(compgen -W "$_cmux_start_opts $_cmux_global_opts" -- "$cur"))
    else
      COMPREPLY=($(compgen -W "$_cmux_modes $_cmux_roots" -- "$cur"))
    fi
    return
  fi

  case "$root" in
    attach)
      COMPREPLY=($(compgen -W "$_cmux_start_opts --terminal" -- "$cur"))
      return
      ;;
    relay|machine-agent)
      COMPREPLY=($(compgen -W "--session --socket --machine --help" -- "$cur"))
      return
      ;;
  esac

  if [[ "$cur" == -* ]]; then
    COMPREPLY=($(compgen -W "$_cmux_global_opts --idempotency-key --expected-revision --correlation-key" -- "$cur"))
    return
  fi

  local actions
  actions="$(_cmux_actions_for "$root")"
  if [ -z "$action" ]; then
    # Position two is either an action or a selector.
    COMPREPLY=($(compgen -W "$actions current" -- "$cur"))
  else
    COMPREPLY=($(compgen -W "$actions current" -- "$cur"))
  fi
}

complete -F _cmux cmux cmux-tui
