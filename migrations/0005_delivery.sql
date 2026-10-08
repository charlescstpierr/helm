-- Helm's own checks are tied to the exact commit produced by a run.
CREATE TABLE run_verifications (
    run_id     INTEGER PRIMARY KEY REFERENCES agent_runs (id) ON DELETE CASCADE,
    commit_sha TEXT NOT NULL CHECK (length(commit_sha) > 0),
    status     TEXT NOT NULL CHECK (status IN ('running', 'passed', 'failed', 'skipped', 'interrupted'))
) STRICT;

CREATE TABLE run_checks (
    run_id    INTEGER NOT NULL REFERENCES run_verifications (run_id) ON DELETE CASCADE,
    position  INTEGER NOT NULL CHECK (position >= 0),
    command   TEXT NOT NULL CHECK (length(trim(command)) > 0),
    status    TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'running', 'passed', 'failed', 'interrupted')),
    stdout    TEXT NOT NULL DEFAULT '',
    stderr    TEXT NOT NULL DEFAULT '',
    exit_code INTEGER,
    error     TEXT,
    PRIMARY KEY (run_id, position),
    CHECK (status <> 'passed' OR (exit_code IS NOT NULL AND exit_code = 0))
) STRICT;

CREATE TABLE card_pull_requests (
    card_id      INTEGER PRIMARY KEY REFERENCES cards (id) ON DELETE CASCADE,
    run_id       INTEGER NOT NULL REFERENCES agent_runs (id) ON DELETE CASCADE,
    repository   TEXT NOT NULL CHECK (length(repository) > 0),
    number       INTEGER NOT NULL CHECK (number > 0),
    expected_base TEXT NOT NULL CHECK (length(expected_base) > 0),
    snapshot     TEXT NOT NULL CHECK (json_valid(snapshot)),
    refreshed_at INTEGER NOT NULL,
    error        TEXT,
    UNIQUE (repository, number)
) STRICT;
