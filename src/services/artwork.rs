use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

/// Shared reqwest client for artwork downloads. cache_image runs once
/// per cover/banner/relation-card import — many in a row when the
/// metadata sweep refreshes a series — and previously rebuilt the TLS
/// pool every call. Timeouts (10s connect, 30s overall) bound a hung
/// CDN connection so it can't pin a pool slot waiting for TCP keepalive.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("building the artwork reqwest client should not fail")
});

use crate::models::artwork_cache;

fn sanitize_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn extension_for(content_type: &str, url: &str) -> &'static str {
    let ct = content_type.to_ascii_lowercase();
    if ct.contains("png") || url.ends_with(".png") {
        "png"
    } else if ct.contains("webp") || url.ends_with(".webp") {
        "webp"
    } else if ct.contains("gif") || url.ends_with(".gif") {
        "gif"
    } else {
        "jpg"
    }
}

fn blob_filename(blob_hash: &str, content_type: &str, source_url: &str) -> String {
    format!("{}.{}", blob_hash, extension_for(content_type, source_url))
}

/// Artwork blob cache root: `RYOKAN_MEDIA_CACHE_DIR`, else
/// `<data dir>/cache/artwork` (`services::paths`, #259).
pub fn media_cache_dir() -> PathBuf {
    let base =
        crate::services::paths::override_or_data_path("RYOKAN_MEDIA_CACHE_DIR", "cache/artwork");
    // Always store absolute paths. Older builds wrote the relative
    // default ("data/cache/artwork/blobs/<hash>.jpg") straight into
    // image_blobs.local_path; those rows then break whenever the
    // process CWD changes (e.g. a Docker image that later adds
    // WORKDIR or sets RYOKAN_MEDIA_CACHE_DIR). Canonicalizing here
    // means every new write records an absolute path regardless of
    // how the env is configured.
    std::path::absolute(&base).unwrap_or(base)
}

/// The same blob under the current cache root, for a stored path that
/// no longer resolves. `image_blobs.local_path` is absolute, so moving
/// the data directory (#259: `/data` to `/config`) or restoring a
/// backup onto another layout leaves rows pointing at the old place
/// until `cache_image` fetches the image again. Blob file names are
/// content-addressed, so the file of that name under the current root
/// is the same image. `None` when the stored path already is that file.
pub fn relocated_blob_path(stored: &str) -> Option<PathBuf> {
    relocate_into(stored, &media_cache_dir())
}

fn relocate_into(stored: &str, cache_dir: &Path) -> Option<PathBuf> {
    let candidate = cache_dir.join("blobs").join(Path::new(stored).file_name()?);
    (candidate != Path::new(stored)).then_some(candidate)
}

pub fn local_url(cache_key: &str, last_write: i64) -> String {
    format!("/media/art/{}?v={}", cache_key, last_write)
}

pub fn canonical_identity_key(provider_id: i64, mal_id: Option<i64>) -> String {
    if let Some(mid) = mal_id.filter(|mid| *mid > 0) {
        format!("mal-{}", mid)
    } else if provider_id > 0 {
        format!("al-{}", provider_id)
    } else {
        format!("prov-{}", provider_id)
    }
}

pub fn series_relation_cover_key(
    series_id: i64,
    related_provider_id: i64,
    related_mal_id: Option<i64>,
) -> String {
    format!(
        "series-{}-relation-{}-cover",
        series_id,
        canonical_identity_key(related_provider_id, related_mal_id)
    )
}

pub fn provider_cover_key(provider_id: i64, mal_id: Option<i64>) -> String {
    format!(
        "provider-{}-cover",
        canonical_identity_key(provider_id, mal_id)
    )
}

pub fn provider_banner_key(provider_id: i64, mal_id: Option<i64>) -> String {
    format!(
        "provider-{}-banner",
        canonical_identity_key(provider_id, mal_id)
    )
}

pub fn provider_relation_cover_key(
    provider_id: i64,
    related_provider_id: i64,
    related_mal_id: Option<i64>,
) -> String {
    format!(
        "provider-{}-relation-{}-cover",
        canonical_identity_key(provider_id, None),
        canonical_identity_key(related_provider_id, related_mal_id)
    )
}

