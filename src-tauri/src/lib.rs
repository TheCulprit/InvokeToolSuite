use base64::Engine;
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use std::{
    collections::HashMap,
    fs,
    io::Cursor,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tauri::{Emitter, Manager, State};

mod civitai;
mod invoke_api;

#[derive(Serialize)]
struct InvokeConnectionInfo {
    connected: bool,
    multiuser_enabled: Option<bool>,
}

fn invoke_api_token_entry(server_url: &str) -> Result<keyring::Entry, String> {
    let normalized_url = server_url.trim().trim_end_matches('/');
    let digest = ring::digest::digest(&ring::digest::SHA256, normalized_url.as_bytes());
    let account = format!("invoke-api-{}", &hex::encode(digest)[..16]);
    keyring::Entry::new("invoke-tool-suite", &account)
        .map_err(|error| format!("Could not access the system credential store: {error}"))
}

fn saved_invoke_api_token(server_url: &str) -> Result<Option<String>, String> {
    match invoke_api_token_entry(server_url)?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(format!("Could not read the saved Invoke login: {error}")),
    }
}

fn make_invoke_api(server_url: &str, token: Option<&str>) -> Result<invoke_api::InvokeApiClient, String> {
    let saved = if token.is_none() { saved_invoke_api_token(server_url)? } else { None };
    invoke_api::InvokeApiClient::new(server_url, token.or(saved.as_deref()))
}

#[tauri::command]
async fn login_invoke_api(server_url: String, email: String, password: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let client = invoke_api::InvokeApiClient::new(&server_url, None)?;
        let token = client.login(email.trim(), &password)?;
        invoke_api_token_entry(&server_url)?.set_password(&token)
            .map_err(|error| format!("Could not save the Invoke login in the system credential store: {error}"))
    }).await.map_err(|error| format!("Invoke login failed: {error}"))?
}

#[tauri::command]
fn clear_invoke_api_login(server_url: String) -> Result<(), String> {
    match invoke_api_token_entry(&server_url)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!("Could not remove the saved Invoke login: {error}")),
    }
}

#[tauri::command]
async fn test_invoke_api(server_url: String, token: Option<String>) -> Result<InvokeConnectionInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let client = make_invoke_api(&server_url, token.as_deref())?;
        let status = client.test_connection()?;
        Ok(InvokeConnectionInfo {
            connected: true,
            multiuser_enabled: status.get("multiuser_enabled").and_then(serde_json::Value::as_bool),
        })
    }).await.map_err(|error| format!("Invoke API connection check failed: {error}"))?
}

#[tauri::command]
async fn api_list_destination_projects(server_url: String, token: Option<String>) -> Result<Vec<invoke_api::ProjectSummary>, String> {
    tauri::async_runtime::spawn_blocking(move || make_invoke_api(&server_url, token.as_deref())?.projects())
        .await.map_err(|error| format!("Could not load Invoke projects: {error}"))?
}

#[tauri::command]
async fn api_list_destination_boards(server_url: String, token: Option<String>, project_id: String) -> Result<Vec<invoke_api::BoardSummary>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let client = make_invoke_api(&server_url, token.as_deref())?;
        let project = client.projects()?.into_iter().find(|project| project.project_id == project_id)
            .ok_or_else(|| "The selected Invoke project no longer exists.".to_owned())?;
        let boards = client.boards()?;
        Ok(boards.into_iter().filter(|board| {
            board.project_id.as_deref() == Some(project_id.as_str()) || board.board_id == project.board_id
        }).collect())
    }).await.map_err(|error| format!("Could not load Invoke boards: {error}"))?
}

#[derive(Default)]
struct PreviewTasks(Mutex<HashMap<String, Arc<AtomicBool>>>);

#[derive(Serialize)]
struct SourceSummary {
    path: String,
    board_count: usize,
    image_count: usize,
}

#[derive(Serialize)]
struct SourceBoard {
    board_id: String,
    board_name: String,
    image_count: usize,
}

#[derive(Serialize, Clone)]
struct SourceImage {
    image_name: String,
    image_subfolder: String,
    created_at: String,
    width: i64,
    height: i64,
    preview: Option<String>,
}

#[derive(Serialize, Clone)]
struct SourcePreviewProgress {
    request_id: String,
    current: usize,
    total: usize,
    image_name: String,
}

#[derive(Serialize, Clone)]
struct SourcePreviewImage {
    request_id: String,
    image: SourceImage,
}

