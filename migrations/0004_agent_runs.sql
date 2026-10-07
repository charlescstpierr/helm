-- Agent assignment is per card. `agent` has no CHECK, like `mentions.target`: the known
-- agents live in `src/agent.rs`, and adding one must not need a table rebuild. `model` NULL
-- means "the project default from helm.toml".
ALTER TABLE cards ADD COLUMN agent TEXT CHECK (agent IS NULL OR length(agent) > 0);
ALTER TABLE cards ADD COLUMN model TEXT CHECK (model IS NULL OR length(model) > 0);

-- One row per launch of an agent on a card. `status` is a state machine whose legal
-- transitions live in `src/runs.rs` (`RunStatus::can_become`); the CHECKs below only pin the
-- invariants that hold in every state:
--   * a run is `succeeded` only once its branch is pushed (`pushed_at`), so a failed push can
--     never be recorded as a success;
--   * `finished_at` is set exactly when the run is no longer queued or running.
CREATE TABLE agent_runs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    card_id         INTEGER NOT NULL REFERENCES cards (id) ON DELETE CASCADE,
    agent           TEXT    NOT NULL CHECK (length(agent) > 0),
    -- The `--model` the run was launched with; NULL when the CLI picked its own default.
    model           TEXT,
    permission_mode TEXT    NOT NULL CHECK (length(permission_mode) > 0),
    status          TEXT    NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled', 'interrupted')),
    prompt          TEXT    NOT NULL,
    -- Recorded as soon as the CLI reports it; the key to a later resume.
    session_id      TEXT,
    resumed_from    INTEGER REFERENCES agent_runs (id) ON DELETE SET NULL,
    worktree_path   TEXT,
    branch          TEXT,
    pid             INTEGER,
    exit_code       INTEGER,
    -- Why the run is `failed` or `interrupted`.
    error           TEXT,
    stderr          TEXT    NOT NULL DEFAULT '',
    cost_usd        REAL,
    tokens_in       INTEGER,
    tokens_out      INTEGER,
    queued_at       INTEGER NOT NULL DEFAULT (unixepoch()),
    started_at      INTEGER,
    finished_at     INTEGER,
    pushed_at       INTEGER,
    CHECK (status <> 'succeeded' OR pushed_at IS NOT NULL),
    CHECK ((status IN ('queued', 'running')) = (finished_at IS NULL))
) STRICT;

CREATE INDEX agent_runs_by_card ON agent_runs (card_id, id);

-- At most one active run per card, enforced by the database rather than by a check-then-insert.
CREATE UNIQUE INDEX agent_runs_one_active_per_card ON agent_runs (card_id)
    WHERE status IN ('queued', 'running');

-- Append-only log of a run. `payload` is the CLI's own JSON line, untouched; `summary` is
-- the one-line rendering the card shows. `seq` is dense per run, so a reader can ask for
-- "everything after seq N".
CREATE TABLE agent_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id     INTEGER NOT NULL REFERENCES agent_runs (id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    kind       TEXT    NOT NULL CHECK (length(kind) > 0),
    summary    TEXT    NOT NULL,
    payload    TEXT    NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (run_id, seq)
) STRICT;
