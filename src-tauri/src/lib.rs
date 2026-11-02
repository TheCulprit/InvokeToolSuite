use base64::Engine;
use rusqlite::{
    backup::{Backup, StepResult},
    params, Connection, OpenFlags,
};
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
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{Emitter, Manager, State};

mod civitai;

#[derive(Default)]
struct PreviewTasks(Mutex<HashMap<String, Arc<AtomicBool>>>);

#[derive(Default)]
struct SafetyBackups(Arc<Mutex<HashMap<PathBuf, PathBuf>>>);

const DEFAULT_BACKUP_RETENTION: usize = 3;
const MAX_BACKUP_RETENTION: usize = 20;

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

fn backup_retention_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let current_directory = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("Could not locate the app settings folder: {error}"))?;
    let current_file = current_directory.join("backup-retention.txt");
    if let Some(parent) = current_directory.parent() {
        let legacy_directory = parent.join("com.invoke.transfer");
        let legacy_file = legacy_directory.join("backup-retention.txt");
        if legacy_file.is_file() {
            if !current_file.exists() {
                fs::create_dir_all(&current_directory).map_err(|error| {
                    format!("Could not create the app settings folder: {error}")
                })?;
                fs::rename(&legacy_file, &current_file)
                    .map_err(|error| format!("Could not move the saved backup setting: {error}"))?;
            } else {
                fs::remove_file(&legacy_file)
                    .map_err(|error| format!("Could not remove the old backup setting: {error}"))?;
            }
            let _ = fs::remove_dir(&legacy_directory);
        }
    }
    Ok(current_file)
}

fn backup_retention(app: &tauri::AppHandle) -> Result<usize, String> {
    let path = backup_retention_path(app)?;
    let retention = fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| (1..=MAX_BACKUP_RETENTION).contains(value))
        .unwrap_or(DEFAULT_BACKUP_RETENTION);
    Ok(retention)
}

#[tauri::command]
fn get_backup_retention(app: tauri::AppHandle, database: Option<String>) -> Result<usize, String> {
    let keep = backup_retention(&app)?;
    if let Some(database) = database.filter(|path| Path::new(path).is_file()) {
        prune_safety_backups(Path::new(&database), keep)?;
    }
    Ok(keep)
}

#[tauri::command]
fn set_backup_retention(
    app: tauri::AppHandle,
    keep: usize,
    database: Option<String>,
) -> Result<(), String> {
    if !(1..=MAX_BACKUP_RETENTION).contains(&keep) {
        return Err(format!(
            "Choose between 1 and {MAX_BACKUP_RETENTION} backups to keep."
        ));
    }
    let path = backup_retention_path(&app)?;
    let directory = path
        .parent()
        .ok_or("Could not locate the app settings folder.")?;
    fs::create_dir_all(directory)
        .map_err(|error| format!("Could not create the app settings folder: {error}"))?;
    fs::write(path, keep.to_string())
        .map_err(|error| format!("Could not save the backup setting: {error}"))?;
    if let Some(database) = database.filter(|path| Path::new(path).is_file()) {
        prune_safety_backups(Path::new(&database), keep)?;
    }
    Ok(())
}

fn prune_safety_backups(database_path: &Path, keep: usize) -> Result<(), String> {
    let directory = database_path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = database_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("invokeai");
    let prefix = format!("{stem}-before-");
    let mut backups = fs::read_dir(directory)
        .map_err(|error| format!("Could not list safety backups: {error}"))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            let is_app_backup = name.starts_with(&prefix)
                && (name.contains("-transfer-")
                    || name.contains("-civitai-")
                    || name.contains("-invoke-tool-suite-")
                    || name.contains("-invoke-tools-"));
            if !is_app_backup || path.extension().and_then(|ext| ext.to_str()) != Some("db") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((path, modified))
        })
        .collect::<Vec<_>>();
    backups.sort_by(|left, right| right.1.cmp(&left.1));
    for (index, (path, _)) in backups.into_iter().enumerate() {
        if index >= keep {
            fs::remove_file(&path)
                .map_err(|error| format!("Could not remove an old safety backup: {error}"))?;
            continue;
        }
        let Some(file_stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let old_markers = [
            "-before-transfer-",
            "-before-civitai-",
            "-before-invoke-tools-",
        ];
        let Some((marker, stamp)) = old_markers.iter().find_map(|marker| {
            file_stem
                .split_once(marker)
                .map(|(_, stamp)| (*marker, stamp))
        }) else {
            continue;
        };
        let Some((database_stem, _)) = file_stem.split_once(marker) else {
            continue;
        };
        let migrated = path.with_file_name(format!(
            "{database_stem}-before-invoke-tool-suite-{stamp}.db"
        ));
        if migrated == path {
            continue;
        }
        if migrated.exists() {
            fs::remove_file(&path).map_err(|error| {
                format!("Could not remove a duplicate old safety backup: {error}")
            })?;
        } else {
            fs::rename(&path, &migrated)
                .map_err(|error| format!("Could not rename an old safety backup: {error}"))?;
        }
    }
    Ok(())
}

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
struct DestinationProject {
    project_id: String,
    name: String,
    board_id: String,
    user_id: String,
}

