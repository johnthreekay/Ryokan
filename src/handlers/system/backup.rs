//! System → Backup (issue #126): download / save a backup, list and
//! manage the backup folder, upload and stage a restore, cancel a
//! staged restore. The work lives in `services::backup`; this module
//! is the HTTP shape around it.
//!
//! Downloads stream from a temp dir that is removed when the response
//! body is dropped (finished or abandoned). Uploads stream to a temp
//! file first, so a multi-gigabyte artwork backup never sits in
//! memory. Both stay cookie-authenticated: a backup carries the
//! encryption key and every stored password, and no API-key scope
//! from #114 is broad enough to hand that out.

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum_htmx::HxRequest;
use futures_util::StreamExt;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

use crate::AppState;
use crate::handlers::responses::htmx_aware_redirect;
use crate::models::log::LogCategory;
use crate::models::{config, scheduled_tasks};
use crate::services::backup::{
    self, BACKUP_WORK_DIR_NAME, BackupError, BackupKind, BackupOptions, BackupPaths,
    MAX_UPLOAD_BYTES, RESTORE_WORK_DIR_NAME, RestoreError,
};
use crate::services::logger;

pub struct BackupTabView {
    pub backup_dir: String,
    pub files: Vec<BackupFileView>,
    pub pending: Option<PendingView>,
    pub schedule: &'static str,
    pub retention: i64,
    pub free_space: Option<String>,
}

pub struct BackupFileView {
    pub name: String,
    pub size: String,
    pub when: String,
    pub pre_restore: bool,
}

pub struct PendingView {
    pub when: String,
    pub version: String,
    pub includes_key: bool,
    pub includes_artwork: bool,
    pub sanitized: bool,
}

fn when_label(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

pub async fn backup_tab_view(state: &AppState) -> BackupTabView {
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let paths = BackupPaths::from_env();
    let dir = paths.backup_dir(&cfg.backup_directory);
    let dir_for_task = dir.clone();
    // Free space is reported for the backup folder, which is what the
    // page labels it as; the work dir under the data dir is checked at
    // backup time by `create_backup`.
    let (files, free) = tokio::task::spawn_blocking(move || {
        (
            backup::list_backups(&dir_for_task),
            backup::free_bytes(&dir_for_task),
        )
    })
    .await
    .unwrap_or_default();
    let pending = backup::pending_restore(&paths).map(|m| PendingView {
        when: m.timestamp_label(),
        version: m.ryokan_version.clone(),
        includes_key: m.includes_key,
        includes_artwork: m.includes_artwork,
        sanitized: m.sanitized,
    });
    BackupTabView {
        backup_dir: dir.display().to_string(),
        files: files
            .into_iter()
            .map(|f| BackupFileView {
                name: f.name,
                size: backup::human_bytes(f.size_bytes),
                when: when_label(f.timestamp),
                pre_restore: f.kind == BackupKind::PreRestore,
            })
            .collect(),
        pending,
        schedule: match cfg.backup_schedule.as_str() {
            "daily" => "daily",
            "weekly" => "weekly",
            _ => "off",
        },
        retention: cfg.backup_retention_count,
        free_space: free.map(backup::human_bytes),
    }
}

/// Name for a downloaded archive. A sanitized support share gets a
/// `-sanitized` marker so it can't be mistaken for the key-bearing kind
/// sitting next to it in a Downloads folder. Downloads never go
/// through `parse_backup_name`, so the marker costs nothing.
fn download_file_name(sanitize: bool, timestamp: i64) -> String {
    if sanitize {
        format!("ryokan-backup-{timestamp}-sanitized.tar.gz")
    } else {
        backup::backup_file_name(BackupKind::Scheduled, timestamp)
    }
}

/// Removes a temp dir when dropped: the download's work dir lives
/// exactly as long as its response body.
struct TempDirGuard(Option<PathBuf>);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The file's bytes in 64 KiB chunks, with `guard` riding along in the
/// stream state so its cleanup runs when the body is done.
///
/// Fused on purpose: a body layer may poll the stream once more after
/// it has ended (the compression middleware does, while it finishes
/// its encoder), and a bare `unfold` panics on that. The panic killed
/// the download mid-stream for every browser that advertised
/// `Accept-Encoding`, while curl, which does not, got the whole file.
fn file_stream(
    file: tokio::fs::File,
    guard: TempDirGuard,
) -> futures_util::stream::Fuse<
    impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
> {
    use futures_util::StreamExt;
    futures_util::stream::unfold((file, guard), |(mut file, guard)| async move {
        let mut buf = vec![0u8; 64 * 1024];
        match tokio::io::AsyncReadExt::read(&mut file, &mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<Bytes, std::io::Error>(Bytes::from(buf)), (file, guard)))
            }
            Err(e) => Some((Err(e), (file, guard))),
        }
    })
    .fuse()
}

