#!/bin/sh
# A stand-in for `claude -p` in tests. The scenario is the first `SCENARIO: <name>` line of the
# prompt on stdin, so one static executable serves every test. It plays back the recorded
# streams next to it and, like the real agent, works in its current directory.
here=$(dirname "$0")
prompt=$(cat)
scenario=$(printf '%s\n' "$prompt" | sed -n 's/^SCENARIO: //p' | head -n 1)
printf '%s\n' "$@" > "$(pwd).args"
printf '%s' "$prompt" > "$(pwd).prompt"
resume_session=
while [ "$#" -gt 0 ]; do
    if [ "$1" = --resume ]; then
        resume_session=$2
        shift
    fi
    shift
done

commit() {
    echo "hello" > HELLO.md
    git add HELLO.md
    git -c user.name=Fake -c user.email=fake@example.com commit -q -m "Add HELLO.md"
}

case "$scenario" in
review|resume_expired|resume_wrong_session|resume_unconfirmed)
    if [ -n "$resume_session" ]; then
        if [ "$scenario" = resume_expired ]; then
            echo "No conversation found with session ID: $resume_session" >&2
            exit 1
        fi
        printf 'reviewed\n' >> HELLO.md
        git add HELLO.md
        git -c user.name=Fake -c user.email=fake@example.com commit -q -m "Apply review feedback"
    elif [ -e HELLO.md ]; then
        echo "A review must resume the previous session" >&2
        exit 1
    else
        commit
    fi
    if [ "$scenario" = resume_wrong_session ] && [ -n "$resume_session" ]; then
        sed 's/d157be31-f3e0-44f0-9aa9-7c88253236bf/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/g' "$here/claude-success.jsonl"
    elif [ "$scenario" = resume_unconfirmed ] && [ -n "$resume_session" ]; then
        sed 's/,"session_id":"[^"]*"//g; s/"session_id":"[^"]*",//g' "$here/claude-success.jsonl"
    else
        cat "$here/claude-success.jsonl"
    fi
    ;;
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
wait_then_success)
    sed -n 1,2p "$here/claude-success.jsonl"
    while [ ! -e "$(pwd).go" ]; do sleep 0.05; done
    commit
    tail -n +3 "$here/claude-success.jsonl"
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