#[derive(Serialize)]
struct DestinationBoard {
    board_id: String,
    board_name: String,
    project_id: Option<String>,
}

#[derive(Serialize)]
struct DestinationUser {
    user_id: String,
}

#[derive(Serialize)]
struct TransferIssue {
    image_name: String,
    reason: String,
}

#[derive(Serialize)]
struct TransferReport {
    imported: usize,
    skipped: usize,
    thumbnails_repaired: usize,
    issues: Vec<TransferIssue>,
    backup_path: String,
    backup_created: bool,
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

fn thumbnail_path(
    images_root: &Path,
    subfolder: &Path,
    image_name: &str,
) -> Result<PathBuf, String> {
    safe_file_name(image_name)?;
    let stem = Path::new(image_name)
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "Image has an invalid file name.".to_owned())?;
    Ok(images_root
        .join("thumbnails")
        .join(subfolder)
        .join(format!("{stem}.webp")))
}

fn write_invoke_thumbnail(source: &Path, destination: &Path) -> Result<(), String> {
    let image =
        image::open(source).map_err(|e| format!("Could not read image for thumbnail: {e}"))?;
    let thumbnail = image.thumbnail(256, 256);
    fs::create_dir_all(destination.parent().unwrap_or(destination))
        .map_err(|e| format!("Could not create thumbnail folder: {e}"))?;
    thumbnail
        .save_with_format(destination, image::ImageFormat::WebP)
        .map_err(|e| {
            let _ = fs::remove_file(destination);
            format!("Could not create v7 thumbnail: {e}")
        })
}