/// Stream `path` as an attachment.
async fn file_response(
    path: &Path,
    download_name: &str,
    guard: TempDirGuard,
) -> Result<Response, String> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let size = file.metadata().await.ok().map(|m| m.len());
    let stream = file_stream(file, guard);
    let mut resp = Response::new(Body::from_stream(stream));
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/gzip"),
    );
    // Names come from `backup_file_name`: ASCII, no quotes.
    if let Ok(v) =
        header::HeaderValue::from_str(&format!("attachment; filename=\"{download_name}\""))
    {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Some(size) = size
        && let Ok(v) = header::HeaderValue::from_str(&size.to_string())
    {
        headers.insert(header::CONTENT_LENGTH, v);
    }
    Ok(resp)
}

fn json_error(status: StatusCode, message: String) -> Response {
    (
        status,
        Json(serde_json::json!({ "ok": false, "error": message })),
    )
        .into_response()
}

fn backup_error_response(e: BackupError) -> Response {
    let status = match e {
        BackupError::Busy => StatusCode::SERVICE_UNAVAILABLE,
        BackupError::NoSpace { .. } => StatusCode::INSUFFICIENT_STORAGE,
        BackupError::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    json_error(status, e.to_string())
}

fn flag(v: &Option<String>) -> bool {
    match v.as_deref().map(str::trim) {
        None => false,
        Some("") | Some("0") | Some("false") | Some("off") => false,
        Some(_) => true,
    }
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct DownloadQuery {
    #[serde(default)]
    include_artwork: Option<String>,
    #[serde(default)]
    sanitize: Option<String>,
}

/// `GET /api/backup/download?include_artwork=1&sanitize=1`. Builds the
/// archive in a temp dir under the data dir and streams it; 503 while
/// another backup runs, 507 when the disk-space precheck fails.
#[utoipa::path(
    get,
    path = "/api/backup/download",
    tag = "Backup",
    summary = "Download a backup",
    description = "Builds a fresh backup archive (database, encryption key, optional artwork) and streams it as a tar.gz. Cookie auth only: the archive carries the encryption key.",
    params(DownloadQuery),
    responses(
        (status = 200, description = "Backup archive (application/gzip)"),
        (status = 503, description = "Another backup is already running"),
        (status = 507, description = "Not enough free disk space"),
    ),
)]
pub async fn api_backup_download(
    State(state): State<AppState>,
    Query(q): Query<DownloadQuery>,
) -> Response {
    let opts = BackupOptions {
        include_artwork: flag(&q.include_artwork),
        sanitize: flag(&q.sanitize),
    };
    let paths = BackupPaths::from_env();
    let work = paths.data_dir.join(BACKUP_WORK_DIR_NAME).join(format!(
        "download-{}",
        hex::encode(rand::random::<[u8; 8]>())
    ));
    let guard = TempDirGuard(Some(work.clone()));
    let name = download_file_name(opts.sanitize, chrono::Utc::now().timestamp());
    let out = work.join(&name);
    let manifest = match backup::create_backup(&state.db, &paths, opts, &out).await {
        Ok(m) => m,
        Err(e) => {
            logger::warn(
                &state.db,
                LogCategory::System,
                "Backup download failed",
                &e.to_string(),
            )
            .await;
            return backup_error_response(e);
        }
    };
    logger::info(
        &state.db,
        LogCategory::System,
        "Backup downloaded",
        &format!(
            "{} ({}{}{})",
            name,
            backup::human_bytes(manifest.db_size_bytes),
            if manifest.includes_artwork {
                " + artwork"
            } else {
                ""
            },
            if manifest.sanitized {
                ", sanitized"
            } else {
                ""
            }
        ),
    )
    .await;
    match file_response(&out, &name, guard).await {
        Ok(resp) => resp,
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// One folder run with the scheduled-task bookkeeping the supervised
/// loop also does, so a manual run shows up in System → Scheduled
/// Tasks the same way.
async fn run_to_folder_tracked(state: &AppState) -> Result<backup::FolderRun, BackupError> {
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let paths = BackupPaths::from_env();
    let _ = scheduled_tasks::mark_started(&state.db, "backup", "Manual backup").await;
    match backup::run_to_folder(&state.db, &paths, &cfg).await {
        Ok(run) => {
            let detail = format!(
                "{} written to {}{}",
                run.file_name,
                run.dir.display(),
                if run.pruned.is_empty() {
                    String::new()
                } else {
                    format!(", pruned {}", run.pruned.join(", "))
                }
            );
            logger::info(&state.db, LogCategory::System, "Backup saved", &detail).await;
            let _ = scheduled_tasks::mark_finished(&state.db, "backup", "ok", &detail).await;
            Ok(run)
        }
        Err(e) => {
            let msg = e.to_string();
            logger::error(&state.db, LogCategory::System, "Backup failed", &msg).await;
            let _ = scheduled_tasks::mark_finished(&state.db, "backup", "error", &msg).await;
            Err(e)
        }
    }
}

/// `POST /api/backup/run` and `POST /api/tasks/backup`: JSON shape for
/// the Scheduled Tasks tab's Run now.
#[utoipa::path(
    post,
    path = "/api/backup/run",
    tag = "Backup",
    summary = "Save a backup to the backup folder",
    description = "Writes a backup archive into the configured backup folder and prunes old ones. Also mounted at POST /api/tasks/backup for the Scheduled Tasks tab.",
    responses(
        (status = 200, description = "Result envelope: ok, message, file", body = serde_json::Value),
        (status = 507, description = "Not enough free disk space"),
    ),
)]
pub async fn api_backup_run(State(state): State<AppState>) -> Response {
    match run_to_folder_tracked(&state).await {
        Ok(run) => Json(serde_json::json!({
            "ok": true,
            "message": format!("Backup saved as {} in {}.", run.file_name, run.dir.display()),
            "file": run.file_name,
        }))
        .into_response(),
        Err(e) => match e {
            BackupError::Busy => {
                Json(serde_json::json!({ "ok": false, "message": e.to_string() })).into_response()
            }
            other => backup_error_response(other),
        },
    }
}

