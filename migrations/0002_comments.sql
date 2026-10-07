CREATE TABLE comments (
    id          INTEGER PRIMARY KEY,
    card_id     INTEGER NOT NULL REFERENCES cards (id) ON DELETE CASCADE,
    author_kind TEXT    NOT NULL CHECK (author_kind IN ('human', 'agent', 'system')),
    author      TEXT    NOT NULL CHECK (length(author) > 0),
    body        TEXT    NOT NULL CHECK (length(body) > 0),
    created_at  INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

CREATE INDEX comments_by_card ON comments (card_id, id);

-- `target` has no CHECK: the known targets live in `src/mentions.rs`, and adding an agent
-- must not need a table rebuild.
CREATE TABLE mentions (
    id         INTEGER PRIMARY KEY,
    comment_id INTEGER NOT NULL REFERENCES comments (id) ON DELETE CASCADE,
    target     TEXT    NOT NULL CHECK (length(target) > 0),
    handled_at INTEGER,
    UNIQUE (comment_id, target)
) STRICT;

CREATE INDEX mentions_unhandled ON mentions (target, comment_id) WHERE handled_at IS NULL;