pub async fn first_cached_url(db: &SqlitePool, cache_keys: &[String], source_url: &str) -> String {
    for key in cache_keys {
        if let Ok(Some(url)) = artwork_cache::get_local_url(db, key).await {
            return url;
        }
    }
    source_url.to_string()
}

pub async fn cache_image(
    db: &SqlitePool,
    cache_key: &str,
    parent_kind: &str,
    parent_id: Option<i64>,
    image_kind: &str,
    source_url: &str,
) -> Result<Option<String>, String> {
    if source_url.trim().is_empty() {
        return Ok(None);
    }

    let safe_key = sanitize_key(cache_key);
    let resp = HTTP_CLIENT
        .get(source_url)
        .header("User-Agent", "Ryokan/0.1")
        .send()
        .await
        .map_err(|e| format!("artwork request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!(
            "artwork request failed with HTTP {}",
            resp.status()
        ));
    }

    let headers = resp.headers().clone();
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("artwork body failed: {}", e))?;

    let blob_hash = hex::encode(Sha256::digest(&bytes));
    let dir = media_cache_dir().join("blobs");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create artwork dir failed: {}", e))?;

    // Check whether we already have a row for this hash, and if so whether
    // the on-disk file is still where the DB says it is. `has_blob` used to
    // only return a bool; that meant a row with a stale relative path (or
    // a blob file that had been deleted out from under us) would short-
    // circuit the write forever, because the ref row would get refreshed
    // while the broken blob row sat untouched. Self-heal: if the stored
    // path is missing, rewrite the file to the current (absolute) path
    // and upsert_blob so image_blobs is updated in place.
    let existing_path = artwork_cache::get_blob_path(db, &blob_hash)
        .await
        .map_err(|e| e.to_string())?;

    let file_is_live = existing_path
        .as_deref()
        .map(|p| std::path::Path::new(p).is_file())
        .unwrap_or(false);

    if !file_is_live {
        let filename = blob_filename(&blob_hash, &content_type, source_url);
        let path = dir.join(&filename);
        if !path.exists() {
            std::fs::write(&path, &bytes).map_err(|e| format!("write artwork failed: {}", e))?;
        }
        artwork_cache::upsert_blob(
            db,
            &blob_hash,
            &path.to_string_lossy(),
            &content_type,
            bytes.len() as i64,
        )
        .await
        .map_err(|e| e.to_string())?;
    }

    let last_write = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    artwork_cache::upsert_ref(
        db,
        artwork_cache::RefUpsert {
            cache_key: &safe_key,
            parent_kind,
            parent_id,
            image_kind,
            source_url,
            blob_hash: &blob_hash,
            last_write,
        },
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(Some(local_url(&safe_key, last_write)))
}

pub async fn cache_series_detail_artwork(
    db: &SqlitePool,
    series_id: i64,
    detail: &crate::services::anilist::AnimeDetail,
) {
    let _ = cache_image(
        db,
        &format!("series-{}-cover", series_id),
        "series",
        Some(series_id),
        "cover",
        &detail.cover_url,
    )
    .await;
    let _ = cache_image(
        db,
        &format!("series-{}-banner", series_id),
        "series",
        Some(series_id),
        "banner",
        &detail.banner_url,
    )
    .await;
}

pub async fn cache_relation_artwork(
    db: &SqlitePool,
    series_id: i64,
    related_provider_id: i64,
    related_mal_id: Option<i64>,
    source_url: &str,
) {
    let _ = cache_image(
        db,
        &series_relation_cover_key(series_id, related_provider_id, related_mal_id),
        "series_relation",
        Some(series_id),
        "cover",
        source_url,
    )
    .await;
}

pub async fn cache_provider_detail_artwork(
    db: &SqlitePool,
    provider_id: i64,
    mal_id: Option<i64>,
    detail: &crate::services::anilist::AnimeDetail,
) {
    let _ = cache_image(
        db,
        &provider_cover_key(provider_id, mal_id),
        "provider",
        Some(provider_id),
        "cover",
        &detail.cover_url,
    )
    .await;
    let _ = cache_image(
        db,
        &provider_banner_key(provider_id, mal_id),
        "provider",
        Some(provider_id),
        "banner",
        &detail.banner_url,
    )
    .await;
}

