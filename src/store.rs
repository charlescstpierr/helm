//! Board domain: the data model as the UI sees it, and every card operation.
//!
//! Functions here are synchronous and take a plain connection so they can be unit-tested
//! against an in-memory database; handlers run them through [`crate::db::Db::call`].

use std::collections::HashMap;
use std::fmt;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::agent::{Agent, ModelName};
use crate::db::DbError;
use crate::mentions::{self, Segment};
use crate::runs::RunStatus;

pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_DESCRIPTION_CHARS: usize = 20_000;
pub const MAX_LABELS_PER_CARD: usize = 10;
pub const MAX_LABEL_CHARS: usize = 32;
pub const MAX_COMMENT_CHARS: usize = 20_000;
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
    /// A run was asked to change state in a way the state machine forbids.
    IllegalTransition {
        from: RunStatus,
        to: RunStatus,
    },
    Db(DbError),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("not found"),
            Self::Invalid(message) => f.write_str(message),
            Self::IllegalTransition { from, to } => {
                write!(f, "a {} run cannot become {}", from.slug(), to.slug())
            }
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

pub type Result<T> = std::result::Result<T, StoreError>;

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
    pub comment_count: i64,
    /// Who works on the card when it enters a `todo` column; `None` leaves it to humans.
    pub agent: Option<Agent>,
    /// The `--model` for that agent; `None` means the project default.
    pub model: Option<ModelName>,
    /// Where the card's latest run stands, for the board tile.
    pub run_status: Option<RunStatus>,
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

    /// The assigned agent's slug, empty when none: what the form's `<select>` compares with.
    pub fn agent_slug(&self) -> &'static str {
        self.agent.map_or("", Agent::slug)
    }

    pub fn agent_name(&self) -> &'static str {
        self.agent.map_or("", Agent::name)
    }

    pub fn model_text(&self) -> &str {
        self.model.as_ref().map_or("", ModelName::as_str)
    }

    /// Labels as the comma-separated text the edit form round-trips.
    pub fn labels_text(&self) -> String {
        let names: Vec<&str> = self.labels.iter().map(|l| l.name.as_str()).collect();
        names.join(", ")
    }
}

/// The stable status of a column, the one thing the orchestrator reasons about. A column's
/// name is only a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Backlog,
    Todo,
    InProgress,
    InReview,
    Done,
}

impl Category {
    pub const ALL: [Self; 5] = [
        Self::Backlog,
        Self::Todo,
        Self::InProgress,
        Self::InReview,
        Self::Done,
    ];

    pub const fn slug(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::InReview => "in_review",
            Self::Done => "done",
        }
    }
}

impl ToSql for Category {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.slug().to_sql()
    }
}

