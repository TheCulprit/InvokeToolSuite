use base64::Engine;
use rusqlite::{
    backup::{Backup, StepResult},
    params, Connection, OpenFlags,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use ring::digest::{Context, SHA256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{Emitter, Manager};

const MAX_PREVIEW_BYTES: u64 = 20 * 1024 * 1024;
const CIVITAI_API: &str = "https://civitai.com/api/v1/model-versions/by-hash/";
const CANCELLED_REASON: &str = "__invoke_tool_suite_cancelled__";
const KEYRING_SERVICE: &str = "invoke-tool-suite";
const LEGACY_KEYRING_SERVICE: &str = "invoke-tools";
const KEYRING_USER: &str = "civitai-api-key";

fn key_entry(service: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(service, KEYRING_USER)
        .map_err(|error| format!("Could not access the system credential store: {error}"))
}

fn civitai_key_entry() -> Result<keyring::Entry, String> {
    key_entry(KEYRING_SERVICE)
}

fn load_civitai_api_key() -> Result<Option<String>, String> {
    let current = civitai_key_entry()?;
    match current.get_password() {
        Ok(value) => {
            let legacy = key_entry(LEGACY_KEYRING_SERVICE)?;
            match legacy.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(Some(value)),
                Err(error) => Err(format!(
                    "Could not remove the old CivitAI key entry: {error}"
                )),
            }
        }
        Err(keyring::Error::NoEntry) => {
            let legacy = key_entry(LEGACY_KEYRING_SERVICE)?;
            match legacy.get_password() {
                Ok(value) => {
                    current.set_password(&value).map_err(|error| {
                        format!("Could not move the saved CivitAI key: {error}")
                    })?;
                    legacy.delete_credential().map_err(|error| {
                        format!("Could not remove the old CivitAI key entry: {error}")
                    })?;
                    Ok(Some(value))
                }
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(format!("Could not read the old CivitAI key: {error}")),
            }
        }
        Err(error) => Err(format!("Could not read the saved CivitAI key: {error}")),
    }
}

#[tauri::command]
pub(super) fn civitai_api_key_saved() -> Result<bool, String> {
    Ok(load_civitai_api_key()?.is_some())
}

#[tauri::command]
pub(super) fn save_civitai_api_key(api_key: String) -> Result<(), String> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err("Enter an API key before saving it.".into());
    }
    civitai_key_entry()?
        .set_password(api_key)
        .map_err(|error| {
            format!("Could not save the CivitAI key to the system credential store: {error}")
        })?;
    let legacy = key_entry(LEGACY_KEYRING_SERVICE)?;
    match legacy.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!(
            "Could not remove the old CivitAI key entry: {error}"
        )),
    }
}

#[tauri::command]
pub(super) fn clear_civitai_api_key() -> Result<(), String> {
    for service in [KEYRING_SERVICE, LEGACY_KEYRING_SERVICE] {
        match key_entry(service)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(error) => return Err(format!("Could not remove a saved CivitAI key: {error}")),
        }
    }
    Ok(())
}

#[derive(Default)]
pub(super) struct CivitaiTasks(Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>);

#[derive(Serialize, Clone)]
struct Progress {
    current: usize,
    total: usize,
    percent: Option<u64>,
    model_name: String,
    status: String,
}

#[derive(Serialize)]
pub(super) struct Issue {
    model_name: String,
    reason: String,
}

#[derive(Serialize)]
pub(super) struct Report {
    models_scanned: usize,
    models_processed: usize,
    cancelled: bool,
    cache_hits: usize,
    cached_not_found: usize,
    lookup_attempts: usize,
    not_found: usize,
    no_trigger_phrases: usize,
    no_preview_image: usize,
    trigger_phrases_added: usize,
    thumbnails_added: usize,
    already_had_triggers: usize,
    already_had_thumbnails: usize,
    skipped: usize,
    errors: usize,
    backup_path: String,
    backup_created: bool,
    issues: Vec<Issue>,
}

