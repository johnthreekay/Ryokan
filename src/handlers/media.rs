use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderValue, Response, StatusCode, header},
};

use crate::{AppState, services::artwork};

pub async fn artwork(
    State(state): State<AppState>,
    Path(cache_key): Path<String>,
) -> Result<Response<Body>, StatusCode> {
    let (bytes, _stored_type) = artwork::load_bytes(&state.db, &cache_key)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    // The type comes from the bytes, not the row: rows cached before
    // `cache_image` sniffed hold whatever `Content-Type` the provider
    // sent, and a blob that isn't an image is never served.
    let content_type = artwork::sniff_image_type(&bytes).ok_or(StatusCode::NOT_FOUND)?;

    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    // Opened on its own, an image needs no script, style or frame; the
    // sandbox also denies the document an origin. `security_headers`
    // keeps a handler's own policy instead of adding the site one.
    resp.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    Ok(resp)
}
