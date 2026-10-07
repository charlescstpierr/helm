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

    fn css_without_comments(css: &str) -> String {
        let mut out = String::new();
        let mut rest = css;
        while let Some(start) = rest.find("/*") {
            out.push_str(&rest[..start]);
            rest = rest[start + 2..]
                .split_once("*/")
                .map_or("", |(_, after)| after);
        }
        out + rest
    }

    /// Splits a stylesheet into `(property, value)` pairs.
    fn declarations(css: &str) -> Vec<(String, String)> {
        css_without_comments(css)
            .split([';', '{', '}'])
            .filter_map(|part| part.split_once(':'))
            .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_lowercase()))
            .collect()
    }

    /// The value with every parenthesised argument list removed, so `var(--x)` vanishes and
    /// `rgb(0 0 0)` is left as the bare function name.
    fn without_arguments(value: &str) -> String {
        let mut depth = 0;
        let mut out = String::new();
        for ch in value.chars() {
            match ch {
                '(' => depth += 1,
                ')' if depth > 0 => depth -= 1,
                _ if depth == 0 => out.push(ch),
                _ => {}
            }
        }
        out
    }

    /// The visual charter plugs in through `tokens.css` alone: component styles must not
    /// hard-code any colour.
    #[test]
    fn component_styles_only_use_colour_tokens() {
        const COLOUR_FUNCTIONS: [&str; 6] = ["rgb", "rgba", "hsl", "hsla", "oklch", "oklab"];
        const COLOUR_PROPERTIES: [&str; 8] = [
            "background",
            "border",
            "outline",
            "fill",
            "stroke",
            "box-shadow",
            "text-shadow",
            "caret",
        ];
        let css = Assets::get("app.css").unwrap();
        let css = std::str::from_utf8(&css.data).unwrap();
        for (name, value) in declarations(css) {
            let colours_something = name.contains("color")
                || COLOUR_PROPERTIES
                    .iter()
                    .any(|property| name.starts_with(property));
            if name.starts_with("--") || !colours_something {
                continue;
            }
            let hard_coded = without_arguments(&value)
                .split(|ch: char| ch.is_whitespace() || ch == ',')
                .any(|token| token.starts_with('#') || COLOUR_FUNCTIONS.contains(&token));
            assert!(
                !hard_coded,
                "app.css hard-codes a colour in `{name}: {value}`, use a token from tokens.css"
            );
        }
    }

    /// Everything is served from the binary: no stylesheet may load another host.
    #[test]
    fn assets_reference_no_external_host() {
        for path in Assets::iter().filter(|path| path.ends_with(".css")) {
            let file = Assets::get(&path).unwrap();
            let css = css_without_comments(std::str::from_utf8(&file.data).unwrap());
            for chunk in css.split("url(").chain(css.split("@import")).skip(1) {
                let target = chunk
                    .trim_start()
                    .trim_start_matches(['"', '\''])
                    .to_lowercase();
                assert!(
                    !(target.starts_with("http:")
                        || target.starts_with("https:")
                        || target.starts_with("//")),
                    "{path} loads an external resource: {target}"
                );
            }
        }
    }
}