#[derive(Serialize)]
pub(super) struct CacheInfo {
    cache_path: String,
    cache_exists: bool,
    entries: usize,
    matched: usize,
    not_found: usize,
    read_error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct CachedMatch {
    fingerprint: String,
    status: String,
    #[serde(default)]
    trigger_phrases: Vec<String>,
    preview_url: Option<String>,
}

#[derive(Clone)]
struct Model {
    key: String,
    name: String,
    kind: String,
    path: PathBuf,
    needs_triggers: bool,
    needs_thumbnail: bool,
}

fn backup_database(db: &Connection, path: &Path, retention: usize) -> Result<PathBuf, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup_path = path.with_file_name(format!(
        "{}-before-invoke-tool-suite-{stamp}.db",
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("invokeai")
    ));
    let mut target = Connection::open(&backup_path)
        .map_err(|e| format!("Could not create the safety backup: {e}"))?;
    let backup = Backup::new(db, &mut target)
        .map_err(|e| format!("Could not start the safety backup: {e}"))?;
    loop {
        match backup
            .step(64)
            .map_err(|e| format!("Could not complete the safety backup: {e}"))?
        {
            StepResult::Done => break,
            StepResult::More => {}
            StepResult::Busy | StepResult::Locked => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            _ => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    drop(backup);
    super::prune_safety_backups(path, retention)?;
    Ok(backup_path)
}

fn has_content(value: Option<&Value>) -> bool {
    match value {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(values)) => values
            .iter()
            .any(|item| item.as_str().is_some_and(|s| !s.trim().is_empty())),
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

fn discover_models(
    db: &Connection,
    include_loras: bool,
    include_checkpoints: bool,
) -> Result<Vec<Model>, String> {
    let exists: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='models')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("Could not inspect the model database: {e}"))?;
    if !exists {
        return Err("This database does not contain InvokeAI's models table.".into());
    }
    let mut statement = db
        .prepare("SELECT id, config FROM models ORDER BY id")
        .map_err(|e| format!("Could not read model records: {e}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?;
    let mut models = Vec::new();
    for row in rows {
        let (id, config_text) = row.map_err(|e| e.to_string())?;
        let Ok(config) = serde_json::from_str::<Value>(&config_text) else {
            continue;
        };
        let model_type = config
            .get("type")
            .or_else(|| config.get("model_type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let model_format = config
            .get("format")
            .or_else(|| config.get("model_format"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let is_lora = model_type.eq_ignore_ascii_case("lora");
        let is_checkpoint = model_type.eq_ignore_ascii_case("checkpoint")
            || (model_type.eq_ignore_ascii_case("main")
                && (model_format.eq_ignore_ascii_case("checkpoint")
                    || model_format.eq_ignore_ascii_case("checkpoint_file")));
        if !(include_loras && is_lora || include_checkpoints && is_checkpoint) {
            continue;
        }
        let key = config
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .to_owned();
        let name = config
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&key)
            .to_owned();
        let Some(stored_path) = config.get("path").and_then(Value::as_str) else {
            continue;
        };
        let path = PathBuf::from(stored_path);
        let triggers = config.get("trigger_phrases");
        let needs_triggers = !has_content(triggers);
        let needs_thumbnail = !has_content(config.get("cover_image"));
        models.push(Model {
            key,
            name,
            kind: if is_lora { "LoRA" } else { "checkpoint" }.to_owned(),
            path,
            needs_triggers,
            needs_thumbnail,
        });
    }
    Ok(models)
}

fn fingerprint(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|time| time.as_nanos())
        .unwrap_or(0);
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    Ok(format!(
        "{}|{}|{}",
        canonical.to_string_lossy().to_lowercase(),
        metadata.len(),
        modified
    ))
}

fn calculate_hashes(path: &Path, cancelled: &AtomicBool) -> Result<Vec<(String, String)>, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut candidates = Vec::new();
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("safetensors"))
    {
        let mut length_bytes = [0u8; 8];
        file.read_exact(&mut length_bytes)
            .map_err(|_| "Invalid safetensors header.".to_owned())?;
        let header_len = u64::from_le_bytes(length_bytes);
        let file_size = file.metadata().map_err(|e| e.to_string())?.len();
        if header_len > file_size.saturating_sub(8) {
            return Err("Invalid safetensors header length.".into());
        }
        let mut full = Context::new(&SHA256);
        full.update(&length_bytes);
        let mut header_left = header_len;
        let mut buffer = vec![0u8; 8 * 1024 * 1024];
        while header_left > 0 {
            if cancelled.load(Ordering::Relaxed) {
                return Err(CANCELLED_REASON.into());
            }
            let count =
                usize::try_from(header_left.min(buffer.len() as u64)).unwrap_or(buffer.len());
            file.read_exact(&mut buffer[..count])
                .map_err(|_| "Could not read complete safetensors header.".to_owned())?;
            full.update(&buffer[..count]);
            header_left -= count as u64;
        }
        let mut tensor_hash = Context::new(&SHA256);
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(CANCELLED_REASON.into());
            }
            let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
            if count == 0 {
                break;
            }
            tensor_hash.update(&buffer[..count]);
            full.update(&buffer[..count]);
        }
        candidates.push((
            "AutoV3".to_owned(),
            hex::encode_upper(&tensor_hash.finish().as_ref()[..6]),
        ));
        candidates.push(("SHA256".to_owned(), hex::encode_upper(full.finish().as_ref())));
    } else {
        let mut hash = Context::new(&SHA256);
        let mut buffer = vec![0u8; 8 * 1024 * 1024];
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(CANCELLED_REASON.into());
            }
            let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        candidates.push(("SHA256".to_owned(), hex::encode_upper(hash.finish().as_ref())));
    }
    candidates.dedup_by(|a, b| a.1 == b.1);
    Ok(candidates)
}

