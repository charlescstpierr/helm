//! Static web assets, embedded in the binary in release builds.
//!
//! In debug builds `rust-embed` reads `assets/` from disk on every request, so CSS and JS
//! can be edited without recompiling.

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// Serves one embedded file, revalidated by content hash so upgrades are picked up at once.
pub async fn serve(Path(path): Path<String>, headers: HeaderMap) -> Response {
    let Some(file) = Assets::get(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let hash = file.metadata.sha256_hash();
    let etag = format!(
        "\"{}\"",
        hash[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let cache = [
        (header::ETAG, etag.clone()),
        (header::CACHE_CONTROL, "no-cache".to_owned()),
    ];
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == etag);
    if unchanged {
        return (StatusCode::NOT_MODIFIED, cache).into_response();
    }
    (
        cache,
        [(header::CONTENT_TYPE, content_type(&path))],
        file.data.into_owned(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_asset_is_embedded() {
        for path in ["tokens.css", "app.css", "app.js", "theme.js", "favicon.svg"] {
            assert!(Assets::get(path).is_some(), "missing asset {path}");
        }
    }

    /// The visual charter plugs in through `tokens.css` alone: component styles must not
    /// hard-code any colour.
    #[test]
    fn component_styles_only_use_colour_tokens() {
        let css = Assets::get("app.css").unwrap();
        let css = std::str::from_utf8(&css.data).unwrap();
        for (number, line) in css.lines().enumerate() {
            let lowered = line.to_lowercase();
            let hex_colour = lowered.match_indices('#').any(|(at, _)| {
                lowered[at + 1..]
                    .chars()
                    .take(3)
                    .filter(char::is_ascii_hexdigit)
                    .count()
                    == 3
            });
            let colour_function = ["rgb(", "rgba(", "hsl(", "hsla(", "oklch(", "oklab("]
                .iter()
                .any(|function| lowered.contains(function));
            assert!(
                !hex_colour && !colour_function,
                "app.css:{} hard-codes a colour, use a token from tokens.css: {line}",
                number + 1
            );
        }
    }

    /// Everything is served from the binary: no asset may point at another host.
    #[test]
    fn assets_reference_no_external_host() {
        for path in Assets::iter() {
            let file = Assets::get(&path).unwrap();
            let text = String::from_utf8_lossy(&file.data).replace("http://www.w3.org/", "");
            assert!(
                !text.contains("http://") && !text.contains("https://") && !text.contains("url(//"),
                "{path} references an external URL"
            );
        }
    }
}
