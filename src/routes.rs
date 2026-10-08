//! HTTP surface: server-rendered pages, form endpoints, the SSE stream and request guards.

use std::convert::Infallible;
use std::time::Duration;

use askama::Template;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use crate::agent::{Agent, ModelName};
use crate::assets;
use crate::changes::Changes;
use crate::checks::{self, Verification, VerificationStatus};
use crate::config::RunGate;
use crate::db::Db;
use crate::delivery::{self, CardPullRequest, Delivery};
use crate::runs::{self, Activity, Run, RunId, RunStatus};
use crate::store::{self, Author, Board, Card, CardInput, Comment, StoreError};
use crate::supervisor::Orchestrator;

/// Sent by `assets/app.js` on its own requests: the server then answers with a fragment or
/// an empty 204 instead of a full page or a redirect.
const FETCH_HEADER: &str = "x-helm-request";
const CONTENT_SECURITY_POLICY: &str =
    "default-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";
const SSE_KEEP_ALIVE: Duration = Duration::from_secs(25);

#[derive(Clone)]
pub struct AppState {
    db: Db,
    changes: Changes,
    orchestrator: Orchestrator,
    delivery: Option<Delivery>,
    /// When bound to loopback, only loopback `Host` names are served (DNS-rebinding guard).
    loopback_only: bool,
}

/// What the card form needs to know about the orchestrator: whether agents can be assigned
/// and run, and which model a blank field stands for.
#[derive(Clone)]
pub struct AgentsView {
    pub gate: RunGate,
    pub default_model: Option<ModelName>,
}

impl From<&Orchestrator> for AgentsView {
    fn from(orchestrator: &Orchestrator) -> Self {
        Self {
            gate: orchestrator.gate(),
            default_model: orchestrator.default_model().cloned(),
        }
    }
}

impl AgentsView {
    /// Assigning needs a repository to work in; a refusal to *run* does not prevent it.
    pub fn can_assign(&self) -> bool {
        self.gate != RunGate::NoRepo
    }

    pub fn notice(&self) -> &'static str {
        self.gate.notice().unwrap_or("")
    }

    /// Shown on the board itself, but only for the case that is a security problem rather
    /// than simply an unconfigured feature.
    pub fn banner(&self) -> &'static str {
        match self.gate {
            RunGate::NotLoopback => self.notice(),
            RunGate::Open | RunGate::NoRepo => "",
        }
    }

    pub fn model_placeholder(&self) -> String {
        match &self.default_model {
            Some(model) => format!("Défaut du projet : {model}"),
            None => "Défaut du projet".to_owned(),
        }
    }
}

impl AppState {
    pub fn new(db: Db, loopback_only: bool, orchestrator: Orchestrator) -> Self {
        Self {
            db,
            changes: Changes::new(),
            orchestrator,
            delivery: None,
            loopback_only,
        }
    }

    pub fn with_delivery(mut self, delivery: Delivery) -> Self {
        self.orchestrator.set_delivery_locks(delivery.locks());
        self.delivery = Some(delivery);
        self
    }

    fn github_enabled(&self) -> bool {
        self.delivery.as_ref().is_some_and(Delivery::enabled)
    }

    fn github_delivery(&self) -> AppResult<&Delivery> {
        self.delivery
            .as_ref()
            .filter(|delivery| delivery.enabled())
            .ok_or_else(|| AppError::Conflict("GitHub désactivé pour ce projet.".to_owned()))
    }

    /// The notifier the background supervisor shares with the handlers.
    pub fn changes(&self) -> Changes {
        self.changes.clone()
    }

    fn agents(&self) -> AgentsView {
        AgentsView::from(&self.orchestrator)
    }

    fn board_changed(&self) {
        self.changes.publish();
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(board_page))
        .route("/board", get(board_fragment))
        .route("/cards", post(create_card))
        .route("/cards/{id}", post(update_card))
        .route("/cards/{id}/edit", get(edit_card))
        .route("/cards/{id}/move", post(move_card))
        .route(
            "/cards/{id}/comments",
            get(comment_thread).post(add_comment),
        )
        .route("/cards/{id}/activity", get(card_activity))
        .route(
            "/cards/{id}/pull-request/publish",
            post(publish_pull_request),
        )
        .route(
            "/cards/{id}/pull-request/refresh",
            post(refresh_pull_request),
        )
        .route("/cards/{id}/pull-request/merge", post(merge_pull_request))
        .route(
            "/cards/{id}/delete",
            get(confirm_delete_card).post(delete_card),
        )
        .route("/runs/{id}/cancel", post(cancel_run))
        .route("/events", get(events))
        .route("/assets/{*path}", get(assets::serve))
        .route("/healthz", get(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Errors

pub enum AppError {
    NotFound,
    Invalid(String),
    /// The request is fine but the target is in a state that forbids it.
    Conflict(String),
    Internal(String),
}

impl From<StoreError> for AppError {
    fn from(e: StoreError) -> Self {
        match &e {
            StoreError::NotFound => Self::NotFound,
            StoreError::Invalid(message) => Self::Invalid(message.clone()),
            StoreError::IllegalTransition { .. } => Self::Conflict(e.to_string()),
            StoreError::Db(e) => Self::Internal(e.to_string()),
        }
    }
}

impl From<askama::Error> for AppError {
    fn from(e: askama::Error) -> Self {
        Self::Internal(format!("template: {e}"))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "Introuvable.").into_response(),
            Self::Invalid(message) => (StatusCode::UNPROCESSABLE_ENTITY, message).into_response(),
            Self::Conflict(message) => (StatusCode::CONFLICT, message).into_response(),
            Self::Internal(detail) => {
                eprintln!("helm: internal error: {detail}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Erreur interne.").into_response()
            }
        }
    }
}

type AppResult<T> = Result<T, AppError>;

// ---------------------------------------------------------------------------
// Guards

/// Rejects cross-site writes and foreign `Host` names, and sets security headers.
///
/// Helm has no login: anything that can reach the port can edit the board, so a web page
/// open in the same browser must not be able to drive it.
async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host = header_str(headers, header::HOST);

    if state.loopback_only && !host.is_some_and(is_loopback_host) {
        return (StatusCode::FORBIDDEN, "Hôte refusé.").into_response();
    }
    let is_write = !matches!(*request.method(), Method::GET | Method::HEAD);
    if is_write && !same_origin(headers, host) {
        return (StatusCode::FORBIDDEN, "Origine refusée.").into_response();
    }

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Not `no-referrer`: under it browsers send `Origin: null` on plain form posts, which
    // `same_origin` rejects.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    response
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn is_loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        // Bracketed IPv6 literal, optionally followed by `:port`.
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// A write is same-origin when the browser says so, or when no browser is involved.
fn same_origin(headers: &HeaderMap, host: Option<&str>) -> bool {
    if header_str(headers, header::HeaderName::from_static("sec-fetch-site"))
        .is_some_and(|site| site != "same-origin" && site != "none")
    {
        return false;
    }
    match header_str(headers, header::ORIGIN) {
        // Non-browser clients (curl, future agent tooling) send no Origin.
        None => true,
        Some(origin) => origin
            .split_once("://")
            .zip(host)
            .is_some_and(|((_, authority), host)| authority.eq_ignore_ascii_case(host)),
    }
}

fn is_fetch(headers: &HeaderMap) -> bool {
    headers.contains_key(FETCH_HEADER)
}

/// Answer to a successful write: nothing for the script, a redirect for a plain form post.
fn written(headers: &HeaderMap) -> Response {
    if is_fetch(headers) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        Redirect::to("/").into_response()
    }
}