fn lookup_civitai(
    client: &reqwest::blocking::Client,
    candidates: &[(String, String)],
    api_key: &str,
    cancelled: &AtomicBool,
) -> Result<Option<Value>, String> {
    for (_algorithm, hash) in candidates {
        if cancelled.load(Ordering::Relaxed) {
            return Err(CANCELLED_REASON.into());
        }
        let mut request = client
            .get(format!("{CIVITAI_API}{hash}"))
            .header("User-Agent", "Invoke-Tool-Suite/1.0");
        if !api_key.trim().is_empty() {
            request = request.bearer_auth(api_key.trim());
        }
        let response = request
            .send()
            .map_err(|e| format!("CivitAI connection failed: {e}"))?;
        if cancelled.load(Ordering::Relaxed) {
            return Err(CANCELLED_REASON.into());
        }
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("CivitAI returned HTTP {}.", response.status()));
        }
        return response
            .json::<Value>()
            .map(Some)
            .map_err(|e| format!("Could not read CivitAI response: {e}"));
    }
    Ok(None)
}

fn extract_phrases(data: &Value) -> Vec<String> {
    data.get("trainedWords")
        .and_then(Value::as_array)
        .map(|words| {
            words
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|word| !word.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn extract_preview(data: &Value) -> Option<String> {
    data.get("images")?
        .as_array()?
        .iter()
        .filter_map(|image| image.get("url").and_then(Value::as_str))
        .find(|url| url.starts_with("https://"))
        .map(str::to_owned)
}

fn fetch_preview(
    client: &reqwest::blocking::Client,
    url: &str,
    cancelled: &AtomicBool,
) -> Result<String, String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(CANCELLED_REASON.into());
    }
    if !url.starts_with("https://") {
        return Err("CivitAI preview URL must use HTTPS.".into());
    }
    let response = client
        .get(url)
        .header("User-Agent", "Invoke-Tool-Suite/1.0")
        .send()
        .map_err(|e| format!("Could not download preview: {e}"))?;
    if response.url().scheme() != "https" {
        return Err("Preview redirected to a non-HTTPS address.".into());
    }
    if !response.status().is_success() {
        return Err(format!(
            "Preview server returned HTTP {}.",
            response.status()
        ));
    }
    let mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if !mime.starts_with("image/") {
        return Err(format!("Preview URL returned non-image content: {mime}."));
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_PREVIEW_BYTES)
    {
        return Err("Preview image exceeds the 20 MB limit.".into());
    }
    let mut bytes = Vec::new();
    let mut response = response.take(MAX_PREVIEW_BYTES + 1);
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(CANCELLED_REASON.into());
        }
        let count = response
            .read(&mut buffer)
            .map_err(|e| format!("Could not read preview image: {e}"))?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.is_empty() {
        return Err("Downloaded preview image is empty.".into());
    }
    if bytes.len() as u64 > MAX_PREVIEW_BYTES {
        return Err("Preview image exceeds the 20 MB limit.".into());
    }
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

fn save_config(
    db: &Connection,
    key: &str,
    phrases: Option<&[String]>,
    cover: Option<&str>,
) -> Result<(), String> {
    let stored: String = db
        .query_row(
            "SELECT config FROM models WHERE id=?1",
            params![key],
            |row| row.get(0),
        )
        .map_err(|e| format!("Could not reload model record before updating it: {e}"))?;
    let mut updated: Value = serde_json::from_str(&stored)
        .map_err(|e| format!("Could not parse model configuration: {e}"))?;
    if let Some(phrases) = phrases {
        updated["trigger_phrases"] = json!(phrases);
    }
    if let Some(cover) = cover {
        updated["cover_image"] = json!(cover);
    }
    let serialized = serde_json::to_string(&updated).map_err(|e| e.to_string())?;
    db.execute(
        "UPDATE models SET config=?1 WHERE id=?2",
        params![serialized, key],
    )
    .map_err(|e| format!("Could not update model record: {e}"))?;
    Ok(())
}

fn emit_model_progress(
    window: &tauri::Window,
    current: usize,
    total: usize,
    model_name: &str,
    status: &str,
) {
    let percent = super::task_progress_percent(current, total);
    super::set_taskbar_percent(window, percent);
    let _ = window.emit(
        "civitai-progress",
        Progress {
            current,
            total,
            percent,
            model_name: model_name.to_owned(),
            status: status.to_owned(),
        },
    );
}

fn process_model(
    window: &tauri::Window,
    current: usize,
    total: usize,
    db: &Connection,
    client: &reqwest::blocking::Client,
    model: &Model,
    cache: &mut HashMap<String, CachedMatch>,
    api_key: &str,
    cancelled: &AtomicBool,
    stats: &mut Report,
) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(CANCELLED_REASON.into());
    }
    if !model.needs_triggers {
        stats.already_had_triggers += 1;
    }
    if !model.needs_thumbnail {
        stats.already_had_thumbnails += 1;
    }
    if !model.needs_triggers && !model.needs_thumbnail {
        return Ok(());
    }
    if !model.path.is_file() {
        stats.skipped += 1;
        return Err("Model file is missing.".into());
    }
    emit_model_progress(
        window,
        current,
        total,
        &model.name,
        &format!("Checking saved {} lookup", model.kind),
    );
    let current_fingerprint = fingerprint(&model.path)?;
    let saved_entry = cache.get(&model.key);
    let cached = saved_entry
        .filter(|entry| entry.fingerprint == current_fingerprint)
        .cloned();
    let (phrases, preview_url) = if let Some(entry) = cached {
        stats.cache_hits += 1;
        emit_model_progress(
            window,
            current,
            total,
            &model.name,
            &format!("Using saved {} lookup", model.kind),
        );
        if entry.status == "not_found" {
            stats.cached_not_found += 1;
            return Ok(());
        }
        (entry.trigger_phrases, entry.preview_url)
    } else {
        stats.lookup_attempts += 1;
        let hashing_status = if saved_entry.is_some() {
            format!(
                "Saved {} lookup is for a different file version; hashing it again",
                model.kind
            )
        } else {
            format!("No saved {} lookup; hashing it now", model.kind)
        };
        emit_model_progress(
            window,
            current,
            total,
            &model.name,
            &hashing_status,
        );
        let candidates = calculate_hashes(&model.path, cancelled)?;
        if fingerprint(&model.path)? != current_fingerprint {
            return Err("Model file changed while it was being hashed.".into());
        }
        emit_model_progress(
            window,
            current,
            total,
            &model.name,
            &format!("Looking up {} on CivitAI", model.kind),
        );
        let Some(data) = lookup_civitai(client, &candidates, api_key, cancelled)? else {
            cache.insert(
                model.key.clone(),
                CachedMatch {
                    fingerprint: current_fingerprint,
                    status: "not_found".into(),
                    trigger_phrases: Vec::new(),
                    preview_url: None,
                },
            );
            stats.not_found += 1;
            return Ok(());
        };
        let phrases = extract_phrases(&data);
        let preview_url = extract_preview(&data);
        cache.insert(
            model.key.clone(),
            CachedMatch {
                fingerprint: current_fingerprint,
                status: "matched".into(),
                trigger_phrases: phrases.clone(),
                preview_url: preview_url.clone(),
            },
        );
        (phrases, preview_url)
    };
    let add_phrases = model.needs_triggers && !phrases.is_empty();
    if model.needs_triggers && phrases.is_empty() {
        stats.no_trigger_phrases += 1;
    }
    if add_phrases {
        save_config(db, &model.key, Some(&phrases), None)?;
        stats.trigger_phrases_added += 1;
    }
    let cover = if model.needs_thumbnail {
        match preview_url {
            Some(url) => {
                emit_model_progress(
                    window,
                    current,
                    total,
                    &model.name,
                    &format!("Downloading {} preview", model.kind),
                );
                match fetch_preview(client, &url, cancelled) {
                    Ok(data_url) => Some(data_url),
                    Err(error) => {
                        stats.errors += 1;
                        return Err(error);
                    }
                }
            }
            None => {
                stats.no_preview_image += 1;
                None
            }
        }
    } else {
        stats.already_had_thumbnails += 1;
        None
    };
    if let Some(cover) = cover {
        save_config(db, &model.key, None, Some(&cover))?;
        stats.thumbnails_added += 1;
    }
    Ok(())
}

