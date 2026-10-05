//! An in-process fake of the RomM save and state API, reproducing the
//! behaviour of RomM 5.2.0's `endpoints/saves.py` and `endpoints/states.py`
//! that rommd relies on:
//!
//! - A slotted save upload is a new version: RomM tags the file name with the
//!   upload time (`name [YYYY-MM-DD_HH-MM-SS].ext`). An upload without a slot
//!   keeps the name and updates the entry with the same name in place.
//! - Without `overwrite`, a slotted upload whose content hash already exists
//!   in the slot returns that existing entry instead of storing a new version.
//! - `autocleanup` keeps the newest `autocleanup_limit` saves of the ROM in
//!   that slot, whatever their file name or emulator.
//! - `PUT` replaces an entry's content and keeps its file name.
//! - `content_hash` is the MD5 of the file, or for zips the MD5 over the sorted
//!   `name:md5` lines of the entries joined by `\n`. Implemented here
//!   independently of `save_archive`, so the two check each other.
//!
//! A virtual clock advances one second per stored upload, so version order and
//! the timestamps in names are deterministic.

use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use md5::{Digest, Md5};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};

use crate::config::RommConfig;
use crate::romm::RommClient;

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: i64,
    pub rom_id: i64,
    pub file_name: String,
    pub emulator: Option<String>,
    pub slot: Option<String>,
    pub data: Vec<u8>,
    pub updated_at: i64,
    pub content_hash: String,
}

#[derive(Default)]
struct Store {
    next_id: i64,
    clock: i64,
    saves: Vec<Entry>,
    states: Vec<Entry>,
    fail_listing: bool,
    /// "METHOD path" of every write, for asserting what a sync did.
    writes: Vec<String>,
}

impl Store {
    fn tick(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }

    fn new_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }
}

type Shared = Arc<Mutex<Store>>;

pub struct FakeRomm {
    base_url: String,
    store: Shared,
}

impl FakeRomm {
    pub async fn start() -> Self {
        let store: Shared = Arc::new(Mutex::new(Store {
            // An hour back, so local edits made during a test are newer than
            // anything RomM holds unless a test ages them.
            clock: chrono::Utc::now().timestamp() - 3600,
            ..Default::default()
        }));
        let app = Router::new()
            .route("/api/saves", get(list_saves).post(add_save))
            .route("/api/saves/{id}", put(update_save))
            .route("/api/saves/{id}/content", get(save_content))
            .route("/api/states", get(list_states).post(add_state))
            .route("/api/states/{id}", put(update_state))
            .route("/api/states/{id}/content", get(state_content))
            .with_state(store.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base_url: format!("http://{}", addr), store }
    }

    pub fn client(&self) -> RommClient {
        RommClient::new(&RommConfig {
            base_url: self.base_url.clone(),
            api_token: Some("test".into()),
            username: None,
            password: None,
        })
    }

    /// Saves of a ROM, oldest first.
    pub fn saves(&self, rom_id: i64) -> Vec<Entry> {
        let mut v: Vec<Entry> = self.store.lock().unwrap().saves.iter().filter(|e| e.rom_id == rom_id).cloned().collect();
        v.sort_by_key(|e| (e.updated_at, e.id));
        v
    }

    pub fn states(&self, rom_id: i64) -> Vec<Entry> {
        self.store.lock().unwrap().states.iter().filter(|e| e.rom_id == rom_id).cloned().collect()
    }

    /// Store a save exactly as given, as another client (argosy, RomM's web
    /// player) would have left it.
    pub fn seed_save(&self, rom_id: i64, file_name: &str, emulator: &str, slot: Option<&str>, data: &[u8]) -> i64 {
        let mut s = self.store.lock().unwrap();
        let (id, at) = (s.new_id(), s.tick());
        s.saves.push(Entry {
            id,
            rom_id,
            file_name: file_name.into(),
            emulator: Some(emulator.into()),
            slot: slot.map(Into::into),
            content_hash: content_hash(data),
            data: data.to_vec(),
            updated_at: at,
        });
        id
    }