// ---------------------------------------------------------------------------
// Pages

#[derive(Template)]
#[template(path = "board.html")]
struct BoardPage {
    board: Board,
    agents: AgentsView,
}

#[derive(Template)]
#[template(path = "_board.html")]
struct BoardFragment {
    board: Board,
}

#[derive(Template)]
#[template(path = "card_delete.html")]
struct CardDeletePage {
    board: Board,
    card: Card,
}

#[derive(Template)]
#[template(path = "card_edit.html")]
struct CardEditPage {
    board: Board,
    card: Card,
    comments: Vec<Comment>,
    activity: Activity,
    verification: Option<Verification>,
    delivery: Option<CardPullRequest>,
    github_enabled: bool,
    agents: AgentsView,
}

#[derive(Template)]
#[template(path = "_card_panel.html")]
struct CardPanelFragment {
    board: Board,
    card: Card,
    comments: Vec<Comment>,
    activity: Activity,
    verification: Option<Verification>,
    delivery: Option<CardPullRequest>,
    github_enabled: bool,
    agents: AgentsView,
}

#[derive(Template)]
#[template(path = "_activity.html")]
struct ActivityFragment {
    activity: Activity,
    verification: Option<Verification>,
    delivery: Option<CardPullRequest>,
    github_enabled: bool,
}

fn can_publish(
    run: &Run,
    verification: &Option<Verification>,
    delivery: &Option<CardPullRequest>,
) -> bool {
    run.status == RunStatus::Succeeded
        && run.pushed_at.is_some()
        && run.branch.is_some()
        && verification.as_ref().is_some_and(|verification| {
            matches!(
                verification.status,
                VerificationStatus::Passed | VerificationStatus::Skipped
            )
        })
        && delivery.as_ref().is_none_or(|delivery| {
            delivery.run_id != run.id
                || (delivery.error.is_some()
                    && delivery.pr.state != crate::github::PullRequestState::Merged)
        })
}

fn can_merge(run: &Run, verification: &Option<Verification>, delivery: &CardPullRequest) -> bool {
    run.status == RunStatus::Succeeded
        && run.pushed_at.is_some()
        && delivery.run_id == run.id
        && run.branch.as_deref() == Some(delivery.pr.head_branch.as_str())
        && delivery.expected_base == delivery.pr.base_branch
        && delivery.error.is_none()
        && delivery.pr.merge_blocker().is_none()
        && verification.as_ref().is_some_and(|verification| {
            verification.commit_sha == delivery.pr.head_sha
                && matches!(
                    verification.status,
                    VerificationStatus::Passed | VerificationStatus::Skipped
                )
        })
}

#[derive(Template)]
#[template(path = "_thread.html")]
struct ThreadFragment {
    comments: Vec<Comment>,
}

async fn board_page(State(state): State<AppState>) -> AppResult<Html<String>> {
    let board = state.db.call(|conn| store::load_board(conn)).await?;
    let agents = state.agents();
    Ok(Html(BoardPage { board, agents }.render()?))
}

async fn board_fragment(State(state): State<AppState>) -> AppResult<Html<String>> {
    let board = state.db.call(|conn| store::load_board(conn)).await?;
    Ok(Html(BoardFragment { board }.render()?))
}

/// Confirmation step for deleting without JavaScript (the script asks in a dialog instead).
async fn confirm_delete_card(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let (board, card) = state
        .db
        .call(move |conn| {
            Ok::<_, StoreError>((store::load_board(conn)?, store::get_card(conn, id)?))
        })
        .await?;
    Ok(Html(CardDeletePage { board, card }.render()?))
}

async fn edit_card(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let (board, card, comments, activity, verification, delivery) = state
        .db
        .call(move |conn| {
            let activity = runs::activity(conn, id)?;
            let verification = activity
                .run
                .as_ref()
                .map(|run| checks::get(conn, run.id))
                .transpose()?
                .flatten();
            Ok::<_, StoreError>((
                store::load_board(conn)?,
                store::get_card(conn, id)?,
                store::list_comments(conn, id)?,
                activity,
                verification,
                delivery::get(conn, id)?,
            ))
        })
        .await?;
    let agents = state.agents();
    let github_enabled = state.github_enabled();
    let html = if is_fetch(&headers) {
        CardPanelFragment {
            board,
            card,
            comments,
            activity,
            verification,
            delivery,
            github_enabled,
            agents,
        }
        .render()?
    } else {
        CardEditPage {
            board,
            card,
            comments,
            activity,
            verification,
            delivery,
            github_enabled,
            agents,
        }
        .render()?
    };
    Ok(Html(html))
}

async fn card_activity(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let (activity, verification, delivery) = state
        .db
        .call(move |conn| {
            store::get_card(conn, id)?;
            let activity = runs::activity(conn, id)?;
            let verification = activity
                .run
                .as_ref()
                .map(|run| checks::get(conn, run.id))
                .transpose()?
                .flatten();
            Ok::<_, StoreError>((activity, verification, delivery::get(conn, id)?))
        })
        .await?;
    Ok(Html(
        ActivityFragment {
            activity,
            verification,
            delivery,
            github_enabled: state.github_enabled(),
        }
        .render()?,
    ))
}

async fn comment_thread(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let comments = state
        .db
        .call(move |conn| {
            store::get_card(conn, id)?;
            store::list_comments(conn, id)
        })
        .await?;
    Ok(Html(ThreadFragment { comments }.render()?))
}

// ---------------------------------------------------------------------------
// Card writes

#[derive(Deserialize)]
struct CardForm {
    column_id: i64,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    priority: i64,
    #[serde(default)]
    labels: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    model: String,
}

impl CardForm {
    fn into_parts(self) -> (i64, CardInput) {
        (
            self.column_id,
            CardInput {
                title: self.title,
                description: self.description,
                priority: self.priority,
                labels: self.labels,
                agent: self.agent,
                model: self.model,
            },
        )
    }
}

/// Without a repository there is nowhere for an agent to work, so a card cannot be given one.
/// Keeping an agent it already has stays allowed: editing the title must not unassign it.
fn check_assignment(
    agents: &AgentsView,
    current: Option<Agent>,
    requested: &str,
) -> Result<(), StoreError> {
    match Agent::parse(requested.trim()) {
        Some(agent) if !agents.can_assign() && current != Some(agent) => {
            Err(StoreError::Invalid(agents.notice().to_owned()))
        }
        _ => Ok(()),
    }
}

