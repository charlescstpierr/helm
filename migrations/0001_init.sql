-- Core kanban schema: projects, columns (statuses), cards, labels.

CREATE TABLE projects (
    id               INTEGER PRIMARY KEY,
    key              TEXT    NOT NULL UNIQUE,
    name             TEXT    NOT NULL,
    next_card_number INTEGER NOT NULL DEFAULT 1,
    created_at       INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- `category` is the stable, machine-readable status; `name` is only a display label.
CREATE TABLE board_columns (
    id         INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    category   TEXT    NOT NULL
        CHECK (category IN ('backlog', 'todo', 'in_progress', 'in_review', 'done')),
    position   INTEGER NOT NULL,
    UNIQUE (project_id, position)
) STRICT;

-- `position` is a dense 0-based rank inside the column, renumbered on every move.
-- `priority`: 0 none, 1 low, 2 medium, 3 high, 4 urgent.
-- AUTOINCREMENT: a deleted card's id is never handed to a later card, so a stale form or
-- link can only ever hit a 404, not somebody else's card.
CREATE TABLE cards (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  INTEGER NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    column_id   INTEGER NOT NULL REFERENCES board_columns (id) ON DELETE RESTRICT,
    number      INTEGER NOT NULL,
    title       TEXT    NOT NULL CHECK (length(title) > 0),
    description TEXT    NOT NULL DEFAULT '',
    priority    INTEGER NOT NULL DEFAULT 0 CHECK (priority BETWEEN 0 AND 4),
    position    INTEGER NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (project_id, number)
) STRICT;

CREATE INDEX cards_by_column ON cards (column_id, position);

-- `color_slot` indexes the `--label-N` CSS variables; no colour value is stored.
CREATE TABLE labels (
    id         INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    -- Unicode-lowercased name: SQLite's NOCASE only folds ASCII.
    name_key   TEXT    NOT NULL,
    color_slot INTEGER NOT NULL DEFAULT 0 CHECK (color_slot BETWEEN 0 AND 7),
    UNIQUE (project_id, name_key)
) STRICT;

CREATE TABLE card_labels (
    card_id  INTEGER NOT NULL REFERENCES cards (id) ON DELETE CASCADE,
    label_id INTEGER NOT NULL REFERENCES labels (id) ON DELETE CASCADE,
    PRIMARY KEY (card_id, label_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX card_labels_by_label ON card_labels (label_id);

INSERT INTO projects (id, key, name) VALUES (1, 'HELM', 'Helm');

INSERT INTO board_columns (project_id, name, category, position) VALUES
    (1, 'Backlog',   'backlog',     0),
    (1, 'À faire',   'todo',        1),
    (1, 'En cours',  'in_progress', 2),
    (1, 'En revue',  'in_review',   3),
    (1, 'Terminé',   'done',        4);