impl FromSql for Category {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Self::ALL
            .into_iter()
            .find(|category| category.slug() == text)
            .ok_or_else(|| FromSqlError::Other(format!("unknown category {text:?}").into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub id: i64,
    pub name: String,
    pub category: Category,
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
    /// An agent slug, or blank for none.
    pub agent: String,
    /// A model name, or blank for the project default. Ignored without an agent.
    pub model: String,
}

struct ValidCard {
    title: String,
    description: String,
    priority: i64,
    labels: Vec<String>,
    agent: Option<Agent>,
    model: Option<ModelName>,
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
        let agent = match self.agent.trim() {
            "" => None,
            slug => Some(
                Agent::parse(slug)
                    .ok_or_else(|| StoreError::Invalid("Agent inconnu.".to_owned()))?,
            ),
        };
        let model = ModelName::parse_optional(&self.model)
            .map_err(|e| StoreError::Invalid(e.to_string()))?
            .filter(|_| agent.is_some());
        Ok(ValidCard {
            title: title.to_owned(),
            description: description.to_owned(),
            priority: self.priority,
            labels: parse_labels(&self.labels)?,
            agent,
            model,
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
            "SELECT id, column_id, number, title, description, priority,
                    (SELECT COUNT(*) FROM comments WHERE card_id = cards.id), agent, model,
                    (SELECT status FROM agent_runs WHERE card_id = cards.id ORDER BY id DESC LIMIT 1)
             FROM cards
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
            "SELECT id, column_id, number, title, description, priority,
                    (SELECT COUNT(*) FROM comments WHERE card_id = cards.id), agent, model,
                    (SELECT status FROM agent_runs WHERE card_id = cards.id ORDER BY id DESC LIMIT 1)
             FROM cards
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
        comment_count: row.get(6)?,
        agent: row.get(7)?,
        model: row.get(8)?,
        run_status: row.get(9)?,
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
        "INSERT INTO cards (project_id, column_id, number, title, description, priority, position, agent, model)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        (
            project_id,
            column_id,
            number,
            &card.title,
            &card.description,
            card.priority,
            position,
            card.agent,
            &card.model,
        ),
    )?;
    let id = tx.last_insert_rowid();
    set_labels(&tx, project_id, id, &card.labels)?;
    tx.commit()?;
    Ok(id)
}

/// Updates a card's fields; changing `column_id` sends it to the bottom of the new column.
/// Returns whether the card entered a different column.
pub fn update_card(
    conn: &mut Connection,
    id: i64,
    column_id: i64,
    input: &CardInput,
) -> Result<bool> {
    let card = input.validate()?;
    let tx = conn.transaction()?;
    let (project_id, current_column) = card_location(&tx, id)?;
    tx.execute(
        "UPDATE cards SET title = ?1, description = ?2, priority = ?3, agent = ?4, model = ?5,
                updated_at = unixepoch()
         WHERE id = ?6",
        (
            &card.title,
            &card.description,
            card.priority,
            card.agent,
            &card.model,
            id,
        ),
    )?;
    set_labels(&tx, project_id, id, &card.labels)?;
    let entered = column_id != current_column;
    if entered {
        place_card(&tx, id, project_id, current_column, column_id, usize::MAX)?;
    }
    tx.commit()?;
    Ok(entered)
}

/// Moves a card to `index` (0-based, clamped) in `column_id`, keeping positions dense.
/// Returns whether the card entered a different column.
pub fn move_card(conn: &mut Connection, id: i64, column_id: i64, index: usize) -> Result<bool> {
    let tx = conn.transaction()?;
    let (project_id, current_column) = card_location(&tx, id)?;
    place_card(&tx, id, project_id, current_column, column_id, index)?;
    tx.commit()?;
    Ok(column_id != current_column)
}

/// The category of the column a card is in.
pub fn card_category(conn: &Connection, id: i64) -> Result<Category> {
    conn.query_row(
        "SELECT bc.category FROM cards c JOIN board_columns bc ON bc.id = c.column_id
         WHERE c.id = ?1",
        [id],
        |row| row.get(0),
    )
    .optional()?
    .ok_or(StoreError::NotFound)
}

/// Moves a card to the bottom of its project's first column of `category`. Returns `false`
/// when the project has no such column (columns are not customisable yet, but may be).
pub fn move_to_category(conn: &mut Connection, id: i64, category: Category) -> Result<bool> {
    let tx = conn.transaction()?;
    let (project_id, current_column) = card_location(&tx, id)?;
    let target: Option<i64> = tx
        .query_row(
            "SELECT id FROM board_columns WHERE project_id = ?1 AND category = ?2
             ORDER BY position LIMIT 1",
            (project_id, category),
            |row| row.get(0),
        )
        .optional()?;
    let Some(column_id) = target else {
        return Ok(false);
    };
    if column_id != current_column {
        place_card(&tx, id, project_id, current_column, column_id, usize::MAX)?;
    }
    tx.commit()?;
    Ok(true)
}

/// `(project key, card number)`: the card's human identifier, `HELM-12`.
pub fn card_key(conn: &Connection, id: i64) -> Result<(String, i64)> {
    conn.query_row(
        "SELECT p.key, c.number FROM cards c JOIN projects p ON p.id = c.project_id
         WHERE c.id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()?
    .ok_or(StoreError::NotFound)
}

pub fn delete_card(conn: &mut Connection, id: i64) -> Result<()> {
    let tx = conn.transaction()?;
    let (project_id, column_id) = card_location(&tx, id)?;
    let active_run: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM agent_runs WHERE card_id = ?1 AND status IN ('queued', 'running'))",
        [id],
        |row| row.get(0),
    )?;
    if active_run {
        return invalid(
            "Une exécution d'agent est en cours : annulez-la avant de supprimer la carte.",
        );
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorKind {
    Human,
    Agent,
    System,
}

impl AuthorKind {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::System => "system",
        }
    }
}

impl ToSql for AuthorKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.slug().to_sql()
    }
}