/// `POST /system/backup/run`: the form button on the Backup tab.
pub async fn backup_run_form(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
) -> Response {
    let target = match run_to_folder_tracked(&state).await {
        Ok(run) => format!(
            "/system?tab=backup&message={}",
            urlencoding::encode(&format!("Backup saved as {}.", run.file_name))
        ),
        Err(e) => format!(
            "/system?tab=backup&error={}",
            urlencoding::encode(&e.to_string())
        ),
    };
    htmx_aware_redirect(is_htmx, &target)
}

fn named_backup(dir: &Path, name: &str) -> Option<PathBuf> {
    backup::parse_backup_name(name)?;
    let path = dir.join(name);
    path.is_file().then_some(path)
}

async fn configured_backup_dir(state: &AppState) -> PathBuf {
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    BackupPaths::from_env().backup_dir(&cfg.backup_directory)
}

/// `GET /api/backup/files/{name}`: download a backup from the folder.
/// The name must parse as one of ours, which is also the traversal
/// guard.
#[utoipa::path(
    get,
    path = "/api/backup/files/{name}",
    tag = "Backup",
    summary = "Download a saved backup",
    description = "Streams one archive from the backup folder. The name must match Ryokan's own backup naming, which also blocks path traversal.",
    params(("name" = String, Path, description = "Backup file name, e.g. ryokan-backup-20260901T120000Z.tar.gz")),
    responses(
        (status = 200, description = "Backup archive (application/gzip)"),
        (status = 404, description = "No such backup"),
    ),
)]
pub async fn api_backup_file(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Response {
    let dir = configured_backup_dir(&state).await;
    let Some(path) = named_backup(&dir, &name) else {
        return json_error(StatusCode::NOT_FOUND, "No such backup.".to_string());
    };
    match file_response(&path, &name, TempDirGuard(None)).await {
        Ok(resp) => resp,
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// `POST /api/backup/files/{name}/delete` (form): remove one backup.
#[utoipa::path(
    post,
    path = "/api/backup/files/{name}/delete",
    tag = "Backup",
    summary = "Delete a saved backup",
    description = "Removes one archive from the backup folder, then redirects back to System > Backup with a message.",
    params(("name" = String, Path, description = "Backup file name")),
    responses(
        (status = 303, description = "Redirect to System > Backup"),
    ),
)]
pub async fn backup_file_delete(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    AxumPath(name): AxumPath<String>,
) -> Response {
    let dir = configured_backup_dir(&state).await;
    let target = match named_backup(&dir, &name) {
        Some(path) => match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                logger::info(&state.db, LogCategory::System, "Backup deleted", &name).await;
                format!(
                    "/system?tab=backup&message={}",
                    urlencoding::encode(&format!("Deleted {name}."))
                )
            }
            Err(e) => format!(
                "/system?tab=backup&error={}",
                urlencoding::encode(&format!("Could not delete {name}: {e}"))
            ),
        },
        None => "/system?tab=backup&error=No+such+backup.".to_string(),
    };
    htmx_aware_redirect(is_htmx, &target)
}

