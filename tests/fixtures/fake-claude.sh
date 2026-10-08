#!/bin/sh
# A stand-in for `claude -p` in tests. The scenario is the first `SCENARIO: <name>` line of the
# prompt on stdin, so one static executable serves every test. It plays back the recorded
# streams next to it and, like the real agent, works in its current directory.
here=$(dirname "$0")
prompt=$(cat)
scenario=$(printf '%s\n' "$prompt" | sed -n 's/^SCENARIO: //p' | head -n 1)
printf '%s\n' "$@" > "$(pwd).args"

commit() {
    echo "hello" > HELLO.md
    git add HELLO.md
    git -c user.name=Fake -c user.email=fake@example.com commit -q -m "Add HELLO.md"
}

case "$scenario" in
success)
    commit
    cat "$here/claude-success.jsonl"
    ;;
dirty)
    commit
    echo "leftover" > LEFTOVER.md
    cat "$here/claude-success.jsonl"
    ;;
no_commit)
    cat "$here/claude-success.jsonl"
    ;;
bad_model)
    cat "$here/claude-bad-model.jsonl"
    echo "[claude-code:unrecognized_model] {}" >&2
    exit 1
    ;;
malformed)
    commit
    echo "this is not json"
    head -n 3 "$here/claude-success.jsonl"
    echo '{"type":"assistant","mess'
    tail -n +4 "$here/claude-success.jsonl"
    ;;
killed)
    head -n 5 "$here/claude-success.jsonl"
    echo "about to die" >&2
    kill -9 $$
    ;;
hang)
    sed -n 2p "$here/claude-success.jsonl"
    sleep 60
    ;;
long_line)
    commit
    sed -n 1,2p "$here/claude-success.jsonl"
    head -c 3000000 /dev/zero | tr '\0' 'x'
    echo
    tail -n +3 "$here/claude-success.jsonl"
    ;;
group_child)
    sleep 300 &
    echo $! > "$(pwd).child"
    sed -n 2p "$here/claude-success.jsonl"
    wait
    ;;
session_child)
    setsid sleep 300 &
    child=$!
    trap 'kill $child; exit 143' TERM
    echo $child > "$(pwd).child"
    sed -n 2p "$here/claude-success.jsonl"
    wait
    ;;
silent_success)
    commit
    ;;
*)
    echo "unknown scenario: $scenario" >&2
    exit 2
    ;;
esac