    pub fn set_fail_listing(&self, fail: bool) {
        self.store.lock().unwrap().fail_listing = fail;
    }

    /// Writes (uploads and updates) received so far.
    pub fn writes(&self) -> Vec<String> {
        self.store.lock().unwrap().writes.clone()
    }
}

// ── Hashing (RomM's scheme) ───────────────────────────────────────────────────

pub fn content_hash(data: &[u8]) -> String {
    if data.starts_with(b"PK\x03\x04")
        && let Ok(mut zip) = zip::ZipArchive::new(std::io::Cursor::new(data)) {
            let mut lines = Vec::new();
            for i in 0..zip.len() {
                let mut f = zip.by_index(i).unwrap();
                if f.is_dir() {
                    continue;
                }
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).unwrap();
                lines.push(format!("{}:{:x}", f.name(), Md5::digest(&buf)));
            }
            lines.sort();
            return format!("{:x}", Md5::digest(lines.join("\n").as_bytes()));
        }
    format!("{:x}", Md5::digest(data))
}

fn tag_with_time(name: &str, at: i64) -> String {
    let stamp = chrono::DateTime::from_timestamp(at, 0).unwrap().format("%Y-%m-%d_%H-%M-%S");
    match name.rsplit_once('.') {
        Some((stem, ext)) => format!("{} [{}].{}", stem, stamp, ext),
        None => format!("{} [{}]", name, stamp),
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

fn save_json(e: &Entry) -> serde_json::Value {
    serde_json::json!({
        "id": e.id,
        "rom_id": e.rom_id,
        "file_name": e.file_name,
        "file_size_bytes": e.data.len(),
        "emulator": e.emulator,
        "slot": e.slot,
        "download_path": format!("/api/saves/{}/content", e.id),
        "updated_at": chrono::DateTime::from_timestamp(e.updated_at, 0).unwrap().to_rfc3339(),
        "content_hash": e.content_hash,
    })
}

fn state_json(e: &Entry) -> serde_json::Value {
    serde_json::json!({
        "id": e.id,
        "rom_id": e.rom_id,
        "file_name": e.file_name,
        "file_size_bytes": e.data.len(),
        "emulator": e.emulator,
        "download_path": format!("/api/states/{}/content", e.id),
        "updated_at": chrono::DateTime::from_timestamp(e.updated_at, 0).unwrap().to_rfc3339(),
    })
}

async fn read_file(mut form: Multipart, field: &str) -> (String, Vec<u8>) {
    while let Some(part) = form.next_field().await.unwrap() {
        if part.name() == Some(field) {
            let name = part.file_name().unwrap_or_default().to_owned();
            return (name, part.bytes().await.unwrap().to_vec());
        }
    }
    panic!("multipart field {} missing", field);
}

type Params = Query<HashMap<String, String>>;

fn rom_id(q: &HashMap<String, String>) -> i64 {
    q.get("rom_id").and_then(|v| v.parse().ok()).expect("rom_id")
}

async fn list_saves(State(s): State<Shared>, Query(q): Params) -> Response {
    let s = s.lock().unwrap();
    if s.fail_listing {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let rom = rom_id(&q);
    Json(s.saves.iter().filter(|e| e.rom_id == rom).map(save_json).collect::<Vec<_>>()).into_response()
}

async fn add_save(State(s): State<Shared>, Query(q): Params, form: Multipart) -> Json<serde_json::Value> {
    let (name, data) = read_file(form, "saveFile").await;
    let mut s = s.lock().unwrap();
    let rom = rom_id(&q);
    let emulator = q.get("emulator").cloned();
    let slot = q.get("slot").cloned();
    let overwrite = q.get("overwrite").is_some_and(|v| v == "true");
    let autocleanup = q.get("autocleanup").is_some_and(|v| v == "true");
    let limit: usize = q.get("autocleanup_limit").and_then(|v| v.parse().ok()).unwrap_or(10).max(1);
    let hash = content_hash(&data);
    s.writes.push(format!("POST save {} slot={:?}", name, slot));

    if let (Some(slot), false) = (&slot, overwrite)
        && let Some(existing) = s.saves.iter().find(|e| e.rom_id == rom && e.slot.as_ref() == Some(slot) && e.content_hash == hash) {
            return Json(save_json(existing));
        }
    let at = s.tick();
    let file_name = if slot.is_some() { tag_with_time(&name, at) } else { name };
    let result = match s.saves.iter_mut().find(|e| e.rom_id == rom && e.file_name == file_name && e.slot == slot) {
        Some(e) => {
            e.data = data;
            e.content_hash = hash;
            e.emulator = emulator;
            e.updated_at = at;
            e.clone()
        }
        None => {
            let id = s.new_id();
            let e = Entry { id, rom_id: rom, file_name, emulator, slot: slot.clone(), data, updated_at: at, content_hash: hash };
            s.saves.push(e.clone());
            e
        }
    };
    if let (Some(slot), true) = (&slot, autocleanup) {
        let mut in_slot: Vec<(i64, i64)> = s
            .saves
            .iter()
            .filter(|e| e.rom_id == rom && e.slot.as_ref() == Some(slot))
            .map(|e| (e.updated_at, e.id))
            .collect();
        in_slot.sort_by(|a, b| b.cmp(a));
        let doomed: Vec<i64> = in_slot.iter().skip(limit).map(|(_, id)| *id).collect();
        s.saves.retain(|e| !doomed.contains(&e.id));
    }
    Json(save_json(&result))
}

async fn update_save(State(s): State<Shared>, Path(id): Path<i64>, form: Multipart) -> Response {
    let (_, data) = read_file(form, "saveFile").await;
    let mut s = s.lock().unwrap();
    let at = s.tick();
    s.writes.push(format!("PUT save {}", id));
    match s.saves.iter_mut().find(|e| e.id == id) {
        Some(e) => {
            e.content_hash = content_hash(&data);
            e.data = data;
            e.updated_at = at;
            Json(save_json(e)).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn save_content(State(s): State<Shared>, Path(id): Path<i64>) -> Response {
    match s.lock().unwrap().saves.iter().find(|e| e.id == id) {
        Some(e) => e.data.clone().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn list_states(State(s): State<Shared>, Query(q): Params) -> Response {
    let s = s.lock().unwrap();
    if s.fail_listing {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let rom = rom_id(&q);
    Json(s.states.iter().filter(|e| e.rom_id == rom).map(state_json).collect::<Vec<_>>()).into_response()
}

async fn add_state(State(s): State<Shared>, Query(q): Params, form: Multipart) -> Json<serde_json::Value> {
    let (name, data) = read_file(form, "stateFile").await;
    let mut s = s.lock().unwrap();
    let rom = rom_id(&q);
    let emulator = q.get("emulator").cloned();
    let at = s.tick();
    s.writes.push(format!("POST state {}", name));
    let hash = content_hash(&data);
    let result = match s.states.iter_mut().find(|e| e.rom_id == rom && e.file_name == name) {
        Some(e) => {
            e.data = data;
            e.content_hash = hash;
            e.updated_at = at;
            e.clone()
        }
        None => {
            let id = s.new_id();
            let e = Entry { id, rom_id: rom, file_name: name, emulator, slot: None, data, updated_at: at, content_hash: hash };
            s.states.push(e.clone());
            e
        }
    };
    Json(state_json(&result))
}

async fn update_state(State(s): State<Shared>, Path(id): Path<i64>, form: Multipart) -> Response {
    let (_, data) = read_file(form, "stateFile").await;
    let mut s = s.lock().unwrap();
    let at = s.tick();
    s.writes.push(format!("PUT state {}", id));
    match s.states.iter_mut().find(|e| e.id == id) {
        Some(e) => {
            e.content_hash = content_hash(&data);
            e.data = data;
            e.updated_at = at;
            Json(state_json(e)).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn state_content(State(s): State<Shared>, Path(id): Path<i64>) -> Response {
    match s.lock().unwrap().states.iter().find(|e| e.id == id) {
        Some(e) => e.data.clone().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