#[derive(Serialize)]
struct TransferIssue {
    image_name: String,
    reason: String,
}

#[derive(Serialize)]
struct ApiTransferReport {
    imported: usize,
    skipped: usize,
    issues: Vec<TransferIssue>,
    dates_note: String,
}

#[derive(Serialize, Clone)]
struct TransferProgress {
    phase: String,
    current: usize,
    total: usize,
    imported: usize,
    skipped: usize,
    image_name: String,
}

pub(crate) fn set_taskbar_indeterminate(window: &tauri::Window) {
    let _ = window.set_progress_bar(tauri::window::ProgressBarState {
        status: Some(tauri::window::ProgressBarStatus::Indeterminate),
        progress: None,
    });
}

pub(crate) fn task_progress_percent(current: usize, total: usize) -> Option<u64> {
    (total > 0).then(|| (current.saturating_mul(100) / total).min(100) as u64)
}

pub(crate) fn set_taskbar_progress(window: &tauri::Window, current: usize, total: usize) {
    set_taskbar_percent(window, task_progress_percent(current, total));
}

pub(crate) fn set_taskbar_percent(window: &tauri::Window, progress: Option<u64>) {
    let _ = window.set_progress_bar(tauri::window::ProgressBarState {
        status: Some(if progress.is_some() {
            tauri::window::ProgressBarStatus::Normal
        } else {
            tauri::window::ProgressBarStatus::Indeterminate
        }),
        progress,
    });
}

pub(crate) fn clear_taskbar_progress(window: &tauri::Window) {
    let _ = window.set_progress_bar(tauri::window::ProgressBarState {
        status: Some(tauri::window::ProgressBarStatus::None),
        progress: None,
    });
}

fn images_root(path: &str) -> PathBuf {
    let root = Path::new(path);
    if root
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("images"))
    {
        return root.to_path_buf();
    }
    let child = root.join("images");
    if child.is_dir() {
        child
    } else {
        root.to_path_buf()
    }
}

fn safe_subfolder(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("Image has an invalid subfolder path.".into());
    }
    Ok(path.to_path_buf())
}

fn safe_file_name(value: &str) -> Result<(), String> {
    let path = Path::new(value);
    if path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
        || path.file_name().and_then(|name| name.to_str()) != Some(value)
    {
        return Err("Image has an invalid file name.".into());
    }
    Ok(())
}

fn make_thumbnail(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if let Ok(image) = image::load_from_memory(&bytes) {
        let thumbnail = image.thumbnail(360, 280);
        let mut cursor = Cursor::new(Vec::new());
        thumbnail
            .write_to(&mut cursor, image::ImageFormat::Jpeg)
            .ok()?;
        Some(format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(cursor.into_inner())
        ))
    } else {
        None
    }
}

fn has_column(db: &Connection, table: &str, column: &str) -> bool {
    let Ok(mut stmt) = db.prepare(&format!("PRAGMA table_info({table})")) else {
        return false;
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) else {
        return false;
    };
    let found = rows.flatten().any(|name| name == column);
    found
}

#[tauri::command]
fn inspect_database(path: String) -> Result<SourceSummary, String> {
    let p = Path::new(&path);
    if !p.is_file() {
        return Err("Choose a database file that exists.".into());
    }
    let db = Connection::open_with_flags(
        p,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("Could not open database: {e}"))?;
    let has_images: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='images')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if !has_images {
        return Err("This database does not contain an InvokeAI images table.".into());
    }
    let image_count: usize = db
        .query_row(
            "SELECT COUNT(*) FROM images WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    let board_count: usize = db
        .query_row("SELECT COUNT(*) FROM boards", [], |r| r.get(0))
        .unwrap_or(0);
    Ok(SourceSummary {
        path,
        board_count,
        image_count,
    })
}

#[tauri::command]
async fn list_source_boards(path: String) -> Result<Vec<SourceBoard>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| e.to_string())?;
        let board_active = if has_column(&db, "boards", "deleted_at") { "WHERE b.deleted_at IS NULL" } else { "" };
        let membership_active = if has_column(&db, "board_images", "deleted_at") { "AND bi.deleted_at IS NULL" } else { "" };
        let sql = format!("SELECT b.board_id, b.board_name, COUNT(i.image_name) FROM boards b LEFT JOIN board_images bi ON bi.board_id=b.board_id {membership_active} LEFT JOIN images i ON i.image_name=bi.image_name AND i.deleted_at IS NULL AND COALESCE(i.is_intermediate, 0)=0 AND i.image_category='general' {board_active} GROUP BY b.board_id, b.board_name ORDER BY b.board_name COLLATE NOCASE");
        let mut stmt = db.prepare(&sql).map_err(|e| format!("Could not read boards: {e}"))?;
        let rows = stmt.query_map([], |row| Ok(SourceBoard { board_id: row.get(0)?, board_name: row.get(1)?, image_count: row.get(2)? })).map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }).await.map_err(|error| format!("Could not load source boards: {error}"))?
}

