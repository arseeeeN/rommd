#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires: u64,
    pub refresh_token: String,
    pub refresh_expires: u64,
}

#[derive(Debug, Deserialize)]
pub struct RomDetail {
    pub id: i64,
    pub fs_name: String,
    pub platform_fs_slug: String,
    pub name: Option<String>,
    /// Component files for multi-file ROMs. Empty for single-file ROMs.
    #[serde(default)]
    pub files: Vec<RomFileSchema>,
}

#[derive(Debug, Deserialize)]
pub struct RomFileSchema {
    pub id: i64,
    pub file_name: String,
}

#[derive(Debug, Deserialize)]
pub struct SaveSchema {
    pub id: i64,
    pub rom_id: i64,
    pub file_name: String,
    pub file_size_bytes: i64,
    pub emulator: Option<String>,
    pub slot: Option<String>,
    pub download_path: String,
    pub updated_at: String,
    /// MD5 of the file, or for zips of the sorted `path:md5` entry lines
    /// (see `save_archive::content_hash`). Absent on older RomM versions.
    #[serde(default)]
    pub content_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StateSchema {
    pub id: i64,
    pub rom_id: i64,
    pub file_name: String,
    pub file_size_bytes: i64,
    pub emulator: Option<String>,
    pub download_path: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProfilePayload {
    pub name: String,
    pub data: serde_json::Value,
}