#[derive(Deserialize)]
struct MoveForm {
    column_id: i64,
    position: usize,
}

async fn create_card(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CardForm>,
) -> AppResult<Response> {
    let (column_id, input) = form.into_parts();
    let agents = state.agents();
    let orchestrator = state.orchestrator.clone();
    state
        .db
        .call(move |conn| {
            check_assignment(&agents, None, &input.agent)?;
            let id = store::create_card(conn, column_id, &input)?;
            orchestrator.after_placement(conn, id, true)
        })
        .await?;
    state.board_changed();
    Ok(written(&headers))
}

async fn update_card(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CardForm>,
) -> AppResult<Response> {
    let (column_id, input) = form.into_parts();
    let agents = state.agents();
    let orchestrator = state.orchestrator.clone();
    state
        .db
        .call(move |conn| {
            orchestrator.ensure_card_available(id)?;
            let current = store::get_card(conn, id)?.agent;
            check_assignment(&agents, current, &input.agent)?;
            let entered = store::update_card(conn, id, column_id, &input)?;
            orchestrator.after_placement(conn, id, entered)
        })
        .await?;
    state.board_changed();
    Ok(written(&headers))
}

async fn move_card(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<MoveForm>,
) -> AppResult<Response> {
    let orchestrator = state.orchestrator.clone();
    state
        .db
        .call(move |conn| {
            orchestrator.ensure_card_available(id)?;
            let entered = store::move_card(conn, id, form.column_id, form.position)?;
            orchestrator.after_placement(conn, id, entered)
        })
        .await?;
    state.board_changed();
    Ok(written(&headers))
}

async fn delete_card(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let orchestrator = state.orchestrator.clone();
    state
        .db
        .call(move |conn| {
            orchestrator.ensure_card_available(id)?;
            store::delete_card(conn, id)
        })
        .await?;
    state.board_changed();
    Ok(written(&headers))
}

async fn cancel_run(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let card_id = state.orchestrator.cancel(&state.db, RunId(id)).await?;
    state.board_changed();
    if is_fetch(&headers) {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok(Redirect::to(&format!("/cards/{card_id}/edit")).into_response())
    }
}

