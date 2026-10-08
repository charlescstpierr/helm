#!/bin/sh
# Offline gh fixture. State lives in <cwd>.gh, outside the tested worktree.
# list.json is an array; view.json, after-create.json, after-ready.json and
# after-merge.json are single PR objects. Mutations replace view.json and list.json.
# Optional <action>.stdout/.stderr/.exit override a command's response, including
# an uncertain mutation that changes state before returning an error.
# commands.log records actions; <action>.args records arguments; create.body is stdin.
set -eu
state="$(pwd).gh"
[ "$1" = pr ] || exit 2
action=$2
mkdir -p "$state"
printf '%s\n' "$action" >> "$state/commands.log"
printf '%s\n' "$@" > "$state/$action.args"
case "$action" in
    create) cat > "$state/create.body" ;;
    list|view|ready|merge) ;;
    *) exit 2 ;;
esac
if [ -f "$state/after-$action.json" ]; then
    cp "$state/after-$action.json" "$state/view.json"
    { printf '['; cat "$state/view.json"; printf ']'; } > "$state/list.json"
fi
if [ -f "$state/$action.stderr" ]; then
    cat "$state/$action.stderr" >&2
fi
if [ -f "$state/$action.stdout" ]; then
    cat "$state/$action.stdout"
elif [ "$action" = list ] || [ "$action" = view ]; then
    cat "$state/$action.json"
elif [ "$action" = create ]; then
    printf '%s\n' 'https://github.com/owner/repo/pull/7'
fi
if [ -f "$state/$action.exit" ]; then
    exit "$(cat "$state/$action.exit")"
fi