/// `POST /api/restore/upload`: raw `application/gzip` body. Streams to
/// a temp file, then validates and stages through
/// `backup::stage_restore`. Nothing is applied until the next restart.
#[utoipa::path(
    post,
    path = "/api/restore/upload",
    tag = "Backup",
    summary = "Stage a restore",
    description = "Accepts a raw application/gzip backup archive, validates it, takes a pre-restore backup, and stages it. The restore is applied on the next restart.",
    request_body(content = String, content_type = "application/gzip", description = "Backup archive bytes"),
    responses(
        (status = 200, description = "Restore staged; restart to apply", body = serde_json::Value),
        (status = 400, description = "Archive rejected (bad manifest, version mismatch, integrity check failed)"),
        (status = 409, description = "A restore is already staged"),
        (status = 413, description = "Archive exceeds the upload limit"),
    ),
)]
pub async fn api_restore_upload(State(state): State<AppState>, body: Body) -> Response {
    let paths = BackupPaths::from_env();
    if paths.pending_dir().exists() {
        return json_error(StatusCode::CONFLICT, RestoreError::Pending.to_string());
    }
    let work = paths.data_dir.join(RESTORE_WORK_DIR_NAME);
    if let Err(e) = tokio::fs::create_dir_all(&work).await {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create {}: {e}", work.display()),
        );
    }
    let upload = work.join(format!(
        "upload-{}.tar.gz",
        hex::encode(rand::random::<[u8; 8]>())
    ));
    if let Err(e) = write_body_to_file(body, &upload).await {
        let _ = tokio::fs::remove_file(&upload).await;
        return json_error(StatusCode::BAD_REQUEST, e);
    }

    let dir = configured_backup_dir(&state).await;
    match backup::stage_restore(&state.db, &paths, &dir, &upload).await {
        Ok(staged) => {
            logger::info(
                &state.db,
                LogCategory::System,
                "Restore staged; restart to apply",
                &format!(
                    "backup from {} (Ryokan {}), pre-restore backup {}",
                    staged.manifest.timestamp_label(),
                    staged.manifest.ryokan_version,
                    staged.pre_restore_backup
                ),
            )
            .await;
            Json(serde_json::json!({
                "ok": true,
                "restart_required": true,
                "backup_time": staged.manifest.timestamp_label(),
                "version": staged.manifest.ryokan_version,
                "includes_key": staged.manifest.includes_key,
                "includes_artwork": staged.manifest.includes_artwork,
                "sanitized": staged.manifest.sanitized,
                "pre_restore_backup": staged.pre_restore_backup,
                "warnings": staged.warnings,
            }))
            .into_response()
        }
        Err(e) => {
            logger::warn(
                &state.db,
                LogCategory::System,
                "Restore upload rejected",
                &e.to_string(),
            )
            .await;
            let status = match e {
                RestoreError::Pending => StatusCode::CONFLICT,
                RestoreError::Invalid(_) => StatusCode::BAD_REQUEST,
                RestoreError::Incompatible(_) => StatusCode::UNPROCESSABLE_ENTITY,
                RestoreError::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            json_error(status, e.to_string())
        }
    }
}

