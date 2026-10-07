//! Board domain: the data model as the UI sees it, and every card operation.
//!
//! Functions here are synchronous and take a plain connection so they can be unit-tested
//! against an in-memory database; handlers run them through [`crate::db::Db::call`].

use std::collections::HashMap;
use std::fmt;

use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::db::DbError;

pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_DESCRIPTION_CHARS: usize = 20_000;
pub const MAX_LABELS_PER_CARD: usize = 10;
pub const MAX_LABEL_CHARS: usize = 32;
/// Number of `--label-N` colour slots defined in `assets/tokens.css`.
pub const LABEL_COLOR_SLOTS: u32 = 8;

/// `(value, slug, display name)` for each priority, lowest first.
pub const PRIORITIES: [(i64, &str, &str); 5] = [
    (0, "none", "Aucune"),
    (1, "low", "Basse"),
    (2, "medium", "Moyenne"),
    (3, "high", "Haute"),
    (4, "urgent", "Urgente"),
];

#[derive(Debug)]
pub enum StoreError {
    NotFound,
    /// The request is well-formed but breaks a rule; the message is shown to the user.
    Invalid(String),
    Db(DbError),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("not found"),
            Self::Invalid(message) => f.write_str(message),
            Self::Db(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<DbError> for StoreError {
    fn from(e: DbError) -> Self {
        Self::Db(e)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(DbError::Sqlite(e))
    }
}

type Result<T> = std::result::Result<T, StoreError>;

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(StoreError::Invalid(message.into()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: i64,
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    pub color_slot: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub id: i64,
    pub column_id: i64,
    pub number: i64,
    pub title: String,
    pub description: String,
    pub priority: i64,
    pub labels: Vec<Label>,
}

impl Card {
    fn priority_entry(&self) -> (i64, &'static str, &'static str) {
        PRIORITIES
            .iter()
            .copied()
            .find(|(value, _, _)| *value == self.priority)
            .unwrap_or(PRIORITIES[0])
    }

    pub fn priority_slug(&self) -> &'static str {
        self.priority_entry().1
    }

    pub fn priority_name(&self) -> &'static str {
        self.priority_entry().2
    }

    pub fn has_priority(&self) -> bool {
        self.priority > 0
    }

    /// Labels as the comma-separated text the edit form round-trips.
    pub fn labels_text(&self) -> String {
        let names: Vec<&str> = self.labels.iter().map(|l| l.name.as_str()).collect();
        names.join(", ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub id: i64,
    pub name: String,
    pub category: String,
    pub cards: Vec<Card>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    pub project: Project,
    pub columns: Vec<Column>,
}

/// User-supplied card fields, before validation.
#[derive(Debug, Clone, Default)]
pub struct CardInput {
    pub title: String,
    pub description: String,
    pub priority: i64,
    /// Comma-separated label names.
    pub labels: String,
}

struct ValidCard {
    title: String,
    description: String,
    priority: i64,
    labels: Vec<String>,
}

impl CardInput {
    fn validate(&self) -> Result<ValidCard> {
        let title = self.title.trim();
        if title.is_empty() {
            return invalid("Le titre est obligatoire.");
        }
        if title.chars().count() > MAX_TITLE_CHARS {
            return invalid(format!("Le titre dépasse {MAX_TITLE_CHARS} caractères."));
        }
        let description = self.description.replace("\r\n", "\n");
        let description = description.trim_end();
        if description.chars().count() > MAX_DESCRIPTION_CHARS {
            return invalid(format!(
                "La description dépasse {MAX_DESCRIPTION_CHARS} caractères."
            ));
        }
        if !PRIORITIES
            .iter()
            .any(|(value, _, _)| *value == self.priority)
        {
            return invalid("Priorité inconnue.");
        }
        Ok(ValidCard {
            title: title.to_owned(),
            description: description.to_owned(),
            priority: self.priority,
            labels: parse_labels(&self.labels)?,
        })
    }
}

/// Splits comma-separated label names, trimming and de-duplicating case-insensitively.
pub fn parse_labels(text: &str) -> Result<Vec<String>> {
    let mut labels: Vec<String> = Vec::new();
    for name in text.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if name.chars().count() > MAX_LABEL_CHARS {
            return invalid(format!(
                "Une étiquette dépasse {MAX_LABEL_CHARS} caractères."
            ));
        }
        let lowered = name.to_lowercase();
        if labels.iter().any(|known| known.to_lowercase() == lowered) {
            continue;
        }
        // Checked as we go so an oversized list is rejected without being fully scanned.
        if labels.len() == MAX_LABELS_PER_CARD {
            return invalid(format!(
                "Pas plus de {MAX_LABELS_PER_CARD} étiquettes par carte."
            ));
        }
        labels.push(name.to_owned());
    }
    Ok(labels)
}

/// Stable colour slot for a label name (FNV-1a), so a label keeps its colour everywhere.
fn color_slot(name: &str) -> i64 {
    let hash = name
        .to_lowercase()
        .bytes()
        .fold(0x811c_9dc5_u32, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
        });
    i64::from(hash % LABEL_COLOR_SLOTS)
}

/// The project shown by the single-board UI: the oldest one.
pub fn default_project(conn: &Connection) -> Result<Project> {
    conn.query_row(
        "SELECT id, key, name FROM projects ORDER BY id LIMIT 1",
        [],
        |row| {
            Ok(Project {
                id: row.get(0)?,
                key: row.get(1)?,
                name: row.get(2)?,
            })
        },
    )
    .optional()?
    .ok_or(StoreError::NotFound)
}

pub fn load_board(conn: &Connection) -> Result<Board> {
    let project = default_project(conn)?;

    let mut columns: Vec<Column> = conn
        .prepare(
            "SELECT id, name, category FROM board_columns
             WHERE project_id = ?1 ORDER BY position",
        )?
        .query_map([project.id], |row| {
            Ok(Column {
                id: row.get(0)?,
                name: row.get(1)?,
                category: row.get(2)?,
                cards: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut labels = labels_by_card(conn, "c.project_id = ?1", project.id)?;
    let cards: Vec<Card> = conn
        .prepare(
            "SELECT id, column_id, number, title, description, priority FROM cards
             WHERE project_id = ?1 ORDER BY column_id, position, id",
        )?
        .query_map([project.id], card_from_row)?
        .collect::<rusqlite::Result<_>>()?;

    let index: HashMap<i64, usize> = columns
        .iter()
        .enumerate()
        .map(|(i, column)| (column.id, i))
        .collect();
    for mut card in cards {
        card.labels = labels.remove(&card.id).unwrap_or_default();
        if let Some(&i) = index.get(&card.column_id) {
            columns[i].cards.push(card);
        }
    }
    Ok(Board { project, columns })
}

pub fn get_card(conn: &Connection, id: i64) -> Result<Card> {
    let mut card = conn
        .query_row(
            "SELECT id, column_id, number, title, description, priority FROM cards
             WHERE id = ?1",
            [id],
            card_from_row,
        )
        .optional()?
        .ok_or(StoreError::NotFound)?;
    card.labels = labels_by_card(conn, "c.id = ?1", id)?
        .remove(&id)
        .unwrap_or_default();
    Ok(card)
}

fn card_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Card> {
    Ok(Card {
        id: row.get(0)?,
        column_id: row.get(1)?,
        number: row.get(2)?,
        title: row.get(3)?,
        description: row.get(4)?,
        priority: row.get(5)?,
        labels: Vec::new(),
    })
}

/// `filter` is a trusted SQL fragment over `cards c` with one `?1` parameter.
fn labels_by_card(conn: &Connection, filter: &str, param: i64) -> Result<HashMap<i64, Vec<Label>>> {
    let mut by_card: HashMap<i64, Vec<Label>> = HashMap::new();
    let mut statement = conn.prepare(&format!(
        "SELECT cl.card_id, l.name, l.color_slot
         FROM card_labels cl
         JOIN labels l ON l.id = cl.label_id
         JOIN cards c ON c.id = cl.card_id
         WHERE {filter}
         ORDER BY l.name_key"
    ))?;
    let rows = statement.query_map([param], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            Label {
                name: row.get(1)?,
                color_slot: row.get(2)?,
            },
        ))
    })?;
    for row in rows {
        let (card_id, label) = row?;
        by_card.entry(card_id).or_default().push(label);
    }
    Ok(by_card)
}

/// Creates a card at the bottom of `column_id` and returns its id.
pub fn create_card(conn: &mut Connection, column_id: i64, input: &CardInput) -> Result<i64> {
    let card = input.validate()?;
    let tx = conn.transaction()?;
    let Some(project_id) = column_project(&tx, column_id)? else {
        return invalid("Colonne inconnue.");
    };
    let number: i64 = tx.query_row(
        "UPDATE projects SET next_card_number = next_card_number + 1
         WHERE id = ?1 RETURNING next_card_number - 1",
        [project_id],
        |row| row.get(0),
    )?;
    let position: i64 = tx.query_row(
        "SELECT COUNT(*) FROM cards WHERE column_id = ?1",
        [column_id],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO cards (project_id, column_id, number, title, description, priority, position)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        (
            project_id,
            column_id,
            number,
            &card.title,
            &card.description,
            card.priority,
            position,
        ),
    )?;
    let id = tx.last_insert_rowid();
    set_labels(&tx, project_id, id, &card.labels)?;
    tx.commit()?;
    Ok(id)
}

/// Updates a card's fields; changing `column_id` sends it to the bottom of the new column.
pub fn update_card(
    conn: &mut Connection,
    id: i64,
    column_id: i64,
    input: &CardInput,
) -> Result<()> {
    let card = input.validate()?;
    let tx = conn.transaction()?;
    let (project_id, current_column) = card_location(&tx, id)?;
    tx.execute(
        "UPDATE cards SET title = ?1, description = ?2, priority = ?3, updated_at = unixepoch()
         WHERE id = ?4",
        (&card.title, &card.description, card.priority, id),
    )?;
    set_labels(&tx, project_id, id, &card.labels)?;
    if column_id != current_column {
        place_card(&tx, id, project_id, current_column, column_id, usize::MAX)?;
    }
    tx.commit()?;
    Ok(())
}

/// Moves a card to `index` (0-based, clamped) in `column_id`, keeping positions dense.
pub fn move_card(conn: &mut Connection, id: i64, column_id: i64, index: usize) -> Result<()> {
    let tx = conn.transaction()?;
    let (project_id, current_column) = card_location(&tx, id)?;
    place_card(&tx, id, project_id, current_column, column_id, index)?;
    tx.commit()?;
    Ok(())
}

pub fn delete_card(conn: &mut Connection, id: i64) -> Result<()> {
    let tx = conn.transaction()?;
    let (project_id, column_id) = card_location(&tx, id)?;
    tx.execute("DELETE FROM cards WHERE id = ?1", [id])?;
    let remaining = column_card_ids(&tx, column_id, id)?;
    renumber(&tx, column_id, &remaining)?;
    prune_labels(&tx, project_id)?;
    tx.commit()?;
    Ok(())
}

fn column_project(tx: &Transaction<'_>, column_id: i64) -> Result<Option<i64>> {
    Ok(tx
        .query_row(
            "SELECT project_id FROM board_columns WHERE id = ?1",
            [column_id],
            |row| row.get(0),
        )
        .optional()?)
}

/// Returns `(project_id, column_id)` of a card.
fn card_location(tx: &Transaction<'_>, id: i64) -> Result<(i64, i64)> {
    tx.query_row(
        "SELECT project_id, column_id FROM cards WHERE id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()?
    .ok_or(StoreError::NotFound)
}

/// Ordered card ids of a column, leaving `except` out.
fn column_card_ids(tx: &Transaction<'_>, column_id: i64, except: i64) -> Result<Vec<i64>> {
    Ok(tx
        .prepare("SELECT id FROM cards WHERE column_id = ?1 AND id <> ?2 ORDER BY position, id")?
        .query_map([column_id, except], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn renumber(tx: &Transaction<'_>, column_id: i64, ids: &[i64]) -> Result<()> {
    let mut statement =
        tx.prepare("UPDATE cards SET column_id = ?1, position = ?2 WHERE id = ?3")?;
    for (position, id) in ids.iter().enumerate() {
        statement.execute((column_id, position as i64, id))?;
    }
    Ok(())
}

fn place_card(
    tx: &Transaction<'_>,
    id: i64,
    project_id: i64,
    from_column: i64,
    to_column: i64,
    index: usize,
) -> Result<()> {
    if column_project(tx, to_column)? != Some(project_id) {
        return invalid("Colonne inconnue.");
    }
    let mut target = column_card_ids(tx, to_column, id)?;
    target.insert(index.min(target.len()), id);
    renumber(tx, to_column, &target)?;
    if from_column != to_column {
        let source = column_card_ids(tx, from_column, id)?;
        renumber(tx, from_column, &source)?;
    }
    tx.execute(
        "UPDATE cards SET updated_at = unixepoch() WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

/// Replaces a card's labels, creating project labels on demand and dropping unused ones.
fn set_labels(tx: &Transaction<'_>, project_id: i64, card_id: i64, names: &[String]) -> Result<()> {
    tx.execute("DELETE FROM card_labels WHERE card_id = ?1", [card_id])?;
    for name in names {
        let key = name.to_lowercase();
        tx.execute(
            "INSERT INTO labels (project_id, name, name_key, color_slot) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (project_id, name_key) DO NOTHING",
            (project_id, name, &key, color_slot(name)),
        )?;
        tx.execute(
            "INSERT INTO card_labels (card_id, label_id)
             SELECT ?1, id FROM labels WHERE project_id = ?2 AND name_key = ?3",
            (card_id, project_id, &key),
        )?;
    }
    prune_labels(tx, project_id)
}

fn prune_labels(tx: &Transaction<'_>, project_id: i64) -> Result<()> {
    tx.execute(
        "DELETE FROM labels
         WHERE project_id = ?1 AND id NOT IN (SELECT label_id FROM card_labels)",
        [project_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        crate::db::Db::test_connection()
    }

    fn input(title: &str) -> CardInput {
        CardInput {
            title: title.to_owned(),
            ..CardInput::default()
        }
    }

    fn column_ids(conn: &Connection) -> Vec<i64> {
        load_board(conn)
            .unwrap()
            .columns
            .iter()
            .map(|c| c.id)
            .collect()
    }

    /// Titles per column, in display order.
    fn layout(conn: &Connection) -> Vec<Vec<String>> {
        load_board(conn)
            .unwrap()
            .columns
            .into_iter()
            .map(|column| column.cards.into_iter().map(|card| card.title).collect())
            .collect()
    }

    fn assert_dense_positions(conn: &Connection) {
        let mut statement = conn
            .prepare("SELECT column_id, position FROM cards ORDER BY column_id, position")
            .unwrap();
        let rows: Vec<(i64, i64)> = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut expected: HashMap<i64, i64> = HashMap::new();
        for (column, position) in rows {
            let next = expected.entry(column).or_insert(0);
            assert_eq!(position, *next, "gap or duplicate in column {column}");
            *next += 1;
        }
    }

    #[test]
    fn new_cards_are_appended_and_numbered_per_project() {
        let mut conn = test_conn();
        let columns = column_ids(&conn);
        assert_eq!(columns.len(), 5);

        let a = create_card(&mut conn, columns[0], &input("  A  ")).unwrap();
        let b = create_card(&mut conn, columns[0], &input("B")).unwrap();
        let c = create_card(&mut conn, columns[1], &input("C")).unwrap();

        assert_eq!(layout(&conn)[0], ["A", "B"]);
        assert_eq!(layout(&conn)[1], ["C"]);
        let numbers: Vec<i64> = [a, b, c]
            .iter()
            .map(|id| get_card(&conn, *id).unwrap().number)
            .collect();
        assert_eq!(numbers, [1, 2, 3]);
    }

    #[test]
    fn card_numbers_are_never_reused_after_deletion() {
        let mut conn = test_conn();
        let column = column_ids(&conn)[0];
        let first = create_card(&mut conn, column, &input("first")).unwrap();
        delete_card(&mut conn, first).unwrap();
        let second = create_card(&mut conn, column, &input("second")).unwrap();
        assert_eq!(get_card(&conn, second).unwrap().number, 2);
    }

    #[test]
    fn moving_within_a_column_reorders_it() {
        let mut conn = test_conn();
        let column = column_ids(&conn)[0];
        let ids: Vec<i64> = ["A", "B", "C", "D"]
            .iter()
            .map(|t| create_card(&mut conn, column, &input(t)).unwrap())
            .collect();

        move_card(&mut conn, ids[3], column, 0).unwrap();
        assert_eq!(layout(&conn)[0], ["D", "A", "B", "C"]);

        move_card(&mut conn, ids[3], column, 2).unwrap();
        assert_eq!(layout(&conn)[0], ["A", "B", "D", "C"]);

        // An out-of-range index clamps to the end; moving onto itself is a no-op.
        move_card(&mut conn, ids[0], column, 99).unwrap();
        assert_eq!(layout(&conn)[0], ["B", "D", "C", "A"]);
        move_card(&mut conn, ids[0], column, 3).unwrap();
        assert_eq!(layout(&conn)[0], ["B", "D", "C", "A"]);
        assert_dense_positions(&conn);
    }

    #[test]
    fn moving_across_columns_keeps_both_dense() {
        let mut conn = test_conn();
        let columns = column_ids(&conn);
        let ids: Vec<i64> = ["A", "B", "C"]
            .iter()
            .map(|t| create_card(&mut conn, columns[0], &input(t)).unwrap())
            .collect();
        create_card(&mut conn, columns[2], &input("X")).unwrap();
        create_card(&mut conn, columns[2], &input("Y")).unwrap();

        move_card(&mut conn, ids[1], columns[2], 1).unwrap();
        let layout = layout(&conn);
        assert_eq!(layout[0], ["A", "C"]);
        assert_eq!(layout[2], ["X", "B", "Y"]);
        assert_dense_positions(&conn);
    }

    #[test]
    fn move_rejects_unknown_card_or_column() {
        let mut conn = test_conn();
        let column = column_ids(&conn)[0];
        let id = create_card(&mut conn, column, &input("A")).unwrap();

        assert!(matches!(
            move_card(&mut conn, 999, column, 0),
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            move_card(&mut conn, id, 999, 0),
            Err(StoreError::Invalid(_))
        ));
        assert_eq!(layout(&conn)[0], ["A"]);
    }

    #[test]
    fn update_edits_fields_and_can_change_column() {
        let mut conn = test_conn();
        let columns = column_ids(&conn);
        let id = create_card(&mut conn, columns[0], &input("Draft")).unwrap();
        create_card(&mut conn, columns[0], &input("Other")).unwrap();
        create_card(&mut conn, columns[4], &input("Shipped")).unwrap();

        let edited = CardInput {
            title: "Final".to_owned(),
            description: "line 1\r\nline 2  \n".to_owned(),
            priority: 4,
            labels: "bug, UI".to_owned(),
        };
        update_card(&mut conn, id, columns[4], &edited).unwrap();

        let card = get_card(&conn, id).unwrap();
        assert_eq!(card.title, "Final");
        assert_eq!(card.description, "line 1\nline 2");
        assert_eq!(card.priority_slug(), "urgent");
        assert_eq!(card.labels_text(), "bug, UI");
        let layout = layout(&conn);
        assert_eq!(layout[0], ["Other"]);
        assert_eq!(layout[4], ["Shipped", "Final"]);
        assert_dense_positions(&conn);
    }

    #[test]
    fn delete_closes_the_gap() {
        let mut conn = test_conn();
        let column = column_ids(&conn)[0];
        let ids: Vec<i64> = ["A", "B", "C"]
            .iter()
            .map(|t| create_card(&mut conn, column, &input(t)).unwrap())
            .collect();

        delete_card(&mut conn, ids[1]).unwrap();
        assert_eq!(layout(&conn)[0], ["A", "C"]);
        assert_dense_positions(&conn);
        assert!(matches!(get_card(&conn, ids[1]), Err(StoreError::NotFound)));
        assert!(matches!(
            delete_card(&mut conn, ids[1]),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn labels_are_shared_case_insensitively_and_pruned_when_unused() {
        let mut conn = test_conn();
        let column = column_ids(&conn)[0];
        let label_count = |conn: &Connection| -> i64 {
            conn.query_row("SELECT COUNT(*) FROM labels", [], |r| r.get(0))
                .unwrap()
        };

        let mut first = input("A");
        first.labels = "Bug, infra, bug, ".to_owned();
        let a = create_card(&mut conn, column, &first).unwrap();
        let mut second = input("B");
        second.labels = "BUG".to_owned();
        let b = create_card(&mut conn, column, &second).unwrap();

        // "BUG" reuses the existing "Bug" label, with the same colour slot.
        assert_eq!(label_count(&conn), 2);
        let card_a = get_card(&conn, a).unwrap();
        let card_b = get_card(&conn, b).unwrap();
        assert_eq!(card_a.labels_text(), "Bug, infra");
        assert_eq!(card_b.labels, [card_a.labels[0].clone()]);
        assert!((0..i64::from(LABEL_COLOR_SLOTS)).contains(&card_a.labels[0].color_slot));

        delete_card(&mut conn, a).unwrap();
        assert_eq!(label_count(&conn), 1);
        update_card(&mut conn, b, column, &input("B")).unwrap();
        assert_eq!(label_count(&conn), 0);
    }

    #[test]
    fn accented_labels_are_shared_across_cards() {
        let mut conn = test_conn();
        let column = column_ids(&conn)[0];
        let mut first = input("A");
        first.labels = "Équipe".to_owned();
        let a = create_card(&mut conn, column, &first).unwrap();
        let mut second = input("B");
        second.labels = "équipe".to_owned();
        let b = create_card(&mut conn, column, &second).unwrap();

        assert_eq!(
            get_card(&conn, a).unwrap().labels,
            get_card(&conn, b).unwrap().labels
        );
        assert_eq!(get_card(&conn, b).unwrap().labels_text(), "Équipe");
    }

    #[test]
    fn card_ids_are_never_reused_after_deletion() {
        let mut conn = test_conn();
        let column = column_ids(&conn)[0];
        let first = create_card(&mut conn, column, &input("first")).unwrap();
        delete_card(&mut conn, first).unwrap();
        let second = create_card(&mut conn, column, &input("second")).unwrap();

        assert_ne!(first, second);
        assert!(matches!(get_card(&conn, first), Err(StoreError::NotFound)));
        assert!(matches!(
            delete_card(&mut conn, first),
            Err(StoreError::NotFound)
        ));
        assert_eq!(layout(&conn)[0], ["second"]);
    }

    #[test]
    fn validation_rejects_bad_input_without_writing() {
        let mut conn = test_conn();
        let column = column_ids(&conn)[0];
        let rejected = |conn: &mut Connection, input: CardInput| {
            assert!(matches!(
                create_card(conn, column, &input),
                Err(StoreError::Invalid(_))
            ));
        };

        rejected(&mut conn, input("   "));
        rejected(&mut conn, input(&"x".repeat(MAX_TITLE_CHARS + 1)));
        rejected(
            &mut conn,
            CardInput {
                priority: 5,
                ..input("A")
            },
        );
        rejected(
            &mut conn,
            CardInput {
                labels: "y".repeat(MAX_LABEL_CHARS + 1),
                ..input("A")
            },
        );
        rejected(
            &mut conn,
            CardInput {
                labels: (0..=MAX_LABELS_PER_CARD)
                    .map(|i| format!("l{i}"))
                    .collect::<Vec<_>>()
                    .join(","),
                ..input("A")
            },
        );
        assert!(matches!(
            create_card(&mut conn, 999, &input("A")),
            Err(StoreError::Invalid(_))
        ));

        assert!(layout(&conn).iter().all(Vec::is_empty));
        // A rejected creation must not burn a card number.
        let id = create_card(&mut conn, column, &input("A")).unwrap();
        assert_eq!(get_card(&conn, id).unwrap().number, 1);
    }

    #[test]
    fn title_limit_counts_characters_not_bytes() {
        let mut conn = test_conn();
        let column = column_ids(&conn)[0];
        let title = "é".repeat(MAX_TITLE_CHARS);
        assert!(create_card(&mut conn, column, &input(&title)).is_ok());
    }
}