fn cache_path_for(database: &str) -> PathBuf {
    Path::new(database).with_file_name("invoke-tool-suite-civitai-cache.json")
}

fn inspect_cache(path: &Path) -> CacheInfo {
    let exists = path.is_file();
    let mut info = CacheInfo {
        cache_path: path.to_string_lossy().into_owned(),
        cache_exists: exists,
        entries: 0,
        matched: 0,
        not_found: 0,
        read_error: None,
    };
    if !exists {
        return info;
    }
    match fs::read(path).map_err(|e| e.to_string()).and_then(|bytes| {
        serde_json::from_slice::<HashMap<String, CachedMatch>>(&bytes).map_err(|e| e.to_string())
    }) {
        Ok(cache) => {
            info.entries = cache.len();
            info.matched = cache
                .values()
                .filter(|entry| entry.status == "matched")
                .count();
            info.not_found = cache
                .values()
                .filter(|entry| entry.status == "not_found")
                .count();
        }
        Err(error) => info.read_error = Some(error),
    }
    info
}

fn persist_cache(path: &Path, cache: &HashMap<String, CachedMatch>) -> Result<(), String> {
    let temp_path = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(cache)
        .map_err(|error| format!("Could not serialize the CivitAI cache: {error}"))?;
    let mut file = File::create(&temp_path)
        .map_err(|error| format!("Could not write the CivitAI cache: {error}"))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("Could not finish writing the CivitAI cache: {error}"))?;
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Could not replace the CivitAI cache: {error}")),
    }
    fs::rename(&temp_path, path)
        .map_err(|error| format!("Could not replace the CivitAI cache: {error}"))
}

