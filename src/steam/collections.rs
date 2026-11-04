//! Steam collections, one per platform, written as Steam ROM Manager writes
//! its own (`src/lib/category-manager.ts`): into the account's
//! `cloudstorage/cloud-storage-namespace-<n>.json`, a JSON list of `[key,
//! entry]` whose `user-collections.<id>` entries hold `{"id", "name",
//! "added": [app IDs], "removed": [...]}` as a JSON string. Steam merges them
//! with the cloud's copy (`union-collections`) when it starts.
//!
//! A collection the user already has with the platform's name gets the games
//! added; otherwise rommd makes one keyed `rommd-<name in hex>` (not SRM's
//! `srm-`, whose collections SRM deletes when it doesn't manage them).
//! rommd only takes out of a collection the shortcuts it made, and deletes
//! only its own collections once they are empty.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const PREFIX: &str = "user-collections.";
const OURS: &str = "rommd-";

/// The namespace file holding the user's collections: the first one
/// `cloud-storage-namespaces.json` lists that has any, else namespace 1.
fn namespace_file(cloud: &Path) -> PathBuf {
    let listed: Vec<(i64, Value)> =
        std::fs::read_to_string(cloud.join("cloud-storage-namespaces.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
    listed
        .iter()
        .map(|(id, _)| cloud.join(format!("cloud-storage-namespace-{id}.json")))
        .find(|f| std::fs::read_to_string(f).is_ok_and(|t| t.contains("\"user-collections.")))
        .unwrap_or_else(|| cloud.join("cloud-storage-namespace-1.json"))
}

fn ids(value: Option<&Value>) -> Vec<u32> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_u64())
                .map(|v| v as u32)
                .collect()
        })
        .unwrap_or_default()
}

/// Put rommd's shortcuts into a collection per platform. `by_platform`: the
/// app IDs of each platform's shortcuts; `ours`: every app ID rommd's
/// shortcuts had or have, which rommd may take out of a collection.
/// `timestamp` is now, in Unix seconds. Returns whether the file changed.
pub fn update(
    account: &Path,
    by_platform: &BTreeMap<String, Vec<u32>>,
    ours: &BTreeSet<u32>,
    timestamp: u64,
) -> Result<bool> {
    let cloud = account.join("cloudstorage");
    let file = namespace_file(&cloud);
    let mut list: Vec<Value> = match std::fs::read_to_string(&file) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not Steam's collection storage", file.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    if !apply(&mut list, by_platform, ours, timestamp) {
        return Ok(false);
    }
    std::fs::create_dir_all(&cloud)?;
    if file.exists() {
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        std::fs::copy(&file, cloud.join(format!("{name}.rommd-backup")))?;
    }
    let staged = file.with_extension("json.rommd-tmp");
    std::fs::write(&staged, serde_json::to_string(&list)?)?;
    std::fs::rename(&staged, &file)?;
    Ok(true)
}

