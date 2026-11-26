use axum::body::Body;
use axum::http::{header, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rust_embed::RustEmbed;
use serde_json::json;

#[derive(RustEmbed)]
#[folder = "public/"]
pub struct PublicAssets;

pub async fn serve_embedded_asset(method: Method, uri: Uri) -> Response {
    let raw_path = uri.path();

    if method == Method::GET || method == Method::HEAD {
        let rel_path = raw_path.trim_start_matches('/');
        let target = if rel_path.is_empty() {
            "index.html"
        } else {
            rel_path
        };

        if let Some(asset) = PublicAssets::get(target) {
            let mime = mime_guess::from_path(target).first_or_octet_stream();
            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref());

            if target.ends_with(".html") {
                builder = builder.header(header::CACHE_CONTROL, "no-cache");
            }

            let body = if method == Method::HEAD {
                Body::empty()
            } else {
                Body::from(asset.data.into_owned())
            };

            return builder
                .body(body)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "type": "error",
            "error": {
                "type": "not_found_error",
                "message": format!("Endpoint {method} {raw_path} not found")
            }
        })),
    )
        .into_response()
}