fn is_valid_thumbnail(path: &Path) -> bool {
    image::open(path).is_ok()
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
fn list_destination_projects(path: String) -> Result<Vec<DestinationProject>, String> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    let mut stmt = db
        .prepare(
            "SELECT project_id, name, board_id, user_id FROM projects ORDER BY name COLLATE NOCASE",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DestinationProject {
                project_id: row.get(0)?,
                name: row.get(1)?,
                board_id: row.get(2)?,
                user_id: row.get(3)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_destination_users(path: String) -> Result<Vec<DestinationUser>, String> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    let mut stmt = db
        .prepare("SELECT user_id FROM users ORDER BY user_id COLLATE NOCASE")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DestinationUser {
                user_id: row.get(0)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_destination_boards(
    path: String,
    project_id: String,
) -> Result<Vec<DestinationBoard>, String> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    let mut stmt = db.prepare("SELECT b.board_id, CASE WHEN b.board_id=p.board_id THEN 'Inbox' ELSE b.board_name END, b.project_id FROM boards b JOIN projects p ON p.project_id=?1 WHERE b.deleted_at IS NULL AND (b.project_id=?1 OR b.board_id=p.board_id) ORDER BY CASE WHEN b.board_id=p.board_id THEN 0 ELSE 1 END, b.board_name COLLATE NOCASE").map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![project_id], |row| {
            Ok(DestinationBoard {
                board_id: row.get(0)?,
                board_name: row.get(1)?,
                project_id: row.get(2)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn transfer_board(
    window: tauri::Window,
    backups: State<'_, SafetyBackups>,
    source_db: String,
    source_outputs: String,
    source_board_id: String,
    destination_db: String,
    destination_outputs: String,
    destination_project_id: Option<String>,
    destination_board_id: Option<String>,
    destination_user_id: Option<String>,
) -> Result<TransferReport, String> {
    let backups = backups.0.clone();
    set_taskbar_indeterminate(&window);
    let taskbar_window = window.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
    let source = Connection::open_with_flags(source_db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| e.to_string())?;
    let mut destination = Connection::open(&destination_db).map_err(|e| format!("Could not open the v7 database for writing: {e}"))?;
    let destination_path = Path::new(&destination_db);
    let destination_key = destination_path.canonicalize().unwrap_or_else(|_| destination_path.to_path_buf());
    let existing_backup = backups.lock().map_err(|_| "Safety backup registry is unavailable.".to_owned())?
        .get(&destination_key).filter(|path| path.is_file()).cloned();
    let (backup_path, backup_created) = if let Some(path) = existing_backup {
        let retention = backup_retention(window.app_handle())?;
        prune_safety_backups(destination_path, retention)?;
        let _ = window.emit("transfer-progress", TransferProgress { phase: "backup-reused".into(), current: 1, total: 1, imported: 0, skipped: 0, image_name: String::new() });
        set_taskbar_progress(&window, 1, 1);
        (path, false)
    } else {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let path = destination_path.with_file_name(format!("{}-before-invoke-tool-suite-{stamp}.db", destination_path.file_stem().and_then(|s| s.to_str()).unwrap_or("invokeai")));
        let mut backup_db = Connection::open(&path).map_err(|e| format!("Could not create the safety backup: {e}"))?;
        let backup = Backup::new(&destination, &mut backup_db).map_err(|e| format!("Could not start the safety backup: {e}"))?;
        let mut last_backup_percent = usize::MAX;
        loop {
            let step = backup.step(32).map_err(|e| format!("Could not complete the safety backup: {e}"))?;
            let status = backup.progress();
            let current = status.pagecount.saturating_sub(status.remaining) as usize;
            let total = status.pagecount as usize;
            let percent = if total == 0 { 0 } else { current.saturating_mul(100) / total };
            if percent != last_backup_percent || step == StepResult::Done {
                let _ = window.emit("transfer-progress", TransferProgress { phase: "backup".into(), current, total, imported: 0, skipped: 0, image_name: String::new() });
                set_taskbar_progress(&window, current, total);
                last_backup_percent = percent;
            }
            match step {
                StepResult::Done => break,
                StepResult::More => {},
                StepResult::Busy | StepResult::Locked => std::thread::sleep(std::time::Duration::from_millis(20)),
                _ => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        drop(backup);
        let retention = backup_retention(window.app_handle())?;
        prune_safety_backups(destination_path, retention)?;
        backups.lock().map_err(|_| "Safety backup registry is unavailable.".to_owned())?.insert(destination_key, path.clone());
        (path, true)
    };
    destination.pragma_update(None, "foreign_keys", "ON").map_err(|e| e.to_string())?;
    let target_user: String = if let Some(project_id) = &destination_project_id {
        destination.query_row("SELECT user_id FROM projects WHERE project_id=?1", params![project_id], |row| row.get(0)).map_err(|_| "The selected destination project no longer exists.".to_owned())?
    } else { destination_user_id.unwrap_or_else(|| "system".into()) };
    let valid_user: bool = destination.query_row("SELECT EXISTS(SELECT 1 FROM users WHERE user_id=?1)", params![target_user], |row| row.get(0)).map_err(|e| e.to_string())?;
    if !valid_user { return Err("Choose a valid destination account for Uncategorized images.".into()); }
    if let Some(board_id) = &destination_board_id {
        let belongs_to_project: bool = destination.query_row("SELECT EXISTS(SELECT 1 FROM boards WHERE board_id=?1 AND deleted_at IS NULL AND (project_id=?2 OR board_id=(SELECT board_id FROM projects WHERE project_id=?2)))", params![board_id, destination_project_id], |row| row.get(0)).map_err(|e| e.to_string())?;
        if !belongs_to_project { return Err("The selected board is not part of the selected project.".into()); }
    }
    let membership_active = if has_column(&source, "board_images", "deleted_at") { "AND bi.deleted_at IS NULL" } else { "" };
    let selection = format!("FROM board_images bi JOIN images i ON i.image_name=bi.image_name WHERE bi.board_id=?1 {membership_active} AND i.deleted_at IS NULL AND COALESCE(i.is_intermediate, 0)=0 AND i.image_category='general'");
    let mut statement = source.prepare(&format!("SELECT i.image_name, i.image_origin, i.image_category, i.width, i.height, i.session_id, i.node_id, i.metadata, i.created_at, i.updated_at, i.starred, i.has_workflow, i.image_subfolder {selection} ORDER BY i.created_at")).map_err(|e| e.to_string())?;
    let total: usize = source.query_row(&format!("SELECT COUNT(*) {selection}"), params![source_board_id], |row| row.get(0)).map_err(|e| e.to_string())?;
    let records = statement.query_map(params![source_board_id], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,i64>(3)?,row.get::<_,i64>(4)?,row.get::<_,Option<String>>(5)?,row.get::<_,Option<String>>(6)?,row.get::<_,Option<String>>(7)?,row.get::<_,String>(8)?,row.get::<_,String>(9)?,row.get::<_,Option<bool>>(10)?.unwrap_or(false),row.get::<_,Option<bool>>(11)?.unwrap_or(false),row.get::<_,String>(12)?))).map_err(|e| e.to_string())?;
    let destination_images = images_root(&destination_outputs);
    fs::create_dir_all(&destination_images).map_err(|e| format!("Could not access the v7 images folder: {e}"))?;
    let mut report = TransferReport { imported: 0, skipped: 0, thumbnails_repaired: 0, issues: Vec::new(), backup_path: backup_path.to_string_lossy().into_owned(), backup_created };
    let mut current = 0;
    for result in records {
        current += 1;
        let mut current_name = "(database row)".to_owned();
        let mut copied_path: Option<PathBuf> = None;
        let mut copied_thumbnail_path: Option<PathBuf> = None;
        let outcome = (|| -> Result<(), String> {
            let (name, origin, category, width, height, _session_id, _node_id, metadata, created_at, _updated_at, starred, has_workflow, subfolder) = result.map_err(|e| e.to_string())?;
            current_name = name.clone();
            safe_file_name(&name)?;
            let relative = safe_subfolder(&subfolder)?;
            let src = images_root(&source_outputs).join(&relative).join(&name);
            let dest = destination_images.join(&relative).join(&name);
            let dest_thumbnail = thumbnail_path(&destination_images, &relative, &name)?;
            let already_exists: bool = destination.query_row("SELECT EXISTS(SELECT 1 FROM images WHERE image_name=?1)", params![name], |row| row.get(0)).unwrap_or(false);
            if already_exists || dest.exists() {
                if already_exists && dest.is_file() && !is_valid_thumbnail(&dest_thumbnail) {
                    let _ = fs::remove_file(&dest_thumbnail);
                    write_invoke_thumbnail(&dest, &dest_thumbnail)?;
                    report.thumbnails_repaired += 1;
                }
                return Err("An image with this name already exists in the destination.".into());
            }
            if dest_thumbnail.exists() { return Err("A thumbnail with this name already exists in the destination.".into()); }
            if !src.is_file() { return Err("The source image file is missing.".into()); }
            fs::create_dir_all(dest.parent().unwrap_or(&destination_images)).map_err(|e| format!("Could not create image folder: {e}"))?;
            fs::copy(&src, &dest).map_err(|e| format!("Could not copy image: {e}"))?;
            copied_path = Some(dest.clone());
            copied_thumbnail_path = Some(dest_thumbnail.clone());
            write_invoke_thumbnail(&dest, &dest_thumbnail)?;
            let file_size = fs::metadata(&dest).map(|meta| meta.len() as i64).unwrap_or(0);
            let tx = destination.transaction().map_err(|e| e.to_string())?;
            tx.execute("INSERT INTO images (image_name,image_origin,image_category,width,height,session_id,node_id,metadata,is_intermediate,created_at,updated_at,deleted_at,starred,has_workflow,user_id,image_subfolder,project_id,file_size_bytes) VALUES (?1,?2,?3,?4,?5,NULL,NULL,?6,0,?7,STRFTIME('%Y-%m-%d %H:%M:%f','NOW'),NULL,?8,?9,?10,?11,?12,?13)", params![name,origin,category,width,height,metadata,created_at,starred,has_workflow,target_user,subfolder,destination_project_id,file_size]).map_err(|e| e.to_string())?;
            if let Some(board_id) = &destination_board_id { tx.execute("INSERT INTO board_images (board_id,image_name,created_at,updated_at,deleted_at) VALUES (?1,?2,?3,STRFTIME('%Y-%m-%d %H:%M:%f','NOW'),NULL)", params![board_id,name,created_at]).map_err(|e| e.to_string())?; }
            if let Err(error) = tx.commit() { return Err(error.to_string()); }
            Ok(())
        })();
        match outcome { Ok(())=>report.imported+=1, Err(reason)=>{if let Some(path)=copied_path { let _=fs::remove_file(path); } if let Some(path)=copied_thumbnail_path { let _=fs::remove_file(path); } report.skipped+=1;report.issues.push(TransferIssue{image_name:current_name.clone(),reason});} }
        let _ = window.emit("transfer-progress", TransferProgress { phase: "copy".into(), current, total, imported: report.imported, skipped: report.skipped, image_name: current_name });
        set_taskbar_progress(&window, current, total);
    }
    Ok(report)
    }).await;
    clear_taskbar_progress(&taskbar_window);
    result.map_err(|error| format!("Transfer worker failed: {error}"))?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(PreviewTasks::default())
        .manage(SafetyBackups::default())
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
            list_destination_projects,
            list_destination_users,
            list_destination_boards,
            transfer_board,
            civitai::enrich_civitai_models,
            civitai::get_civitai_cache_info,
            civitai::clear_civitai_cache,
            civitai::cancel_civitai_enrichment,
            civitai::civitai_api_key_saved,
            civitai::save_civitai_api_key,
            civitai::clear_civitai_api_key,
            get_backup_retention,
            set_backup_retention
        ])
        .run(tauri::generate_context!())
        .expect("error while running Invoke Tool Suite");
}
