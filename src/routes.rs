//! HTTP surface: server-rendered pages, form endpoints, the SSE stream and request guards.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
use tokio::sync::broadcast::{self, error::RecvError};

use crate::assets;
use crate::db::Db;
use crate::store::{self, Author, Board, Card, CardInput, Comment, StoreError};

/// Sent by `assets/app.js` on its own requests: the server then answers with a fragment or
/// an empty 204 instead of a full page or a redirect.
const FETCH_HEADER: &str = "x-helm-request";
const CONTENT_SECURITY_POLICY: &str =
    "default-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";
const SSE_KEEP_ALIVE: Duration = Duration::from_secs(25);

#[derive(Clone)]
pub struct AppState {
    db: Db,
    /// Carries the board revision after each change; subscribers just reload the board.
    events: broadcast::Sender<u64>,
    revision: Arc<AtomicU64>,
    /// When bound to loopback, only loopback `Host` names are served (DNS-rebinding guard).
    loopback_only: bool,
}

impl AppState {
    pub fn new(db: Db, loopback_only: bool) -> Self {
        let (events, _) = broadcast::channel(16);
        Self {
            db,
            events,
            revision: Arc::new(AtomicU64::new(0)),
            loopback_only,
        }
    }

    fn board_changed(&self) {
        let revision = self.revision.fetch_add(1, Ordering::Relaxed) + 1;
        // No subscriber simply means no browser tab is open.
        let _ = self.events.send(revision);
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
        .route(
            "/cards/{id}/delete",
            get(confirm_delete_card).post(delete_card),
        )
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
    Internal(String),
}

impl From<StoreError> for AppError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound => Self::NotFound,
            StoreError::Invalid(message) => Self::Invalid(message),
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
}

#[derive(Template)]
#[template(path = "_card_panel.html")]
struct CardPanelFragment {
    board: Board,
    card: Card,
    comments: Vec<Comment>,
}

#[derive(Template)]
#[template(path = "_thread.html")]
struct ThreadFragment {
    comments: Vec<Comment>,
}

async fn board_page(State(state): State<AppState>) -> AppResult<Html<String>> {
    let board = state.db.call(|conn| store::load_board(conn)).await?;
    Ok(Html(BoardPage { board }.render()?))
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
    let (board, card, comments) = state
        .db
        .call(move |conn| {
            Ok::<_, StoreError>((
                store::load_board(conn)?,
                store::get_card(conn, id)?,
                store::list_comments(conn, id)?,
            ))
        })
        .await?;
    let html = if is_fetch(&headers) {
        CardPanelFragment {
            board,
            card,
            comments,
        }
        .render()?
    } else {
        CardEditPage {
            board,
            card,
            comments,
        }
        .render()?
    };
    Ok(Html(html))
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
            },
        )
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
    state
        .db
        .call(move |conn| store::create_card(conn, column_id, &input))
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
    state
        .db
        .call(move |conn| store::update_card(conn, id, column_id, &input))
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
    state
        .db
        .call(move |conn| store::move_card(conn, id, form.column_id, form.position))
        .await?;
    state.board_changed();
    Ok(written(&headers))
}

async fn delete_card(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> AppResult<Response> {
    state
        .db
        .call(move |conn| store::delete_card(conn, id))
        .await?;
    state.board_changed();
    Ok(written(&headers))
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
    let receiver = state.events.subscribe();
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
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app() -> Router {
        router(AppState::new(Db::open_in_memory().unwrap(), true))
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