#[tauri::command]
async fn list_board_images(
    window: tauri::Window,
    tasks: State<'_, PreviewTasks>,
    database: String,
    outputs: String,
    board_id: String,
    request_id: String,
) -> Result<(), String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    tasks
        .0
        .lock()
        .map_err(|_| "Preview task registry is unavailable.".to_owned())?
        .insert(request_id.clone(), cancelled.clone());
    let task_id = request_id.clone();
    let task_result = tauri::async_runtime::spawn_blocking(move || {
        let db = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| e.to_string())?;
        let membership_active = if has_column(&db, "board_images", "deleted_at") { "AND bi.deleted_at IS NULL" } else { "" };
        let selection = format!("FROM board_images bi JOIN images i ON i.image_name=bi.image_name WHERE bi.board_id=?1 {membership_active} AND i.deleted_at IS NULL AND COALESCE(i.is_intermediate, 0)=0 AND i.image_category='general'");
        let total: usize = db.query_row(&format!("SELECT COUNT(*) {selection}"), params![board_id], |row| row.get(0)).map_err(|e| e.to_string())?;
        let sql = format!("SELECT i.image_name, i.created_at, i.width, i.height, i.image_subfolder {selection} ORDER BY i.created_at");
        let mut stmt = db.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt.query_map(params![board_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, String>(4)?))).map_err(|e| e.to_string())?;
        for (index, row) in rows.enumerate() {
            if cancelled.load(Ordering::Relaxed) { break; }
            let (name, created_at, width, height, subfolder) = row.map_err(|e| e.to_string())?;
            let preview = if safe_file_name(&name).is_ok() {
                make_thumbnail(&images_root(&outputs).join(safe_subfolder(&subfolder)?).join(&name))
            } else { None };
            let image = SourceImage { image_name: name.clone(), image_subfolder: subfolder, created_at, width, height, preview };
            let _ = window.emit("source-preview-image", SourcePreviewImage { request_id: task_id.clone(), image });
            let _ = window.emit("source-preview-progress", SourcePreviewProgress { request_id: task_id.clone(), current: index + 1, total, image_name: name });
        }
        Ok(())
    }).await.map_err(|error| format!("Could not load board preview: {error}"))?;
    tasks
        .0
        .lock()
        .map_err(|_| "Preview task registry is unavailable.".to_owned())?
        .remove(&request_id);
    task_result
}

#[tauri::command]
async fn load_source_image(
    outputs: String,
    image_name: String,
    image_subfolder: String,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        safe_file_name(&image_name)?;
        let relative = safe_subfolder(&image_subfolder)?;
        let path = images_root(&outputs).join(relative).join(&image_name);
        let bytes = fs::read(&path).map_err(|e| format!("Could not read the source image: {e}"))?;
        let mime = match image::guess_format(&bytes)
            .map_err(|e| format!("Could not identify the source image format: {e}"))?
        {
            image::ImageFormat::Png => "image/png",
            image::ImageFormat::Jpeg => "image/jpeg",
            image::ImageFormat::WebP => "image/webp",
            _ => return Err("This image format cannot be shown in the preview.".into()),
        };
        Ok(format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    })
    .await
    .map_err(|error| format!("Could not load source image: {error}"))?
}

#[tauri::command]
fn cancel_source_preview(tasks: State<'_, PreviewTasks>, request_id: String) {
    if let Ok(registry) = tasks.0.lock() {
        if let Some(cancelled) = registry.get(&request_id) {
            cancelled.store(true, Ordering::Relaxed);
        }
    }
}

