-- `comments.id` and `mentions.id` were plain rowids: after the newest row was deleted (a card
-- delete cascades to its comments), SQLite handed the same id to the next insert. A stale
-- link or an orchestrator cursor on an old comment id could then land on someone else's
-- comment. AUTOINCREMENT makes ids monotonic, as `cards.id` already is.
--
-- SQLite cannot add AUTOINCREMENT in place, so both tables are rebuilt. `mentions` is
-- rebuilt too and dropped first: dropping `comments` under `foreign_keys = ON` would cascade
-- into it and delete every mention.
CREATE TABLE comments_new (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    card_id     INTEGER NOT NULL REFERENCES cards (id) ON DELETE CASCADE,
    author_kind TEXT    NOT NULL CHECK (author_kind IN ('human', 'agent', 'system')),
    author      TEXT    NOT NULL CHECK (length(author) > 0),
    body        TEXT    NOT NULL CHECK (length(body) > 0),
    created_at  INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

INSERT INTO comments_new (id, card_id, author_kind, author, body, created_at)
    SELECT id, card_id, author_kind, author, body, created_at FROM comments;

CREATE TABLE mentions_new (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    comment_id INTEGER NOT NULL REFERENCES comments_new (id) ON DELETE CASCADE,
    target     TEXT    NOT NULL CHECK (length(target) > 0),
    handled_at INTEGER,
    UNIQUE (comment_id, target)
) STRICT;

INSERT INTO mentions_new (id, comment_id, target, handled_at)
    SELECT id, comment_id, target, handled_at FROM mentions;

DROP TABLE mentions;
DROP TABLE comments;
ALTER TABLE comments_new RENAME TO comments;
ALTER TABLE mentions_new RENAME TO mentions;

CREATE INDEX comments_by_card ON comments (card_id, id);
CREATE INDEX mentions_unhandled ON mentions (target, comment_id) WHERE handled_at IS NULL;