fn migrate_legacy_cache(database: &str) -> Result<PathBuf, String> {
    let current_path = cache_path_for(database);
    let legacy_path = Path::new(database).with_file_name("invoke-tools-civitai-cache.json");
    if !legacy_path.is_file() {
        return Ok(current_path);
    }
    let legacy_bytes = fs::read(&legacy_path)
        .map_err(|error| format!("Could not read the old CivitAI cache: {error}"))?;
    let mut legacy: HashMap<String, CachedMatch> = serde_json::from_slice(&legacy_bytes)
        .map_err(|error| format!("Could not migrate the old CivitAI cache: {error}"))?;
    if current_path.is_file() {
        let current_bytes = fs::read(&current_path)
            .map_err(|error| format!("Could not read the current CivitAI cache: {error}"))?;
        let mut current: HashMap<String, CachedMatch> = serde_json::from_slice(&current_bytes)
            .map_err(|error| format!("Could not read the current CivitAI cache: {error}"))?;
        for (key, value) in legacy.drain() {
            current.entry(key).or_insert(value);
        }
        persist_cache(&current_path, &current)?;
    } else {
        persist_cache(&current_path, &legacy)?;
    }
    fs::remove_file(&legacy_path)
        .map_err(|error| format!("Could not remove the old CivitAI cache: {error}"))?;
    Ok(current_path)
}