/// The change to the list itself; whether anything changed.
fn apply(
    list: &mut Vec<Value>,
    by_platform: &BTreeMap<String, Vec<u32>>,
    ours: &BTreeSet<u32>,
    timestamp: u64,
) -> bool {
    // The live collections: key → (index in the list, parsed value).
    let mut live: BTreeMap<String, (usize, Value)> = BTreeMap::new();
    for (i, item) in list.iter().enumerate() {
        let (Some(key), Some(entry)) = (item.get(0).and_then(Value::as_str), item.get(1)) else {
            continue;
        };
        if !key.starts_with(PREFIX)
            || entry.get("is_deleted").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        if let Some(value) = entry
            .get("value")
            .and_then(Value::as_str)
            .and_then(|v| serde_json::from_str::<Value>(v).ok())
        {
            live.insert(key[PREFIX.len()..].to_owned(), (i, value));
        }
    }
    let by_name = |name: &str| {
        live.iter()
            .find(|(_, (_, v))| {
                v.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
            })
            .map(|(id, _)| id.clone())
    };

    // What each collection rommd touches should hold.
    let mut wanted: BTreeMap<String, (String, Vec<u32>)> = BTreeMap::new();
    for (platform, apps) in by_platform {
        let id = by_name(platform).unwrap_or_else(|| {
            let hex: String = platform.bytes().map(|b| format!("{b:02x}")).collect();
            format!("{OURS}{hex}")
        });
        wanted.insert(id, (platform.clone(), apps.clone()));
    }
    for id in live.keys().filter(|id| id.starts_with(OURS)) {
        wanted
            .entry(id.clone())
            .or_insert_with(|| (String::new(), Vec::new()));
    }

    let mut changed = false;
    for (id, (name, apps)) in wanted {
        let key = format!("{PREFIX}{id}");
        let old = live.get(&id);
        let mut added: Vec<u32> = ids(old.and_then(|(_, v)| v.get("added")))
            .into_iter()
            .filter(|a| !ours.contains(a) || apps.contains(a))
            .collect();
        for a in &apps {
            if !added.contains(a) {
                added.push(*a);
            }
        }
        let removed: Vec<u32> = ids(old.and_then(|(_, v)| v.get("removed")))
            .into_iter()
            .filter(|a| !apps.contains(a))
            .collect();
        let name = old
            .and_then(|(_, v)| v.get("name").and_then(Value::as_str))
            .map(str::to_owned)
            .unwrap_or(name);
        let value = json!({ "id": id, "name": name, "added": added, "removed": removed });
        if old.is_some_and(|(_, v)| *v == value) || (old.is_none() && added.is_empty()) {
            continue;
        }
        changed = true;
        let entry = if added.is_empty() && id.starts_with(OURS) {
            json!({ "key": key, "timestamp": timestamp, "is_deleted": true,
                    "version": timestamp.to_string() })
        } else {
            json!({ "key": key, "timestamp": timestamp, "value": value.to_string(),
                    "version": timestamp.to_string(), "conflictResolutionMethod": "custom",
                    "strMethodId": "union-collections" })
        };
        match old {
            Some((i, _)) => list[*i] = json!([key, entry]),
            None => list.push(json!([key, entry])),
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection(id: &str, name: &str, added: &[u32]) -> Value {
        let key = format!("{PREFIX}{id}");
        let value = json!({ "id": id, "name": name, "added": added, "removed": [] });
        json!([key, { "key": key, "timestamp": 1, "value": value.to_string(), "version": "1",
                      "conflictResolutionMethod": "custom", "strMethodId": "union-collections" }])
    }

    fn members(list: &[Value], id: &str) -> Option<(String, Vec<u32>)> {
        let item = list.iter().find(|i| i[0] == format!("{PREFIX}{id}"))?;
        if item[1]["is_deleted"] == true {
            return None;
        }
        let v: Value = serde_json::from_str(item[1]["value"].as_str()?).ok()?;
        Some((v["name"].as_str()?.to_owned(), ids(v.get("added"))))
    }

    #[test]
    fn games_go_into_a_collection_per_platform() {
        let mut list = vec![
            collection("favorite", "Favorites", &[10, 20]),
            collection("uc-abc", "Nintendo 64", &[30, 999]),
        ];
        let by_platform = BTreeMap::from([
            ("Nintendo 64".to_owned(), vec![100, 101]),
            ("PlayStation".to_owned(), vec![200]),
        ]);
        let ours = BTreeSet::from([100, 101, 200, 999]);
        assert!(apply(&mut list, &by_platform, &ours, 50));
        // The user's own collection of that name, minus the shortcut rommd dropped.
        assert_eq!(
            members(&list, "uc-abc"),
            Some(("Nintendo 64".into(), vec![30, 100, 101]))
        );
        let ps = format!(
            "{OURS}{}",
            "PlayStation"
                .bytes()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        assert_eq!(members(&list, &ps), Some(("PlayStation".into(), vec![200])));
        assert_eq!(
            members(&list, "favorite"),
            Some(("Favorites".into(), vec![10, 20]))
        );
        assert!(
            !apply(&mut list, &by_platform, &ours, 60),
            "nothing left to change"
        );
    }

    #[test]
    fn rommds_own_empty_collection_is_deleted() {
        let mut list = vec![collection("rommd-4e", "N", &[100])];
        assert!(apply(
            &mut list,
            &BTreeMap::new(),
            &BTreeSet::from([100]),
            70
        ));
        assert_eq!(list[0][1]["is_deleted"], true);
        assert_eq!(members(&list, "rommd-4e"), None);
    }
}
