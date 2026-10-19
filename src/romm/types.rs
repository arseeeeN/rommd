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
    /// The platform's canonical slug; picks the emulator.
    pub platform_slug: String,
    /// The platform's folder name on the server, which can be renamed.
    pub platform_fs_slug: String,
    #[serde(default)]
    pub platform_display_name: Option<String>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlatformSchema {
    pub id: i64,
    pub slug: String,
    pub fs_slug: String,
    pub name: String,
    #[serde(default)]
    pub custom_name: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// One ROM in a search result.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RomSummary {
    pub id: i64,
    pub name: Option<String>,
    pub fs_name: String,
    pub platform_slug: String,
    #[serde(default)]
    pub platform_display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RomPage {
    pub items: Vec<RomSummary>,
    pub total: i64,
}