#[tauri::command]
async fn transfer_board_via_api(
    window: tauri::Window,
    source_db: String,
    source_outputs: String,
    source_board_id: String,
    server_url: String,
    api_token: Option<String>,
    destination_project_id: Option<String>,
    destination_board_id: Option<String>,
) -> Result<ApiTransferReport, String> {
    let taskbar_window = window.clone();
    set_taskbar_indeterminate(&window);
    let result = tauri::async_runtime::spawn_blocking(move || {
        let source = Connection::open_with_flags(
            source_db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ).map_err(|error| format!("Could not read the source archive: {error}"))?;
        let membership_active = if has_column(&source, "board_images", "deleted_at") {
            "AND bi.deleted_at IS NULL"
        } else { "" };
        let sql = format!("SELECT i.image_name,i.image_category,i.metadata,i.image_subfolder,i.created_at FROM board_images bi JOIN images i ON i.image_name=bi.image_name WHERE bi.board_id=?1 {membership_active} AND i.deleted_at IS NULL AND COALESCE(i.is_intermediate,0)=0 AND i.image_category='general' ORDER BY i.created_at ASC,i.image_name ASC");
        let mut statement = source.prepare(&sql).map_err(|error| error.to_string())?;
        let records = statement.query_map(params![source_board_id], |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?, row.get::<_, String>(4)?,
        ))).map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())?;
        drop(statement);
        let client = make_invoke_api(&server_url, api_token.as_deref())?;
        let mut existing = client.image_names(destination_board_id.as_deref())?
            .into_iter().collect::<std::collections::HashSet<_>>();
        let total = records.len();
        let mut report = ApiTransferReport {
            imported: 0,
            skipped: 0,
            issues: Vec::new(),
            dates_note: "Images were uploaded oldest first. Their order is preserved, but Invoke assigns them new transfer-time dates.".into(),
        };
        for (index, (name, category, metadata, subfolder, _created_at)) in records.into_iter().enumerate() {
            let mut reason = None;
            if safe_file_name(&name).is_err() {
                reason = Some("The source image has an invalid filename.".to_owned());
            }
            let relative = match safe_subfolder(&subfolder) {
                Ok(path) => path,
                Err(error) => { reason = Some(error); PathBuf::new() }
            };
            let source_path = images_root(&source_outputs).join(relative).join(&name);
            if reason.is_none() && existing.contains(&name) {
                reason = Some("An image with this filename already exists in the destination.".into());
            }
            if reason.is_none() && !source_path.is_file() {
                reason = Some("The source image file is missing.".into());
            }
            if let Some(reason) = reason {
                report.skipped += 1;
                report.issues.push(TransferIssue { image_name: name.clone(), reason });
            } else {
                let metadata = metadata.filter(|value| {
                    serde_json::from_str::<serde_json::Value>(value).is_ok_and(|parsed| parsed.is_object())
                });
                match client.upload_image(
                    &source_path,
                    &category,
                    destination_board_id.as_deref(),
                    destination_project_id.as_deref(),
                    metadata.as_deref(),
                ) {
                    Ok(response) => {
                        let uploaded_name = response.get("image_name").and_then(serde_json::Value::as_str).unwrap_or(&name).to_owned();
                        existing.insert(name.clone());
                        existing.insert(uploaded_name);
                        report.imported += 1;
                    }
                    Err(error) => {
                        report.skipped += 1;
                        report.issues.push(TransferIssue { image_name: name.clone(), reason: error });
                    }
                }
            }
            let _ = window.emit("transfer-progress", TransferProgress {
                phase: "upload".into(), current: index + 1, total,
                imported: report.imported, skipped: report.skipped, image_name: name,
            });
            set_taskbar_progress(&window, index + 1, total);
        }
        Ok(report)
    }).await;
    clear_taskbar_progress(&taskbar_window);
    result.map_err(|error| format!("Image transfer worker failed: {error}"))?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(PreviewTasks::default())
        .manage(civitai::CivitaiTasks::default())
        .setup(|app| {
            if let (Some(icon), Some(window)) = (
                app.default_window_icon().cloned(),
                app.get_webview_window("main"),
            ) {
                window.set_icon(icon)?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            inspect_database,
            list_source_boards,
            list_board_images,
            load_source_image,
            cancel_source_preview,
            test_invoke_api,
            login_invoke_api,
            clear_invoke_api_login,
            api_list_destination_projects,
            api_list_destination_boards,
            transfer_board_via_api,
            civitai::enrich_civitai_models,
            civitai::get_civitai_cache_info,
            civitai::clear_civitai_cache,
            civitai::cancel_civitai_enrichment,
            civitai::civitai_api_key_saved,
            civitai::save_civitai_api_key,
            civitai::clear_civitai_api_key,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Invoke Tool Suite");
}