impl FromSql for AuthorKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "human" => Ok(Self::Human),
            "agent" => Ok(Self::Agent),
            "system" => Ok(Self::System),
            other => Err(FromSqlError::Other(
                format!("unknown author kind {other:?}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    pub kind: AuthorKind,
    pub name: String,
}

impl Author {
    pub fn helm() -> Self {
        Self {
            kind: AuthorKind::System,
            name: "helm".to_owned(),
        }
    }

    pub fn moi() -> Self {
        Self {
            kind: AuthorKind::Human,
            name: "moi".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: i64,
    pub author: Author,
    pub body: String,
    pub created_at: i64,
}

/// `2026-10-07 14:03 UTC`: the no-script rendering of a timestamp.
pub fn utc_display(timestamp: i64) -> String {
    let iso = utc_iso(timestamp);
    format!("{} {} UTC", &iso[..10], &iso[11..16])
}

/// `14:03:07 UTC`: for log lines, where the date is the card's.
pub fn utc_time_display(timestamp: i64) -> String {
    format!("{} UTC", &utc_iso(timestamp)[11..19])
}

impl Comment {
    pub fn segments(&self) -> Vec<Segment<'_>> {
        mentions::segments(&self.body)
    }

    pub fn created_iso(&self) -> String {
        utc_iso(self.created_at)
    }

    pub fn created_display(&self) -> String {
        utc_display(self.created_at)
    }
}

pub fn utc_iso(timestamp: i64) -> String {
    let (year, month, day) = civil_from_days(timestamp.div_euclid(86_400));
    let seconds = timestamp.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3_600,
        seconds % 3_600 / 60,
        seconds % 60
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

pub fn list_comments(conn: &Connection, card_id: i64) -> Result<Vec<Comment>> {
    Ok(conn
        .prepare(
            "SELECT id, author_kind, author, body, created_at FROM comments
             WHERE card_id = ?1 ORDER BY id",
        )?
        .query_map([card_id], |row| {
            Ok(Comment {
                id: row.get(0)?,
                author: Author {
                    kind: row.get(1)?,
                    name: row.get(2)?,
                },
                body: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Stores a comment. Only human and agent comments record `@mentions`: a system comment
/// quotes text Helm does not control (an error message, a branch name) and must never queue
/// work for an agent.
pub fn add_comment(
    conn: &mut Connection,
    card_id: i64,
    author: &Author,
    body: &str,
) -> Result<i64> {
    let body = body.replace("\r\n", "\n");
    let body = body.trim_start_matches('\n').trim_end();
    if body.trim().is_empty() {
        return invalid("Le commentaire est vide.");
    }
    if body.chars().count() > MAX_COMMENT_CHARS {
        return invalid(format!(
            "Le commentaire dépasse {MAX_COMMENT_CHARS} caractères."
        ));
    }
    let tx = conn.transaction()?;
    card_location(&tx, card_id)?;
    tx.execute(
        "INSERT INTO comments (card_id, author_kind, author, body) VALUES (?1, ?2, ?3, ?4)",
        (card_id, author.kind, &author.name, body),
    )?;
    let comment_id = tx.last_insert_rowid();
    let targets = match author.kind {
        AuthorKind::System => Vec::new(),
        AuthorKind::Human | AuthorKind::Agent => mentions::mentioned_targets(body),
    };
    for target in targets {
        tx.execute(
            "INSERT INTO mentions (comment_id, target) VALUES (?1, ?2)",
            (comment_id, target.slug()),
        )?;
    }
    tx.commit()?;
    Ok(comment_id)
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

    fn assigned(agent: &str, model: &str) -> CardInput {
        CardInput {
            title: "Task".to_owned(),
            agent: agent.to_owned(),
            model: model.to_owned(),
            ..CardInput::default()
        }
    }

    #[test]
    fn an_agent_and_model_assignment_round_trips_and_can_be_changed() {
        let mut conn = test_conn();
        let id = create_card(&mut conn, 1, &assigned("claude", " haiku ")).unwrap();
        let card = get_card(&conn, id).unwrap();
        assert_eq!(card.agent, Some(Agent::Claude));
        assert_eq!(card.model_text(), "haiku");
        assert_eq!(layout_agents(&conn), [Some(Agent::Claude)]);

        update_card(&mut conn, id, 1, &assigned("claude", "")).unwrap();
        let card = get_card(&conn, id).unwrap();
        assert_eq!((card.agent, card.model), (Some(Agent::Claude), None));

        update_card(&mut conn, id, 1, &assigned("", "opus")).unwrap();
        let card = get_card(&conn, id).unwrap();
        assert_eq!(
            (card.agent, card.model),
            (None, None),
            "a model without an agent is dropped"
        );
    }

    fn layout_agents(conn: &Connection) -> Vec<Option<Agent>> {
        load_board(conn)
            .unwrap()
            .columns
            .into_iter()
            .flat_map(|column| column.cards)
            .map(|card| card.agent)
            .collect()
    }

    #[test]
    fn unknown_agents_and_unsafe_model_names_are_rejected_without_writing() {
        let mut conn = test_conn();
        for (agent, model) in [
            ("codex", ""),
            ("claude", "--dangerously-skip-permissions"),
            ("claude", "a b"),
        ] {
            assert!(matches!(
                create_card(&mut conn, 1, &assigned(agent, model)),
                Err(StoreError::Invalid(_))
            ));
        }
        assert!(layout_agents(&conn).is_empty());
    }

    #[test]
    fn categories_move_cards_to_the_matching_column_and_report_a_missing_one() {
        let mut conn = test_conn();
        let id = create_card(&mut conn, 1, &input("A")).unwrap();
        assert_eq!(card_category(&conn, id).unwrap(), Category::Backlog);

        assert!(move_to_category(&mut conn, id, Category::InReview).unwrap());
        assert_eq!(card_category(&conn, id).unwrap(), Category::InReview);
        assert_eq!(layout(&conn)[3], ["A"]);
        assert_dense_positions(&conn);

        conn.execute("DELETE FROM board_columns WHERE category = 'done'", [])
            .unwrap();
        assert!(!move_to_category(&mut conn, id, Category::Done).unwrap());
        assert_eq!(card_category(&conn, id).unwrap(), Category::InReview);
        assert!(matches!(
            card_category(&conn, 99),
            Err(StoreError::NotFound)
        ));
        assert_eq!(card_key(&conn, id).unwrap(), ("HELM".to_owned(), 1));
    }

    #[test]
    fn update_and_move_report_whether_the_card_changed_column() {
        let mut conn = test_conn();
        let id = create_card(&mut conn, 1, &input("A")).unwrap();
        assert!(!move_card(&mut conn, id, 1, 0).unwrap());
        assert!(move_card(&mut conn, id, 2, 0).unwrap());
        assert!(!update_card(&mut conn, id, 2, &input("B")).unwrap());
        assert!(update_card(&mut conn, id, 3, &input("B")).unwrap());
    }

    #[test]
    fn system_comments_never_record_mentions_but_human_ones_do() {
        let mut conn = test_conn();
        let id = create_card(&mut conn, 1, &input("A")).unwrap();
        add_comment(
            &mut conn,
            id,
            &Author::helm(),
            "git said: ask @claude and @codex",
        )
        .unwrap();
        let mentions: i64 = conn
            .query_row("SELECT COUNT(*) FROM mentions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mentions, 0);
        add_comment(&mut conn, id, &Author::moi(), "@claude please").unwrap();
        let mentions: i64 = conn
            .query_row("SELECT COUNT(*) FROM mentions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mentions, 1);
        assert_eq!(list_comments(&conn, id).unwrap()[0].author, Author::helm());
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
            ..CardInput::default()
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

    fn card_in_first_column(conn: &mut Connection) -> i64 {
        let column = column_ids(conn)[0];
        create_card(conn, column, &input("Card")).unwrap()
    }

    fn mention_rows(conn: &Connection) -> Vec<(i64, String, Option<i64>)> {
        conn.prepare("SELECT comment_id, target, handled_at FROM mentions ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn comment_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM comments", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_comment_records_one_unhandled_mention_per_known_target() {
        let mut conn = test_conn();
        let card = card_in_first_column(&mut conn);
        let id = add_comment(
            &mut conn,
            card,
            &Author::moi(),
            "@codex take this, @Codex again, cc @claude. Mail a@moi.fr about @param",
        )
        .unwrap();

        assert_eq!(
            mention_rows(&conn),
            [
                (id, "codex".to_owned(), None),
                (id, "claude".to_owned(), None)
            ]
        );
    }

    #[test]
    fn a_comment_without_a_known_mention_records_none() {
        let mut conn = test_conn();
        let card = card_in_first_column(&mut conn);
        add_comment(&mut conn, card, &Author::moi(), "mail me at x@codex.com").unwrap();
        assert_eq!(mention_rows(&conn), []);
    }

    #[test]
    fn a_failed_mention_insert_leaves_no_comment_behind() {
        let mut conn = test_conn();
        let card = card_in_first_column(&mut conn);
        conn.execute_batch(
            "CREATE TRIGGER refuse_mentions BEFORE INSERT ON mentions
             BEGIN SELECT RAISE(ABORT, 'no mentions today'); END;",
        )
        .unwrap();

        assert!(add_comment(&mut conn, card, &Author::moi(), "@codex hi").is_err());

        assert_eq!(comment_rows(&conn), 0);
        assert!(list_comments(&conn, card).unwrap().is_empty());
    }

    #[test]
    fn threads_are_chronological_and_per_card() {
        let mut conn = test_conn();
        let first = card_in_first_column(&mut conn);
        let second = card_in_first_column(&mut conn);
        for body in ["one", "two", "three"] {
            add_comment(&mut conn, first, &Author::moi(), body).unwrap();
        }
        add_comment(&mut conn, second, &Author::moi(), "elsewhere").unwrap();

        let thread = list_comments(&conn, first).unwrap();
        let bodies: Vec<&str> = thread.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, ["one", "two", "three"]);
        assert!(thread.iter().all(|c| c.author == Author::moi()));
        assert_eq!(list_comments(&conn, second).unwrap().len(), 1);
    }

    #[test]
    fn author_kind_and_name_round_trip() {
        let mut conn = test_conn();
        let card = card_in_first_column(&mut conn);
        let codex = Author {
            kind: AuthorKind::Agent,
            name: "codex".to_owned(),
        };
        let helm = Author {
            kind: AuthorKind::System,
            name: "helm".to_owned(),
        };
        add_comment(&mut conn, card, &codex, "done").unwrap();
        add_comment(&mut conn, card, &helm, "run finished").unwrap();

        let authors: Vec<Author> = list_comments(&conn, card)
            .unwrap()
            .into_iter()
            .map(|c| c.author)
            .collect();
        assert_eq!(authors, [codex, helm]);
    }

    #[test]
    fn comment_bodies_are_validated_and_normalised() {
        let mut conn = test_conn();
        let card = card_in_first_column(&mut conn);
        for empty in ["", "   ", "\n\r\n \t"] {
            assert!(
                matches!(
                    add_comment(&mut conn, card, &Author::moi(), empty),
                    Err(StoreError::Invalid(_))
                ),
                "{empty:?}"
            );
        }
        let too_long = "é".repeat(MAX_COMMENT_CHARS + 1);
        assert!(matches!(
            add_comment(&mut conn, card, &Author::moi(), &too_long),
            Err(StoreError::Invalid(_))
        ));
        let at_limit = "é".repeat(MAX_COMMENT_CHARS);
        assert!(add_comment(&mut conn, card, &Author::moi(), &at_limit).is_ok());
        assert_eq!(comment_rows(&conn), 1);

        add_comment(
            &mut conn,
            card,
            &Author::moi(),
            "\r\n\r\nline 1\r\n  line 2  \r\n\r\n",
        )
        .unwrap();
        let last = list_comments(&conn, card).unwrap().pop().unwrap();
        assert_eq!(last.body, "line 1\n  line 2");
    }

    #[test]
    fn commenting_on_a_missing_card_stores_nothing() {
        let mut conn = test_conn();
        assert!(matches!(
            add_comment(&mut conn, 42, &Author::moi(), "@codex hi"),
            Err(StoreError::NotFound)
        ));
        assert_eq!(comment_rows(&conn), 0);
        assert_eq!(mention_rows(&conn), []);
    }

    #[test]
    fn the_board_counts_comments_per_card() {
        let mut conn = test_conn();
        let busy = card_in_first_column(&mut conn);
        let quiet = card_in_first_column(&mut conn);
        add_comment(&mut conn, busy, &Author::moi(), "a").unwrap();
        add_comment(&mut conn, busy, &Author::moi(), "b").unwrap();

        assert_eq!(get_card(&conn, busy).unwrap().comment_count, 2);
        assert_eq!(get_card(&conn, quiet).unwrap().comment_count, 0);
        let board = load_board(&conn).unwrap();
        let counts: Vec<i64> = board.columns[0]
            .cards
            .iter()
            .map(|c| c.comment_count)
            .collect();
        assert_eq!(counts, [2, 0]);
    }

    #[test]
    fn deleting_a_card_deletes_its_thread_and_mentions() {
        let mut conn = test_conn();
        let doomed = card_in_first_column(&mut conn);
        let kept = card_in_first_column(&mut conn);
        add_comment(&mut conn, doomed, &Author::moi(), "@codex bye").unwrap();
        add_comment(&mut conn, kept, &Author::moi(), "@claude stay").unwrap();

        delete_card(&mut conn, doomed).unwrap();

        assert!(list_comments(&conn, doomed).unwrap().is_empty());
        assert_eq!(list_comments(&conn, kept).unwrap().len(), 1);
        let remaining: Vec<String> = mention_rows(&conn).into_iter().map(|r| r.1).collect();
        assert_eq!(remaining, ["claude"]);
    }

    #[test]
    fn timestamps_are_formatted_as_utc() {
        for (timestamp, expected) in [
            (0, "1970-01-01T00:00:00Z"),
            (86_399, "1970-01-01T23:59:59Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_709_164_799, "2024-02-28T23:59:59Z"),
            (1_791_383_525, "2026-10-07T14:32:05Z"),
            (4_102_444_800, "2100-01-01T00:00:00Z"),
            (-1, "1969-12-31T23:59:59Z"),
        ] {
            assert_eq!(utc_iso(timestamp), expected);
        }
        let comment = Comment {
            id: 1,
            author: Author::moi(),
            body: String::new(),
            created_at: 1_791_383_525,
        };
        assert_eq!(comment.created_display(), "2026-10-07 14:32 UTC");
    }
}