pub async fn cache_provider_relation_artwork(
    db: &SqlitePool,
    provider_id: i64,
    related_provider_id: i64,
    related_mal_id: Option<i64>,
    source_url: &str,
) {
    let _ = cache_image(
        db,
        &provider_relation_cover_key(provider_id, related_provider_id, related_mal_id),
        "provider_relation",
        Some(provider_id),
        "cover",
        source_url,
    )
    .await;
}

pub async fn cached_or_source_url(db: &SqlitePool, cache_key: &str, source_url: &str) -> String {
    artwork_cache::get_local_url(db, cache_key)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| source_url.to_string())
}

pub async fn load_bytes(db: &SqlitePool, cache_key: &str) -> Option<(Vec<u8>, String)> {
    let entry = artwork_cache::get(db, cache_key).await.ok().flatten()?;
    // Use tokio::fs::read so the artwork serving path doesn't block a
    // runtime worker — Seerr does a lot of artwork lookups during
    // discovery scans and the sync read would stack up behind itself.
    let bytes = match tokio::fs::read(Path::new(&entry.local_path)).await {
        Ok(bytes) => bytes,
        Err(_) => tokio::fs::read(relocated_blob_path(&entry.local_path)?)
            .await
            .ok()?,
    };
    Some((bytes, entry.content_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── relocated_blob_path ──────────────────────────────────────

    #[test]
    fn relocate_into_maps_a_moved_blob_onto_the_current_root() {
        assert_eq!(
            relocate_into(
                "/data/cache/artwork/blobs/abc.jpg",
                Path::new("/config/cache/artwork")
            ),
            Some(PathBuf::from("/config/cache/artwork/blobs/abc.jpg"))
        );
    }

    #[test]
    fn relocate_into_is_none_when_nothing_moved() {
        assert_eq!(
            relocate_into(
                "/data/cache/artwork/blobs/abc.jpg",
                Path::new("/data/cache/artwork")
            ),
            None
        );
        assert_eq!(relocate_into("", Path::new("/data/cache/artwork")), None);
    }

    // ─── sanitize_key ─────────────────────────────────────────────

    #[test]
    fn sanitize_key_preserves_alphanumeric_and_dash_and_underscore() {
        assert_eq!(sanitize_key("abc-123_XYZ"), "abc-123_XYZ");
    }

    #[test]
    fn sanitize_key_replaces_slashes_and_spaces_with_dash() {
        // Cache keys flow into URL paths and filesystem filenames —
        // anything that isn't [A-Za-z0-9_-] gets dashed to avoid a
        // caller from injecting path segments or breaking the URL.
        assert_eq!(
            sanitize_key("provider/id with space"),
            "provider-id-with-space"
        );
    }

    #[test]
    fn sanitize_key_replaces_dots_and_colons() {
        // `.` and `:` are path-traversal hazards and URL-parse
        // hazards respectively — both get dashed.
        assert_eq!(sanitize_key("foo.bar:baz"), "foo-bar-baz");
    }

    #[test]
    fn sanitize_key_empty_input_returns_empty() {
        assert_eq!(sanitize_key(""), "");
    }

    // ─── extension_for ────────────────────────────────────────────

    #[test]
    fn extension_for_png_content_type_returns_png() {
        assert_eq!(extension_for("image/png", "http://example.com/img"), "png");
    }

    #[test]
    fn extension_for_webp_content_type_returns_webp() {
        assert_eq!(
            extension_for("image/webp", "http://example.com/img"),
            "webp"
        );
    }

    #[test]
    fn extension_for_falls_back_to_jpg_on_unknown_content_type() {
        // Default is jpg — JPEG is the most common provider format
        // and covers the unknown-content-type case without forcing a
        // rediscovery on every new shape.
        assert_eq!(extension_for("application/octet-stream", ""), "jpg");
    }

    #[test]
    fn extension_for_falls_back_to_url_suffix_when_content_type_silent() {
        // AniList sometimes returns `image/jpeg` for PNGs and vice
        // versa. URL suffix is the tiebreaker.
        assert_eq!(
            extension_for("image/jpeg", "http://cdn.example.com/art.png"),
            "png"
        );
    }

    // ─── blob_filename ────────────────────────────────────────────

    #[test]
    fn blob_filename_combines_hash_and_extension() {
        assert_eq!(
            blob_filename("abc123", "image/png", "http://x.com/i.png"),
            "abc123.png"
        );
    }

    #[test]
    fn blob_filename_uses_jpg_default_for_unknown_types() {
        assert_eq!(
            blob_filename("deadbeef", "application/json", "http://x.com/i"),
            "deadbeef.jpg"
        );
    }

    // ─── local_url ────────────────────────────────────────────────

    #[test]
    fn local_url_embeds_cache_key_and_cachebust_param() {
        // The `?v=<epoch>` query param forces browser revalidation
        // when the artwork's last_write changes — without it every
        // Jellyfin client caches a stale cover forever.
        assert_eq!(
            local_url("provider-al-12345-cover", 1_700_000_000),
            "/media/art/provider-al-12345-cover?v=1700000000"
        );
    }

    // ─── canonical_identity_key ───────────────────────────────────

    #[test]
    fn canonical_identity_key_prefers_mal_id_when_positive() {
        // MAL ID is the more stable external key — AniList IDs shift
        // during provider re-imports, MAL rarely changes. Preferring
        // MAL when available means a series re-imported from
        // AniList-fallback-to-Jikan still hits the cached artwork.
        assert_eq!(canonical_identity_key(123, Some(456)), "mal-456");
    }

    #[test]
    fn canonical_identity_key_falls_back_to_anilist_id_when_mal_absent() {
        assert_eq!(canonical_identity_key(123, None), "al-123");
    }

    #[test]
    fn canonical_identity_key_ignores_non_positive_mal_id() {
        // A negative or zero mal_id is the Jikan-fallback sentinel
        // shape (`-mal_id` stored in `series.anilist_id` for
        // series added via the MAL fallback). Filter those out so
        // we don't generate a `mal-0` cache key.
        assert_eq!(canonical_identity_key(123, Some(0)), "al-123");
        assert_eq!(canonical_identity_key(123, Some(-1)), "al-123");
    }

    #[test]
    fn canonical_identity_key_uses_prov_prefix_for_negative_provider_id() {
        // Negative provider_id is the sentinel for MAL-fallback series
        // without an AniList mapping. Emit `prov-<negid>` so the key
        // is still disambiguated but doesn't claim to be an AL id.
        assert_eq!(canonical_identity_key(-12345, None), "prov--12345");
    }

    // ─── provider_cover_key / provider_banner_key ─────────────────

    #[test]
    fn provider_cover_key_combines_identity_key_with_cover_suffix() {
        assert_eq!(provider_cover_key(123, Some(456)), "provider-mal-456-cover");
    }

    #[test]
    fn provider_banner_key_combines_identity_key_with_banner_suffix() {
        assert_eq!(
            provider_banner_key(123, Some(456)),
            "provider-mal-456-banner"
        );
    }

    #[test]
    fn provider_cover_and_banner_keys_share_identity_prefix() {
        // If the identity prefix diverges between cover and banner,
        // a series-rename + re-scan would double-write one of the
        // two. Pin the invariant that they share the prefix.
        let cover = provider_cover_key(42, None);
        let banner = provider_banner_key(42, None);
        let cover_prefix = cover.trim_end_matches("-cover");
        let banner_prefix = banner.trim_end_matches("-banner");
        assert_eq!(cover_prefix, banner_prefix);
    }

    // ─── series_relation_cover_key / provider_relation_cover_key ──

    #[test]
    fn series_relation_cover_key_includes_parent_series_and_related_identity() {
        assert_eq!(
            series_relation_cover_key(10, 20, Some(30)),
            "series-10-relation-mal-30-cover"
        );
    }

    #[test]
    fn provider_relation_cover_key_uses_parent_provider_id_without_mal() {
        // The parent identity in a provider-relation key intentionally
        // omits `mal_id` — the relation is defined from the provider's
        // perspective, so using the provider's AL id keeps the
        // relation graph consistent when the same series has a
        // different MAL id in different relation trees.
        assert_eq!(
            provider_relation_cover_key(10, 20, Some(30)),
            "provider-al-10-relation-mal-30-cover"
        );
    }
}