async fn publish_pull_request(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> AppResult<Response> {
    state.db.call(move |conn| store::get_card(conn, id)).await?;
    state
        .github_delivery()?
        .retry(id)
        .await
        .map_err(AppError::Conflict)?;
    Ok(delivery_written(&headers, id))
}

async fn refresh_pull_request(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> AppResult<Response> {
    state.db.call(move |conn| store::get_card(conn, id)).await?;
    state
        .github_delivery()?
        .refresh(id)
        .await
        .map_err(AppError::Conflict)?;
    Ok(delivery_written(&headers, id))
}

async fn merge_pull_request(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> AppResult<Response> {
    state.db.call(move |conn| store::get_card(conn, id)).await?;
    state
        .github_delivery()?
        .merge(id)
        .await
        .map_err(AppError::Conflict)?;
    Ok(delivery_written(&headers, id))
}

fn delivery_written(headers: &HeaderMap, card_id: i64) -> Response {
    if is_fetch(headers) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        Redirect::to(&format!("/cards/{card_id}/edit#run-activity")).into_response()
    }
}

#[derive(Deserialize)]
struct CommentForm {
    body: String,
}

async fn add_comment(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CommentForm>,
) -> AppResult<Response> {
    let comment_id = state
        .db
        .call(move |conn| store::add_comment(conn, id, &Author::moi(), &form.body))
        .await?;
    state.board_changed();
    if is_fetch(&headers) {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok(Redirect::to(&format!("/cards/{id}/edit#comment-{comment_id}")).into_response())
    }
}

// ---------------------------------------------------------------------------
// Live updates

/// One `board` event per change. The payload is only a revision number: clients re-fetch
/// `/board`, so a slow tab that missed events still converges on the next one.
async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let receiver = state.changes.subscribe();
    let stream = stream::unfold(receiver, |mut receiver| async move {
        let data = match receiver.recv().await {
            Ok(revision) => revision.to_string(),
            Err(RecvError::Lagged(_)) => "resync".to_owned(),
            Err(RecvError::Closed) => return None,
        };
        Some((Ok(Event::default().event("board").data(data)), receiver))
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_KEEP_ALIVE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::PermissionMode;
    use crate::supervisor::RunDefaults;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn orchestrator(gate: RunGate) -> Orchestrator {
        Orchestrator::new(
            gate,
            RunDefaults {
                permission_mode: PermissionMode::DEFAULT,
                model: ModelName::parse_optional("sonnet").unwrap(),
            },
        )
    }

    fn app_with(gate: RunGate) -> Router {
        router(AppState::new(
            Db::open_in_memory().unwrap(),
            true,
            orchestrator(gate),
        ))
    }

    fn app() -> Router {
        app_with(RunGate::Open)
    }

    fn get(uri: &str) -> Request {
        Request::builder()
            .uri(uri)
            .header(header::HOST, "localhost:7878")
            .body(Body::empty())
            .unwrap()
    }

    fn post(uri: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::HOST, "localhost:7878")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
    }

    async fn send(app: &Router, request: Request) -> (StatusCode, HeaderMap, String) {
        let response = app.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (
            parts.status,
            parts.headers,
            String::from_utf8(bytes.to_vec()).unwrap(),
        )
    }

    async fn post_form(app: &Router, uri: &str, body: &'static str) -> StatusCode {
        let request = post(uri)
            .header(FETCH_HEADER, "fetch")
            .body(Body::from(body))
            .unwrap();
        send(app, request).await.0
    }

    #[tokio::test]
    async fn board_page_shows_default_columns_and_security_headers() {
        let app = app();
        let (status, headers, body) = send(&app, get("/")).await;
        assert_eq!(status, StatusCode::OK);
        for column in ["Backlog", "À faire", "En cours", "En revue", "Terminé"] {
            assert!(body.contains(column), "missing column {column}");
        }
        assert!(body.contains("/assets/tokens.css"));
        assert_eq!(
            headers[header::CONTENT_SECURITY_POLICY],
            CONTENT_SECURITY_POLICY
        );
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    }

    #[tokio::test]
    async fn card_lifecycle_through_the_form_endpoints() {
        let app = app();

        // Create, with markup in the title to check escaping.
        let created = post_form(
            &app,
            "/cards",
            "column_id=1&title=%3Cb%3EShip+it%3C%2Fb%3E&priority=3&labels=infra%2C+ui",
        )
        .await;
        assert_eq!(created, StatusCode::NO_CONTENT);
        let (_, _, board) = send(&app, get("/board")).await;
        assert!(board.contains("HELM-1"));
        assert!(
            board.contains("&lt;b&gt;Ship it&lt;/b&gt;") || board.contains("&#60;b&#62;Ship it")
        );
        assert!(!board.contains("<b>Ship it"));
        assert!(board.contains("data-priority=\"high\""));
        assert!(board.contains("infra") && board.contains("ui"));

        // Edit form, as a fragment for the dialog and as a full page without script.
        let mut fragment = get("/cards/1/edit");
        fragment
            .headers_mut()
            .insert(FETCH_HEADER, HeaderValue::from_static("fetch"));
        let (status, _, fragment) = send(&app, fragment).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!fragment.contains("<html"));
        assert!(fragment.contains("infra, ui"));
        let (_, _, page) = send(&app, get("/cards/1/edit")).await;
        assert!(page.contains("<html") && page.contains("action=\"/cards/1\""));

        // Update moves it to "En cours" (column 3) and renames it.
        let updated = post_form(&app, "/cards/1", "column_id=3&title=Shipped&priority=0").await;
        assert_eq!(updated, StatusCode::NO_CONTENT);
        assert_eq!(
            post_form(&app, "/cards", "column_id=3&title=Second").await,
            StatusCode::NO_CONTENT
        );

        // Move the second card above the first.
        let moved = post_form(&app, "/cards/2/move", "column_id=3&position=0").await;
        assert_eq!(moved, StatusCode::NO_CONTENT);
        let (_, _, board) = send(&app, get("/board")).await;
        assert!(board.find("Second").unwrap() < board.find("Shipped").unwrap());
        assert!(!board.contains("infra"));

        // Delete.
        assert_eq!(
            post_form(&app, "/cards/1/delete", "").await,
            StatusCode::NO_CONTENT
        );
        let (_, _, board) = send(&app, get("/board")).await;
        assert!(!board.contains("Shipped") && board.contains("Second"));
    }

    #[tokio::test]
    async fn stale_routes_never_reach_a_card_created_after_a_deletion() {
        let app = app();
        post_form(&app, "/cards", "column_id=1&title=Old").await;
        assert_eq!(
            post_form(&app, "/cards/1/delete", "").await,
            StatusCode::NO_CONTENT
        );
        post_form(&app, "/cards", "column_id=1&title=New").await;

        // A form left open on the deleted card must not edit or delete the new one.
        assert_eq!(
            post_form(&app, "/cards/1", "column_id=1&title=Hijacked").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post_form(&app, "/cards/1/delete", "").await,
            StatusCode::NOT_FOUND
        );
        let (_, _, board) = send(&app, get("/board")).await;
        assert!(board.contains("New") && !board.contains("Hijacked"));
    }

    #[tokio::test]
    async fn deleting_without_script_goes_through_a_confirmation_page() {
        let app = app();
        post_form(&app, "/cards", "column_id=1&title=Keep+me").await;

        // The edit page links to the confirmation page instead of deleting on click.
        let (_, _, edit) = send(&app, get("/cards/1/edit")).await;
        assert!(edit.contains("href=\"/cards/1/delete\""));
        let (status, _, confirm) = send(&app, get("/cards/1/delete")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(confirm.contains("Keep me") && confirm.contains("method=\"post\""));
        let (_, _, board) = send(&app, get("/board")).await;
        assert!(board.contains("Keep me"));

        assert_eq!(
            send(&app, get("/cards/42/delete")).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn plain_form_posts_redirect_to_the_board() {
        let app = app();
        let body = "column_id=1&title=No+script";
        let request = post("/cards").body(Body::from(body)).unwrap();
        let (status, headers, _) = send(&app, request).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers[header::LOCATION], "/");
    }

    #[tokio::test]
    async fn invalid_and_unknown_cards_are_reported() {
        let app = app();
        assert_eq!(
            post_form(&app, "/cards", "column_id=1&title=+++").await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            post_form(&app, "/cards", "column_id=999&title=A").await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            post_form(&app, "/cards/42/move", "column_id=1&position=0").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post_form(&app, "/cards/42/delete", "").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send(&app, get("/cards/42/edit")).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn cross_site_writes_and_foreign_hosts_are_refused() {
        let app = app();
        let body = "column_id=1&title=Evil";

        let cross_origin = post("/cards")
            .header(header::ORIGIN, "http://evil.example")
            .body(Body::from(body))
            .unwrap();
        assert_eq!(send(&app, cross_origin).await.0, StatusCode::FORBIDDEN);

        let cross_site = post("/cards")
            .header("sec-fetch-site", "cross-site")
            .body(Body::from(body))
            .unwrap();
        assert_eq!(send(&app, cross_site).await.0, StatusCode::FORBIDDEN);

        let (_, headers, _) = send(&app, get("/")).await;
        assert_eq!(headers[header::REFERRER_POLICY], "same-origin");

        let same_origin = post("/cards")
            .header(header::ORIGIN, "http://localhost:7878")
            .header("sec-fetch-site", "same-origin")
            .body(Body::from("column_id=1&title=Legit"))
            .unwrap();
        assert_eq!(send(&app, same_origin).await.0, StatusCode::SEE_OTHER);

        // DNS rebinding: the attacker's name resolves to 127.0.0.1.
        let mut rebound = get("/");
        rebound
            .headers_mut()
            .insert(header::HOST, HeaderValue::from_static("evil.example:7878"));
        assert_eq!(send(&app, rebound).await.0, StatusCode::FORBIDDEN);

        let (_, _, board) = send(&app, get("/board")).await;
        assert!(board.contains("Legit") && !board.contains("Evil"));
    }

    #[test]
    fn loopback_host_names() {
        for host in [
            "localhost",
            "LOCALHOST:7878",
            "127.0.0.1:7878",
            "[::1]:7878",
            "[::1]",
        ] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in [
            "evil.example",
            "localhost.evil.example",
            "192.168.1.10:7878",
            "",
        ] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[tokio::test]
    async fn assets_are_served_with_revalidation() {
        let app = app();
        let (status, headers, body) = send(&app, get("/assets/tokens.css")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/css")
        );
        assert!(body.contains("--color-"));

        let mut conditional = get("/assets/tokens.css");
        conditional
            .headers_mut()
            .insert(header::IF_NONE_MATCH, headers[header::ETAG].clone());
        assert_eq!(send(&app, conditional).await.0, StatusCode::NOT_MODIFIED);
        assert_eq!(
            send(&app, get("/assets/missing.css")).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn sse_stream_announces_board_changes() {
        let app = app();
        let response = app.clone().oneshot(get("/events")).await.unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        let mut body = response.into_body();

        assert_eq!(
            post_form(&app, "/cards", "column_id=1&title=Live").await,
            StatusCode::NO_CONTENT
        );
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("an SSE frame after a board change")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(text.contains("event: board"), "{text}");
        assert!(text.contains("data: 1"), "{text}");
    }

    #[tokio::test]
    async fn the_card_form_assigns_an_agent_and_model_without_script() {
        let app = app();
        post_form(&app, "/cards", "column_id=1&title=Task").await;

        let (_, _, page) = send(&app, get("/cards/1/edit")).await;
        assert!(page.contains("name=\"agent\"") && page.contains("name=\"model\""));
        assert!(page.contains("Défaut du projet : sonnet"), "{page}");
        assert!(!page.contains("class=\"notice\""));

        let request = post("/cards/1")
            .body(Body::from(
                "column_id=1&title=Task&agent=claude&model=haiku",
            ))
            .unwrap();
        assert_eq!(send(&app, request).await.0, StatusCode::SEE_OTHER);

        let (_, _, page) = send(&app, get("/cards/1/edit")).await;
        assert!(page.contains("value=\"claude\" selected"), "{page}");
        assert!(page.contains("value=\"haiku\""));
        let (_, _, board) = send(&app, get("/board")).await;
        assert!(board.contains("data-agent=\"claude\""), "{board}");

        assert_eq!(
            post_form(&app, "/cards/1", "column_id=1&title=Task&agent=codex").await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            post_form(
                &app,
                "/cards/1",
                "column_id=1&title=Task&agent=claude&model=--x"
            )
            .await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[tokio::test]
    async fn without_a_repository_agents_cannot_be_assigned_but_an_existing_one_is_kept() {
        let with_repo = app_with(RunGate::Open);
        post_form(&with_repo, "/cards", "column_id=1&title=Task&agent=claude").await;

        // Same database, repository removed from the configuration.
        let bare = app_with(RunGate::NoRepo);
        assert_eq!(
            post_form(&bare, "/cards", "column_id=1&title=New&agent=claude").await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            post_form(&bare, "/cards", "column_id=1&title=Plain").await,
            StatusCode::NO_CONTENT
        );
        let (_, _, page) = send(&bare, get("/cards/1/edit")).await;
        assert!(
            page.contains("class=\"notice\"") && page.contains("project.repo"),
            "{page}"
        );
        assert!(page.contains("id=\"card-agent\"") && page.contains("disabled"));
    }

    #[tokio::test]
    async fn a_card_keeps_its_assignment_when_edited_while_agents_are_unavailable() {
        let db = Db::open_in_memory().unwrap();
        let open = router(AppState::new(db.clone(), true, orchestrator(RunGate::Open)));
        post_form(
            &open,
            "/cards",
            "column_id=1&title=Task&agent=claude&model=haiku",
        )
        .await;

        let bare = router(AppState::new(
            db.clone(),
            true,
            orchestrator(RunGate::NoRepo),
        ));
        let (_, _, page) = send(&bare, get("/cards/1/edit")).await;
        assert!(
            page.contains("type=\"hidden\" name=\"agent\" value=\"claude\""),
            "{page}"
        );
        assert!(
            page.contains("type=\"hidden\" name=\"model\" value=\"haiku\""),
            "the disabled model must still be submitted: {page}"
        );
        // Disabled controls are not submitted, so hidden fields carry the assignment.
        assert_eq!(
            post_form(
                &bare,
                "/cards/1",
                "column_id=1&title=Renamed&agent=claude&model=haiku",
            )
            .await,
            StatusCode::NO_CONTENT
        );
        let (_, _, board) = send(&open, get("/board")).await;
        assert!(board.contains("Renamed") && board.contains("data-agent=\"claude\""));
        let card = db.call(|conn| store::get_card(conn, 1)).await.unwrap();
        assert_eq!(card.model_text(), "haiku");
    }

    fn wired_app() -> (Router, Db) {
        let db = Db::open_in_memory().unwrap();
        let app = router(AppState::new(db.clone(), true, orchestrator(RunGate::Open)));
        (app, db)
    }

    async fn run_statuses(db: &Db) -> Vec<(i64, String)> {
        db.call(|conn| {
            let rows = conn
                .prepare("SELECT card_id, status FROM agent_runs ORDER BY id")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok::<_, StoreError>(rows)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn dragging_an_assigned_card_into_todo_queues_a_run_and_dragging_it_out_withdraws_it() {
        let (app, db) = wired_app();
        post_form(
            &app,
            "/cards",
            "column_id=1&title=Task&agent=claude&model=haiku",
        )
        .await;
        assert!(run_statuses(&db).await.is_empty(), "backlog queues nothing");

        post_form(&app, "/cards/1/move", "column_id=2&position=0").await;
        assert_eq!(run_statuses(&db).await, [(1, "queued".to_owned())]);

        post_form(&app, "/cards/1/move", "column_id=1&position=0").await;
        assert_eq!(run_statuses(&db).await, [(1, "cancelled".to_owned())]);

        // The form's column list does the same without script.
        let request = post("/cards/1")
            .body(Body::from("column_id=2&title=Task&agent=claude"))
            .unwrap();
        assert_eq!(send(&app, request).await.0, StatusCode::SEE_OTHER);
        assert_eq!(run_statuses(&db).await.len(), 2);
        assert_eq!(run_statuses(&db).await[1].1, "queued");
    }

    #[tokio::test]
    async fn a_closed_gate_queues_no_run_even_for_an_assigned_card() {
        let db = Db::open_in_memory().unwrap();
        let app = router(AppState::new(
            db.clone(),
            true,
            orchestrator(RunGate::NotLoopback),
        ));
        post_form(&app, "/cards", "column_id=2&title=Task&agent=claude").await;
        assert!(run_statuses(&db).await.is_empty());
        let (_, _, page) = send(&app, get("/cards/1/edit")).await;
        assert!(
            page.contains("class=\"notice\"") && page.contains("loopback"),
            "{page}"
        );
    }

    #[tokio::test]
    async fn a_run_can_be_cancelled_from_a_form_once_and_only_while_it_is_active() {
        let (app, db) = wired_app();
        post_form(&app, "/cards", "column_id=2&title=Task&agent=claude").await;
        assert_eq!(run_statuses(&db).await, [(1, "queued".to_owned())]);

        let forged = post("/runs/1/cancel")
            .header(header::ORIGIN, "http://evil.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, forged).await.0, StatusCode::FORBIDDEN);
        assert_eq!(run_statuses(&db).await[0].1, "queued");

        let request = post("/runs/1/cancel").body(Body::empty()).unwrap();
        let (status, headers, _) = send(&app, request).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers[header::LOCATION], "/cards/1/edit");
        assert_eq!(run_statuses(&db).await[0].1, "cancelled");

        assert_eq!(
            post_form(&app, "/runs/1/cancel", "").await,
            StatusCode::CONFLICT
        );
        assert_eq!(
            post_form(&app, "/runs/99/cancel", "").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn a_card_with_an_active_run_cannot_be_deleted_from_the_board() {
        let (app, _) = wired_app();
        post_form(&app, "/cards", "column_id=2&title=Task&agent=claude").await;
        assert_eq!(
            post_form(&app, "/cards/1/delete", "").await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        post_form(&app, "/runs/1/cancel", "").await;
        assert_eq!(
            post_form(&app, "/cards/1/delete", "").await,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn github_actions_are_explicit_posts_and_refused_when_unconfigured() {
        let app = app();
        post_form(&app, "/cards", "column_id=2&title=Task&agent=claude").await;

        for action in ["publish", "refresh", "merge"] {
            let uri = format!("/cards/1/pull-request/{action}");
            assert_eq!(
                send(&app, get(&uri)).await.0,
                StatusCode::METHOD_NOT_ALLOWED
            );
            let forged = post(&uri)
                .header(header::ORIGIN, "https://evil.example")
                .body(Body::empty())
                .unwrap();
            assert_eq!(send(&app, forged).await.0, StatusCode::FORBIDDEN);
            let (status, _, body) = send(&app, post(&uri).body(Body::empty()).unwrap()).await;
            assert_eq!(status, StatusCode::CONFLICT, "{action}: {body}");
            assert!(body.contains("GitHub"), "{body}");
            let missing = format!("/cards/42/pull-request/{action}");
            assert_eq!(post_form(&app, &missing, "").await, StatusCode::NOT_FOUND);
        }

        let (_, _, panel) = send(&app, get("/cards/1/activity")).await;
        assert!(panel.contains("Non exécutées"), "{panel}");
        assert!(panel.contains("GitHub désactivé"), "{panel}");
        assert!(!panel.contains("/pull-request/"), "{panel}");
    }

    fn delivery_app(db: &Db) -> Router {
        delivery_app_with_config(
            db,
            std::path::PathBuf::from("/unused-rendering-repository"),
            std::path::PathBuf::from("/github-must-not-run-on-get"),
        )
    }

    fn delivery_app_with_config(
        db: &Db,
        repo: std::path::PathBuf,
        command: std::path::PathBuf,
    ) -> Router {
        let state = AppState::new(db.clone(), true, orchestrator(RunGate::Open));
        let delivery = Delivery::new(
            db.clone(),
            state.changes(),
            RunGate::Open,
            Some(repo),
            Some(crate::config::GithubConfig {
                repository: "owner/repo".to_owned(),
                command,
            }),
        );
        router(state.with_delivery(delivery))
    }

    async fn store_verified_pull_request(db: &Db) -> RunId {
        db.call(|conn| {
            let run = runs::claim_next_queued(conn)?.unwrap();
            runs::record_workspace(conn, run.id, "/unused-rendering-worktree", "helm/HELM-1")?;
            let sha = "1234567890123456789012345678901234567890";
            checks::start(conn, run.id, sha, &["echo '<command>'".to_owned()])?;
            checks::start_check(conn, run.id, 0)?;
            checks::finish_check(
                conn,
                run.id,
                0,
                &crate::process::CommandOutput {
                    stdout: "<script>bad()</script>".to_owned(),
                    stderr: "<error>details</error>".to_owned(),
                    exit_code: Some(0),
                    timed_out: false,
                },
            )?;
            checks::finish(conn, run.id, VerificationStatus::Passed)?;
            runs::finish(conn, run.id, &runs::Outcome::Succeeded)?;
            delivery::record(
                conn,
                run.card_id,
                run.id,
                "owner/repo",
                &crate::github::PullRequest {
                    number: 17,
                    url: "https://github.com/owner/repo/pull/17".to_owned(),
                    state: crate::github::PullRequestState::Open,
                    draft: false,
                    head_sha: sha.to_owned(),
                    head_branch: "helm/HELM-1".to_owned(),
                    base_branch: "main".to_owned(),
                    checks: crate::github::CiStatus::Passed,
                    mergeable: "MERGEABLE".to_owned(),
                    merge_state_status: "CLEAN".to_owned(),
                    review_decision: "APPROVED".to_owned(),
                },
            )?;
            Ok::<_, StoreError>(run.id)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn stored_checks_and_pull_requests_render_on_every_card_view_without_remote_calls() {
        let db = Db::open_in_memory().unwrap();
        let app = delivery_app(&db);
        post_form(&app, "/cards", "column_id=2&title=Task&agent=claude").await;
        store_verified_pull_request(&db).await;

        let before = db
            .call(|conn| delivery::get(conn, 1))
            .await
            .unwrap()
            .unwrap();
        let mut dialog = get("/cards/1/edit");
        dialog
            .headers_mut()
            .insert(FETCH_HEADER, HeaderValue::from_static("fetch"));
        for request in [get("/cards/1/activity"), get("/cards/1/edit"), dialog] {
            let (status, _, body) = send(&app, request).await;
            assert_eq!(status, StatusCode::OK);
            for expected in [
                "Vérifications locales",
                "Réussies",
                "1234567890123456789012345678901234567890",
                "Code de retour : 0",
                "https://github.com/owner/repo/pull/17",
                "Dernière actualisation",
                "action=\"/cards/1/pull-request/refresh\"",
                "action=\"/cards/1/pull-request/merge\"",
                "data-run-form",
            ] {
                assert!(body.contains(expected), "missing {expected}: {body}");
            }
            assert!(!body.contains("<script>bad()"), "{body}");
            assert!(!body.contains("<command>"), "{body}");
            assert!(!body.contains("<error>details"), "{body}");
            assert!(body.contains("bad()") && body.contains("details"));
            assert!(!body.contains("/pull-request/publish"));
        }
        let after = db
            .call(|conn| delivery::get(conn, 1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.refreshed_at, after.refreshed_at);
        assert_eq!(before.pr, after.pr);

        let disabled = router(AppState::new(db.clone(), true, orchestrator(RunGate::Open)));
        let (_, _, body) = send(&disabled, get("/cards/1/activity")).await;
        assert!(body.contains("owner/repo #17"));
        assert!(!body.contains("action=\"/cards/1/pull-request/"));

        db.call(|conn| {
            conn.execute("DELETE FROM card_pull_requests WHERE card_id = 1", [])?;
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
        let (_, _, body) = send(&app, get("/cards/1/activity")).await;
        assert!(body.contains("Aucune PR enregistrée"));
        assert!(body.contains("/pull-request/publish"));
        assert!(!body.contains("/pull-request/merge"));
    }

    #[tokio::test]
    async fn merge_controls_follow_saved_checks_errors_and_latest_run() {
        let db = Db::open_in_memory().unwrap();
        let app = delivery_app(&db);
        post_form(&app, "/cards", "column_id=2&title=Task&agent=claude").await;
        let run_id = store_verified_pull_request(&db).await;

        for checks_status in [
            crate::github::CiStatus::Pending,
            crate::github::CiStatus::Failed,
            crate::github::CiStatus::Unknown,
        ] {
            db.call(move |conn| {
                let mut delivery = delivery::get(conn, 1)?.unwrap();
                delivery.pr.checks = checks_status;
                delivery::record(conn, 1, run_id, &delivery.repository, &delivery.pr)
            })
            .await
            .unwrap();
            let (_, _, body) = send(&app, get("/cards/1/activity")).await;
            assert!(!body.contains("/pull-request/merge"), "{body}");
        }

        db.call(move |conn| {
            let mut delivery = delivery::get(conn, 1)?.unwrap();
            delivery.pr.checks = crate::github::CiStatus::None;
            delivery::record(conn, 1, run_id, &delivery.repository, &delivery.pr)?;
            conn.execute("DELETE FROM run_verifications WHERE run_id = ?1", [run_id])?;
            checks::start(conn, run_id, &delivery.pr.head_sha, &[])
        })
        .await
        .unwrap();
        let (_, _, body) = send(&app, get("/cards/1/activity")).await;
        assert!(body.contains("Non exécutées"));
        assert!(body.contains("/pull-request/merge"), "{body}");

        db.call(move |conn| {
            conn.execute(
                "UPDATE card_pull_requests SET error = '<remote> unavailable' WHERE card_id = 1",
                [],
            )?;
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
        let (_, _, body) = send(&app, get("/cards/1/activity")).await;
        assert!(!body.contains("/pull-request/merge"));
        assert!(body.contains("/pull-request/publish"));
        assert!(!body.contains("<remote>"));

        db.call(move |conn| {
            let mut delivery = delivery::get(conn, 1)?.unwrap();
            delivery.pr.head_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned();
            delivery::record(conn, 1, run_id, &delivery.repository, &delivery.pr)
        })
        .await
        .unwrap();
        let (_, _, body) = send(&app, get("/cards/1/activity")).await;
        assert!(!body.contains("/pull-request/merge"));

        post_form(&app, "/cards/1/move", "column_id=1&position=0").await;
        post_form(&app, "/cards/1/move", "column_id=2&position=0").await;
        let (_, _, body) = send(&app, get("/cards/1/activity")).await;
        assert!(body.contains("exécution précédente"));
        assert!(!body.contains("/pull-request/merge"));
        assert!(!body.contains("/pull-request/publish"));
        assert!(body.contains("Aucun résultat de vérification"));
        for action in ["publish", "refresh", "merge"] {
            let uri = format!("/cards/1/pull-request/{action}");
            assert_eq!(post_form(&app, &uri, "").await, StatusCode::CONFLICT);
        }
    }

    #[tokio::test]
    async fn a_retargeted_pull_request_does_not_offer_merge() {
        let db = Db::open_in_memory().unwrap();
        let app = delivery_app(&db);
        post_form(&app, "/cards", "column_id=2&title=Task&agent=claude").await;
        let run_id = store_verified_pull_request(&db).await;
        db.call(move |conn| {
            let mut saved = delivery::get(conn, 1)?.unwrap();
            saved.pr.base_branch = "other-base".to_owned();
            delivery::record(conn, 1, run_id, &saved.repository, &saved.pr)
        })
        .await
        .unwrap();
        let (_, _, body) = send(&app, get("/cards/1/activity")).await;
        assert!(!body.contains("/pull-request/merge"), "{body}");
        assert!(body.contains("/pull-request/refresh"));
    }

    #[tokio::test]
    async fn delivery_posts_redirect_without_javascript_and_report_remote_errors() {
        let root =
            std::env::temp_dir().join(format!("helm-routes-delivery-{}", std::process::id()));
        let repo = root.join("repo");
        let remote = root.join("repo.gh");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        let mut snapshot = serde_json::json!({
            "number": 17,
            "url": "https://github.com/owner/repo/pull/17",
            "state": "OPEN",
            "isDraft": false,
            "headRefOid": "1234567890123456789012345678901234567890",
            "headRefName": "helm/HELM-1",
            "baseRefName": "main",
            "statusCheckRollup": [],
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "reviewDecision": "APPROVED",
            "isCrossRepository": false,
        });
        std::fs::write(remote.join("view.json"), snapshot.to_string()).unwrap();
        let db = Db::open_in_memory().unwrap();
        let app = delivery_app_with_config(
            &db,
            repo,
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-gh.sh"),
        );
        post_form(&app, "/cards", "column_id=2&title=Task&agent=claude").await;
        store_verified_pull_request(&db).await;

        let uri = "/cards/1/pull-request/refresh";
        let (status, headers, _) = send(&app, post(uri).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers[header::LOCATION], "/cards/1/edit#run-activity");
        assert_eq!(post_form(&app, uri, "").await, StatusCode::NO_CONTENT);

        std::fs::write(remote.join("view.exit"), "1").unwrap();
        std::fs::write(remote.join("view.stderr"), "GitHub indisponible").unwrap();
        let (status, _, body) = send(&app, post(uri).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.contains("GitHub indisponible"), "{body}");
        std::fs::remove_file(remote.join("view.exit")).unwrap();
        std::fs::remove_file(remote.join("view.stderr")).unwrap();

        snapshot["state"] = serde_json::Value::String("MERGED".to_owned());
        std::fs::write(remote.join("after-merge.json"), snapshot.to_string()).unwrap();
        let (status, headers, _) = send(
            &app,
            post("/cards/1/pull-request/merge")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers[header::LOCATION], "/cards/1/edit#run-activity");
        let saved = db
            .call(|conn| delivery::get(conn, 1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.pr.state, crate::github::PullRequestState::Merged);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn the_card_shows_its_runs_activity_live_without_script_and_as_a_fragment() {
        let (app, db) = wired_app();
        post_form(&app, "/cards", "column_id=1&title=Task&agent=claude").await;

        let (_, _, page) = send(&app, get("/cards/1/edit")).await;
        assert!(
            page.contains("id=\"run-activity\"") && page.contains("Aucune exécution"),
            "{page}"
        );

        post_form(&app, "/cards/1/move", "column_id=2&position=0").await;
        let run_id = db
            .call(|conn| {
                let run = runs::claim_next_queued(conn)?.unwrap();
                runs::record_workspace(conn, run.id, "/wt/HELM-1", "helm/HELM-1")?;
                runs::record_session(conn, run.id, "sess-123")?;
                runs::record_usage(conn, run.id, Some(0.05), Some(12), Some(34))?;
                for (kind, summary) in [
                    (runs::EventKind::System, "system:hook_started"),
                    (runs::EventKind::ToolUse, "Bash: <b>ls</b>"),
                    (runs::EventKind::Error, "boom"),
                ] {
                    runs::append_event(
                        conn,
                        run.id,
                        &runs::NewEvent {
                            kind,
                            summary: summary.to_owned(),
                            payload: "{}".to_owned(),
                        },
                    )?;
                }
                Ok::<_, StoreError>(run.id)
            })
            .await
            .unwrap();

        let (status, _, panel) = send(&app, get("/cards/1/activity")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(panel.starts_with("<div class=\"activity\" id=\"run-activity\">"));
        assert!(panel.contains("data-status=\"running\"") && panel.contains("En cours"));
        assert!(panel.contains("helm/HELM-1") && panel.contains("sess-123"));
        assert!(panel.contains("0,0500 $") && panel.contains("12 → 34"));
        assert!(
            panel.contains("action=\"/runs/1/cancel\""),
            "a running run can be cancelled"
        );
        assert!(
            panel.contains("Bash: &lt;b&gt;ls&lt;/b&gt;") || panel.contains("Bash: &#60;b&#62;ls"),
            "escaped: {panel}"
        );
        assert!(
            !panel.contains("hook_started"),
            "CLI bookkeeping is not shown"
        );
        assert!(
            panel.contains("3 événements enregistrés, 1 non affichés (1 de bruit interne du CLI)")
        );
        assert!(
            !panel.contains("plus anciens"),
            "nothing is older than the limit here: {panel}"
        );
        assert!(panel.find("Bash:").unwrap() < panel.find("boom").unwrap());
        assert!(
            !panel.contains("<form class=\"form\""),
            "the fragment never carries the card form"
        );

        // The card dialog and the full page embed the same panel, after the form.
        let (_, _, page) = send(&app, get("/cards/1/edit")).await;
        assert!(page.contains("Activité de l'agent") && page.contains("sess-123"));
        let (_, _, board) = send(&app, get("/board")).await;
        assert!(
            board.contains("class=\"run-status\" data-status=\"running\""),
            "{board}"
        );

        db.call(move |conn| {
            runs::finish(
                conn,
                run_id,
                &runs::Outcome::Failed("push refused".to_owned()),
            )
        })
        .await
        .unwrap();
        let (_, _, panel) = send(&app, get("/cards/1/activity")).await;
        assert!(panel.contains("data-status=\"failed\"") && panel.contains("push refused"));
        assert!(
            !panel.contains("/runs/1/cancel"),
            "a finished run has nothing to cancel"
        );

        assert_eq!(
            send(&app, get("/cards/42/activity")).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn a_new_run_event_wakes_open_browsers_through_the_shared_notifier() {
        let db = Db::open_in_memory().unwrap();
        let state = AppState::new(db, true, orchestrator(RunGate::Open));
        let changes = state.changes();
        let app = router(state);
        let response = app.clone().oneshot(get("/events")).await.unwrap();
        let mut body = response.into_body();
        changes.publish();
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("an SSE frame after a supervisor publish")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(
            text.contains("event: board") && text.contains("data: 1"),
            "{text}"
        );
    }

    const COMMENT: &str =
        "body=%40codex+please+look%0Amail+a%40codex.com+%3Cscript%3Ex%3C%2Fscript%3E";

    #[tokio::test]
    async fn a_comment_posted_without_script_lands_in_the_thread_on_the_card_page() {
        let app = app();
        post_form(&app, "/cards", "column_id=1&title=Talk").await;

        let request = post("/cards/1/comments").body(Body::from(COMMENT)).unwrap();
        let (status, headers, _) = send(&app, request).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers[header::LOCATION], "/cards/1/edit#comment-1");

        let (_, _, page) = send(&app, get("/cards/1/edit")).await;
        assert!(page.contains("id=\"comment-1\""), "{page}");
        assert!(page.contains("data-author-kind=\"human\""));
        assert!(page.contains("moi"));
        assert!(page.contains("<span class=\"mention\" data-target=\"codex\">@codex</span> please look\nmail a@codex.com"));
        assert!(!page.contains("<script>x"), "comment body must be escaped");
        assert!(page.contains("&lt;script&gt;x") || page.contains("&#60;script&#62;x"));
        assert!(page.contains("action=\"/cards/1/comments\""));
    }

    #[tokio::test]
    async fn the_dialog_fragment_and_the_thread_fragment_show_comments_in_order() {
        let app = app();
        post_form(&app, "/cards", "column_id=1&title=Talk").await;
        assert_eq!(
            post_form(&app, "/cards/1/comments", "body=first").await,
            StatusCode::NO_CONTENT
        );
        post_form(&app, "/cards/1/comments", "body=second").await;

        let mut dialog = get("/cards/1/edit");
        dialog
            .headers_mut()
            .insert(FETCH_HEADER, HeaderValue::from_static("fetch"));
        let (_, _, dialog) = send(&app, dialog).await;
        assert!(!dialog.contains("<html"));
        assert!(dialog.find("first").unwrap() < dialog.find("second").unwrap());

        let (status, _, thread) = send(&app, get("/cards/1/comments")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(thread.starts_with("<div class=\"thread\" id=\"comment-thread\">"));
        assert!(!thread.contains("<form"));
        assert!(thread.find("first").unwrap() < thread.find("second").unwrap());
    }

    #[tokio::test]
    async fn the_board_shows_a_comment_count_only_when_it_is_not_zero() {
        let app = app();
        post_form(&app, "/cards", "column_id=1&title=Quiet").await;
        let (_, _, board) = send(&app, get("/board")).await;
        assert!(!board.contains("card__comments"));

        post_form(&app, "/cards/1/comments", "body=a").await;
        post_form(&app, "/cards/1/comments", "body=b").await;
        let (_, _, board) = send(&app, get("/board")).await;
        assert!(board.contains("card__comments"));
        assert!(board.contains("Commentaires : </span>2"), "{board}");
    }

    #[tokio::test]
    async fn bad_comment_requests_are_reported_and_store_nothing() {
        let app = app();
        post_form(&app, "/cards", "column_id=1&title=Talk").await;
        assert_eq!(
            post_form(&app, "/cards/1/comments", "body=+%0A+").await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            post_form(&app, "/cards/42/comments", "body=hi").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send(&app, get("/cards/42/comments")).await.0,
            StatusCode::NOT_FOUND
        );
        let (_, _, thread) = send(&app, get("/cards/1/comments")).await;
        assert!(thread.contains("Aucun commentaire."));
    }

    #[tokio::test]
    async fn a_cross_site_page_cannot_post_comments() {
        let app = app();
        post_form(&app, "/cards", "column_id=1&title=Talk").await;
        let forged = post("/cards/1/comments")
            .header(header::ORIGIN, "http://evil.example")
            .body(Body::from("body=@codex+rm+-rf"))
            .unwrap();
        assert_eq!(send(&app, forged).await.0, StatusCode::FORBIDDEN);
        let (_, _, thread) = send(&app, get("/cards/1/comments")).await;
        assert!(thread.contains("Aucun commentaire."));
    }

    #[tokio::test]
    async fn posting_a_comment_wakes_the_open_browsers() {
        let app = app();
        post_form(&app, "/cards", "column_id=1&title=Talk").await;
        let response = app.clone().oneshot(get("/events")).await.unwrap();
        let mut body = response.into_body();
        assert_eq!(
            post_form(&app, "/cards/1/comments", "body=hello").await,
            StatusCode::NO_CONTENT
        );
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("an SSE frame after a comment")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(text.contains("event: board"), "{text}");
        assert!(text.contains("data: 2"), "{text}");
    }
}
