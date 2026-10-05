use super::state::SyncBase;

/// The RomM side of a save, as far as the decision needs it.
#[derive(Debug, Clone, Copy)]
pub struct RemoteView<'a> {
    pub id: i64,
    pub hash: Option<&'a str>,
    pub updated_at: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing exists on either side, or neither side changed.
    Nothing,
    /// Both sides hold the same content; just record that.
    InSync,
    Upload,
    Download,
    /// Both sides changed since the last sync (or they differ and there is no
    /// sync history to say which is newer).
    Conflict,
}

/// Three-way decision between the local save, the RomM save, and the state
/// both were in when last synced.
pub fn decide(
    local_hash: Option<&str>,
    remote: Option<RemoteView>,
    base: Option<&SyncBase>,
) -> Action {
    let (local, remote) = match (local_hash, remote) {
        (None, None) => return Action::Nothing,
        (Some(_), None) => return Action::Upload,
        (None, Some(_)) => return Action::Download,
        (Some(l), Some(r)) => (l, r),
    };
    if remote.hash == Some(local) {
        return Action::InSync;
    }
    let Some(base) = base else {
        return Action::Conflict;
    };
    let local_changed = base.local_hash != local;
    let remote_changed = remote.id != base.remote_id
        || match (remote.hash, base.remote_hash.as_deref()) {
            (Some(now), Some(then)) => now != then,
            _ => remote.updated_at != base.remote_updated_at,
        };
    match (local_changed, remote_changed) {
        (false, false) => Action::Nothing,
        (true, false) => Action::Upload,
        (false, true) => Action::Download,
        (true, true) => Action::Conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "2026-01-01T00:00:00Z";
    const T1: &str = "2026-01-02T00:00:00Z";

    fn base(local: &str, id: i64, remote: Option<&str>, at: &str) -> SyncBase {
        SyncBase {
            local_hash: local.into(),
            remote_id: id,
            remote_hash: remote.map(Into::into),
            remote_updated_at: at.into(),
        }
    }

    fn remote(
        id: i64,
        hash: Option<&'static str>,
        at: &'static str,
    ) -> Option<RemoteView<'static>> {
        Some(RemoteView {
            id,
            hash,
            updated_at: at,
        })
    }

    #[test]
    fn missing_sides() {
        assert_eq!(decide(None, None, None), Action::Nothing);
        assert_eq!(decide(Some("a"), None, None), Action::Upload);
        assert_eq!(
            decide(None, remote(1, Some("a"), T0), None),
            Action::Download
        );
    }

    #[test]
    fn equal_content_is_in_sync_even_without_history() {
        assert_eq!(
            decide(Some("a"), remote(1, Some("a"), T0), None),
            Action::InSync
        );
    }

    #[test]
    fn differing_content_without_history_is_a_conflict() {
        assert_eq!(
            decide(Some("a"), remote(1, Some("b"), T0), None),
            Action::Conflict
        );
    }

    #[test]
    fn three_way_with_hashes() {
        let b = base("a", 1, Some("a"), T0);
        assert_eq!(
            decide(Some("a"), remote(1, Some("r"), T1), Some(&b)),
            Action::Download
        );
        assert_eq!(
            decide(Some("l"), remote(1, Some("a"), T0), Some(&b)),
            Action::Upload
        );
        assert_eq!(
            decide(Some("l"), remote(1, Some("r"), T1), Some(&b)),
            Action::Conflict
        );
    }

    #[test]
    fn remote_hash_mismatch_with_our_hash_is_not_a_change() {
        // RomM hashed our upload differently than we did: unchanged if its own
        // hash is stable.
        let b = base("a", 1, Some("x"), T0);
        assert_eq!(
            decide(Some("a"), remote(1, Some("x"), T0), Some(&b)),
            Action::Nothing
        );
    }

    #[test]
    fn falls_back_to_timestamps_without_remote_hash() {
        let b = base("a", 1, None, T0);
        assert_eq!(
            decide(Some("a"), remote(1, None, T0), Some(&b)),
            Action::Nothing
        );
        assert_eq!(
            decide(Some("a"), remote(1, None, T1), Some(&b)),
            Action::Download
        );
        assert_eq!(
            decide(Some("l"), remote(1, None, T0), Some(&b)),
            Action::Upload
        );
    }

    #[test]
    fn switching_to_another_remote_entry_is_a_remote_change() {
        // e.g. a newer Ryujinx upload replaces the Eden entry we synced with.
        let b = base("a", 1, Some("a"), T0);
        assert_eq!(
            decide(Some("a"), remote(2, Some("r"), T1), Some(&b)),
            Action::Download
        );
    }
}