async fn write_body_to_file(body: Body, path: &Path) -> Result<(), String> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    let mut stream = body.into_data_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("upload interrupted: {e}"))?;
        written += chunk.len() as u64;
        if written > MAX_UPLOAD_BYTES {
            return Err("The upload is larger than any Ryokan backup can be.".to_string());
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write upload: {e}"))?;
    }
    file.flush()
        .await
        .map_err(|e| format!("flush upload: {e}"))?;
    if written == 0 {
        return Err("The upload is empty.".to_string());
    }
    Ok(())
}

/// `POST /api/restore/cancel` (form): drop the staged restore.
#[utoipa::path(
    post,
    path = "/api/restore/cancel",
    tag = "Backup",
    summary = "Cancel a staged restore",
    description = "Removes the staged restore so the next restart changes nothing. Redirects back to System > Backup.",
    responses(
        (status = 303, description = "Redirect to System > Backup"),
    ),
)]
pub async fn restore_cancel(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
) -> Response {
    let paths = BackupPaths::from_env();
    let target = match backup::cancel_pending_restore(&paths) {
        Ok(true) => {
            logger::info(
                &state.db,
                LogCategory::System,
                "Staged restore cancelled",
                "",
            )
            .await;
            "/system?tab=backup&message=Restore+cancelled.+Nothing+was+changed.".to_string()
        }
        Ok(false) => "/system?tab=backup&message=No+restore+was+staged.".to_string(),
        Err(e) => format!("/system?tab=backup&error={}", urlencoding::encode(&e)),
    };
    htmx_aware_redirect(is_htmx, &target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitized_downloads_carry_a_marker_in_the_name() {
        assert_eq!(download_file_name(false, 5), "ryokan-backup-5.tar.gz");
        assert_eq!(
            download_file_name(true, 5),
            "ryokan-backup-5-sanitized.tar.gz"
        );
        assert!(backup::parse_backup_name(&download_file_name(true, 5)).is_none());
    }

    #[test]
    fn query_flags_read_checkbox_and_boolean_shapes() {
        assert!(!flag(&None));
        assert!(!flag(&Some(String::new())));
        assert!(!flag(&Some("0".to_string())));
        assert!(!flag(&Some("false".to_string())));
        assert!(flag(&Some("1".to_string())));
        assert!(flag(&Some("on".to_string())));
        assert!(flag(&Some("true".to_string())));
    }

    #[test]
    fn named_backup_only_resolves_files_this_module_produced() {
        let dir = std::env::temp_dir().join(format!("ryokan-named-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ryokan-backup-100.tar.gz"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        assert!(named_backup(&dir, "ryokan-backup-100.tar.gz").is_some());
        assert!(
            named_backup(&dir, "ryokan-backup-999.tar.gz").is_none(),
            "missing file"
        );
        assert!(
            named_backup(&dir, "notes.txt").is_none(),
            "not a backup name"
        );
        assert!(
            named_backup(&dir, "../ryokan-backup-100.tar.gz").is_none(),
            "traversal never parses as a backup name"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn body_writer_rejects_empty_uploads_and_keeps_bytes_otherwise() {
        let dir = std::env::temp_dir().join(format!("ryokan-body-writer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty.bin");
        let err = write_body_to_file(Body::empty(), &empty).await.unwrap_err();
        assert!(err.contains("empty"), "{err}");
        let full = dir.join("full.bin");
        write_body_to_file(Body::from("gzip bytes"), &full)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&full).unwrap(), b"gzip bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The download body is polled once more after it ends by the
    /// compression layer; an unfused `unfold` panics there and the
    /// browser sees a failed download.
    #[tokio::test]
    async fn file_stream_yields_none_again_after_the_end() {
        use futures_util::StreamExt;
        let dir = std::env::temp_dir().join(format!("ryokan-file-stream-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("payload.bin");
        std::fs::write(&path, vec![7u8; 70 * 1024]).unwrap();
        let file = tokio::fs::File::open(&path).await.unwrap();
        let mut stream = std::pin::pin!(file_stream(file, TempDirGuard(None)));
        let mut total = 0usize;
        while let Some(chunk) = stream.next().await {
            total += chunk.unwrap().len();
        }
        assert_eq!(total, 70 * 1024);
        assert!(
            stream.next().await.is_none(),
            "a second poll after the end must be None"
        );
        assert!(stream.next().await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
