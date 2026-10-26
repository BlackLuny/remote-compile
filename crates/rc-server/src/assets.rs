//! The admin console is compiled into the binary (§14.1), so deployment is a
//! single file with no separate frontend artefact to keep in sync.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "$CARGO_MANIFEST_DIR/../../web/dist"]
struct Assets;

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(file) = Assets::get(path) {
        return respond(path, file.data.into_owned());
    }

    // Unknown non-API paths are client-side routes: hand back the shell and
    // let the router sort it out.
    if !path.starts_with("api/") {
        if let Some(index) = Assets::get("index.html") {
            return respond("index.html", index.data.into_owned());
        }
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn respond(path: &str, body: Vec<u8>) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // Vite emits content-hashed asset filenames, so those are immutable; the
    // shell must never be cached at all. A shell served from cache points at
    // asset hashes the deploy has already deleted, and the tab then waits on
    // a file that will never arrive — exactly the "loading spinner forever"
    // failure. `no-store` keeps the shell out of every cache, including the
    // back/forward one, so a reload always fetches a shell matching the
    // binary that is actually running.
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-store"
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime.as_ref()),
            (header::CACHE_CONTROL, cache),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serves_the_shell_for_client_routes() {
        let res = serve("/tasks/t-123".parse().unwrap()).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html"
        );
    }

    #[tokio::test]
    async fn unknown_api_paths_do_not_get_the_html_shell() {
        // Returning HTML for a missing API route makes client errors look like
        // a parse failure instead of a 404.
        let res = serve("/api/does-not-exist".parse().unwrap()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_shell_is_never_cached_while_hashed_assets_are_immutable() {
        // A cached shell outlives the deploy that changed the asset hashes: it
        // keeps asking for files the new binary no longer embeds, and the tab
        // spins on a request that can never be answered.
        let shell = serve("/tasks".parse().unwrap()).await;
        assert_eq!(shell.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");

        let asset = Assets::iter()
            .find(|p| p.starts_with("assets/"))
            .expect("the console build must be embedded");
        let res = serve(format!("/{asset}").parse().unwrap()).await;
        assert_eq!(
            res.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable"
        );
    }
}