#[tauri::command]
pub(super) fn get_civitai_cache_info(database: String) -> Result<CacheInfo, String> {
    let path = Path::new(&database);
    if !path.is_file() {
        return Err("Choose an existing v7 database file first.".into());
    }
    Ok(inspect_cache(&migrate_legacy_cache(&database)?))
}

#[tauri::command]
pub(super) fn clear_civitai_cache(database: String) -> Result<CacheInfo, String> {
    let path = Path::new(&database);
    if !path.is_file() {
        return Err("Choose an existing v7 database file first.".into());
    }
    let cache_path = migrate_legacy_cache(&database)?;
    match fs::remove_file(&cache_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Could not clear the CivitAI cache: {error}")),
    }
    Ok(inspect_cache(&cache_path))
}

#[tauri::command]
pub async fn enrich_civitai_models(
    window: tauri::Window,
    backups: tauri::State<'_, super::SafetyBackups>,
    tasks: tauri::State<'_, CivitaiTasks>,
    database: String,
    include_loras: bool,
    include_checkpoints: bool,
    refresh_cache: bool,
    task_id: String,
) -> Result<Report, String> {
    let backups = backups.0.clone();
    let task_registry = tasks.0.clone();
    let retention = super::backup_retention(window.app_handle())?;
    if !include_loras && !include_checkpoints {
        return Err("Select LoRAs, checkpoints, or both.".into());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    {
        let mut registry = task_registry
            .lock()
            .map_err(|_| "CivitAI task registry is unavailable.".to_owned())?;
        if registry.contains_key(&task_id) {
            return Err("A CivitAI job with this id is already running.".into());
        }
        registry.insert(task_id.clone(), cancelled.clone());
    }
    let worker_task_id = task_id.clone();
    let worker_cancelled = cancelled.clone();
    super::set_taskbar_indeterminate(&window);
    let taskbar_window = window.clone();
    let join = tauri::async_runtime::spawn_blocking(move || {
        let database_path = Path::new(&database);
        if !database_path.is_file() {
            return Err("Choose an existing v7 database file.".to_owned());
        }
        let db = Connection::open_with_flags(
            database_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| format!("Could not open the v7 model database: {e}"))?;
        let models = discover_models(&db, include_loras, include_checkpoints)?;
        let destination_key = database_path
            .canonicalize()
            .unwrap_or_else(|_| database_path.to_path_buf());
        let existing = backups
            .lock()
            .map_err(|_| "Safety backup registry is unavailable.".to_owned())?
            .get(&destination_key)
            .filter(|path| path.is_file())
            .cloned();
        let (backup_path, backup_created) = if let Some(path) = existing {
            super::prune_safety_backups(database_path, retention)?;
            (path, false)
        } else {
            let path = backup_database(&db, database_path, retention)?;
            backups
                .lock()
                .map_err(|_| "Safety backup registry is unavailable.".to_owned())?
                .insert(destination_key, path.clone());
            (path, true)
        };
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| format!("Could not initialize secure CivitAI connection: {e}"))?;
        let api_key = load_civitai_api_key()?.unwrap_or_default();
        let cache_path = migrate_legacy_cache(&database)?;
        if refresh_cache {
            match fs::remove_file(&cache_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("Could not clear the CivitAI cache: {error}")),
            }
        }
        let mut cache: HashMap<String, CachedMatch> = fs::read(&cache_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let total = models.len();
        let mut report = Report {
            models_scanned: total,
            models_processed: 0,
            cancelled: false,
            cache_hits: 0,
            cached_not_found: 0,
            lookup_attempts: 0,
            not_found: 0,
            no_trigger_phrases: 0,
            no_preview_image: 0,
            trigger_phrases_added: 0,
            thumbnails_added: 0,
            already_had_triggers: 0,
            already_had_thumbnails: 0,
            skipped: 0,
            errors: 0,
            backup_path: backup_path.to_string_lossy().into_owned(),
            backup_created,
            issues: Vec::new(),
        };
        let mut cache_dirty = false;
        for (index, model) in models.iter().enumerate() {
            if worker_cancelled.load(Ordering::Relaxed) {
                report.cancelled = true;
                let _ = window.emit(
                    "civitai-progress",
                    Progress {
                        current: report.models_processed,
                        total,
                        percent: super::task_progress_percent(report.models_processed, total),
                        model_name: String::new(),
                        status: "Cancelled".into(),
                    },
                );
                break;
            }
            let _ = window.emit(
                "civitai-progress",
                Progress {
                    current: index + 1,
                    total,
                    percent: super::task_progress_percent(index + 1, total),
                    model_name: model.name.clone(),
                    status: "Looking up model".into(),
                },
            );
            super::set_taskbar_percent(&window, super::task_progress_percent(index + 1, total));
            let attempts_before = report.lookup_attempts;
            let result = process_model(
                &window,
                index + 1,
                total,
                &db,
                &client,
                model,
                &mut cache,
                &api_key,
                &worker_cancelled,
                &mut report,
            );
            if report.lookup_attempts > attempts_before {
                cache_dirty = true;
            }
            // Save each attempted lookup before a cancellation can leave the loop.
            if cache_dirty {
                match persist_cache(&cache_path, &cache) {
                    Ok(()) => {
                        cache_dirty = false;
                    }
                    Err(reason) => {
                        report.errors += 1;
                        report.issues.push(Issue {
                            model_name: "CivitAI lookup cache".into(),
                            reason,
                        });
                        break;
                    }
                }
            }
            let cancellation_requested = worker_cancelled.load(Ordering::Relaxed);
            if matches!(&result, Err(reason) if reason == CANCELLED_REASON) {
                report.cancelled = true;
                break;
            }
            if let Err(reason) = result {
                if !cancellation_requested {
                    report.errors += 1;
                    report.issues.push(Issue {
                        model_name: model.name.clone(),
                        reason,
                    });
                }
            }
            report.models_processed += 1;
            if cancellation_requested {
                report.cancelled = true;
                break;
            }
        }
        report.cancelled |= worker_cancelled.load(Ordering::Relaxed);
        if cache_dirty {
            if let Err(reason) = persist_cache(&cache_path, &cache) {
                report.errors += 1;
                report.issues.push(Issue {
                    model_name: "CivitAI lookup cache".into(),
                    reason,
                });
            }
        }
        Ok(report)
    })
    .await;
    if let Ok(mut registry) = task_registry.lock() {
        registry.remove(&worker_task_id);
    }
    super::clear_taskbar_progress(&taskbar_window);
    join.map_err(|error| format!("CivitAI worker failed: {error}"))?
}

#[tauri::command]
pub(super) fn cancel_civitai_enrichment(
    tasks: tauri::State<'_, CivitaiTasks>,
    task_id: String,
) -> Result<(), String> {
    let registry = tasks
        .0
        .lock()
        .map_err(|_| "CivitAI task registry is unavailable.".to_owned())?;
    let Some(cancelled) = registry.get(&task_id) else {
        return Err("That CivitAI job is no longer running.".into());
    };
    cancelled.store(true, Ordering::Relaxed);
    Ok(())
}
