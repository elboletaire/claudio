//! The TUI client's persistent state (`~/.config/claudio/state.json`) and the
//! recovery merge against the daemon's view of the world.
//!
//! state.json remembers what only the client knows: tab order, user-given
//! names, the last active tab and recently used directories. The daemon
//! journal is authoritative for which sessions exist; [`merge`] and
//! [`merge_for_host`] reconcile the two on every (re)connect.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::paths;
use crate::proto::{SessionId, SessionInfo, SessionKind, SessionState};

/// Most recently used directories kept for the new-session wizard.
pub const MAX_RECENT_DIRS: usize = 20;

/// A session id and host that the user explicitly closed.
/// Persisted before the Kill is sent so recovery never resurrects it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KillTombstone {
    pub host: String,
    pub id: SessionId,
}

/// Everything persisted in state.json.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientState {
    #[serde(default)]
    pub sessions: Vec<SavedSession>,
    #[serde(default)]
    pub active: Option<SessionId>,
    /// Most recently used directories, keyed by host ("local" for the local
    /// machine). Older state.json files stored a flat `Vec<String>`; those are
    /// transparently promoted to `{"local": [...]}` on load via a custom
    /// deserialiser so no data is ever lost.
    #[serde(default, deserialize_with = "deser_recent_dirs")]
    pub recent_dirs: HashMap<String, Vec<String>>,
    /// Sessions the user explicitly closed; Kill is retried until acknowledged.
    #[serde(default)]
    pub killed: Vec<KillTombstone>,
    /// Per host, the claude version the user chose to skip updating to
    /// ("skip this version" in the update prompt).
    #[serde(default)]
    pub claude_skipped: HashMap<String, String>,
}

/// Deserialise `recent_dirs` from either the new `{host: [dirs]}` map or the
/// legacy flat `[dirs]` list. The legacy list is promoted to `{"local": [dirs]}`.
fn deser_recent_dirs<'de, D>(d: D) -> Result<HashMap<String, Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{MapAccess, SeqAccess, Visitor};

    struct Vis;

    impl<'de> Visitor<'de> for Vis {
        type Value = HashMap<String, Vec<String>>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a map of host→dirs or a legacy list of dirs")
        }

        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut out = HashMap::new();
            while let Some((k, v)) = map.next_entry::<String, Vec<String>>()? {
                out.insert(k, v);
            }
            Ok(out)
        }

        fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
            let mut dirs = Vec::new();
            while let Some(d) = seq.next_element::<String>()? {
                dirs.push(d);
            }
            let mut out = HashMap::new();
            if !dirs.is_empty() {
                out.insert("local".to_owned(), dirs);
            }
            Ok(out)
        }
    }

    d.deserialize_any(Vis)
}

/// One session as the client remembers it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedSession {
    pub id: SessionId,
    #[serde(default)]
    pub name: Option<String>,
    pub cwd: String,
    #[serde(default = "local")]
    pub host: String,
    #[serde(default)]
    pub claude_session_id: Option<String>,
    #[serde(default)]
    pub created_at: u64,
    /// The proxy profile name used by this session (profile name only, never the token).
    #[serde(default)]
    pub proxy: Option<String>,
    /// Launched first-party (`--first-party`). Omitted when false, so a
    /// state.json without it reads as gateway, as before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub first_party: bool,
    /// A terminal tab never resumes and never gets proxy env.
    #[serde(default)]
    pub kind: SessionKind,
}

fn local() -> String {
    "local".into()
}

impl ClientState {
    /// Read state.json. A missing or unreadable file yields the empty state:
    /// the daemon journal is authoritative, so nothing important is lost.
    pub fn load(path: &Path) -> ClientState {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Atomically write state.json.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        paths::write_atomic(path, &json)
    }

    /// Whether `id` is in the kill tombstone list.
    pub fn is_killed(&self, id: SessionId) -> bool {
        self.killed.iter().any(|t| t.id == id)
    }
}

/// Move `dir` to the front of `host`'s MRU list in `recent`, capped at
/// [`MAX_RECENT_DIRS`] per host.
pub fn push_recent(recent: &mut HashMap<String, Vec<String>>, host: &str, dir: &str) {
    let dirs = recent.entry(host.to_owned()).or_default();
    dirs.retain(|d| d != dir);
    dirs.insert(0, dir.to_owned());
    dirs.truncate(MAX_RECENT_DIRS);
}

/// Return the MRU directory list for `host` (empty slice when unknown).
pub fn recent_for_host<'a>(recent: &'a HashMap<String, Vec<String>>, host: &str) -> &'a [String] {
    recent.get(host).map(Vec::as_slice).unwrap_or(&[])
}

/// A session after reconciling state.json with the daemon.
#[derive(Debug, Clone, PartialEq)]
pub struct Recovered {
    /// The record to keep (and persist).
    pub saved: SavedSession,
    pub state: SessionState,
    pub title: Option<String>,
    /// `Some(args)` when the daemon knows the session but has no process for
    /// it: re-spawn it with these claude arguments (none for a terminal).
    pub respawn: Option<Vec<String>>,
}

/// Like [`merge_for_host`] but assigns the given `host` to live sessions that aren't
/// in the saved list (instead of always defaulting to "local").
///
/// Used when reconciling a specific remote host's session list.
pub fn merge_for_host(host: &str, saved: &ClientState, live: &[SessionInfo]) -> Vec<Recovered> {
    // Skip tombstoned sessions, and those that are not the manager's
    // (`--plain`'s: never shown, never respawned).
    let live_non_killed: Vec<&SessionInfo> = live
        .iter()
        .filter(|l| !l.ephemeral && !saved.is_killed(l.id))
        .collect();

    let known = saved.sessions.iter().filter_map(|s| {
        // Skip tombstoned.
        if saved.is_killed(s.id) {
            return None;
        }
        let info = live_non_killed.iter().find(|l| l.id == s.id)?;
        Some(recover(Some(s), info))
    });
    let unknown = live_non_killed
        .iter()
        .filter(|l| !saved.sessions.iter().any(|s| s.id == l.id))
        .map(|info| recover_with_host(None, info, host));
    known.chain(unknown).collect()
}

fn recover(saved: Option<&SavedSession>, info: &SessionInfo) -> Recovered {
    let host = saved.map_or_else(local, |s| s.host.clone());
    recover_with_host(saved, info, &host)
}

fn recover_with_host(saved: Option<&SavedSession>, info: &SessionInfo, host: &str) -> Recovered {
    let claude_session_id = info
        .claude_session_id
        .clone()
        .or_else(|| saved.and_then(|s| s.claude_session_id.clone()));
    let respawn = info.pid.is_none().then(|| match (info.kind, &claude_session_id) {
        (SessionKind::Claude, Some(csid)) => vec!["--resume".to_owned(), csid.clone()],
        _ => Vec::new(),
    });
    Recovered {
        saved: SavedSession {
            id: info.id,
            // A saved entry's name wins even when cleared (None): the daemon
            // may still carry the name from an older spawn.
            name: match saved {
                Some(s) => s.name.clone(),
                None => info.name.clone(),
            },
            cwd: info.cwd.clone(),
            host: saved
                .map(|s| s.host.clone())
                .unwrap_or_else(|| host.to_owned()),
            claude_session_id,
            created_at: info.created_at,
            proxy: saved.and_then(|s| s.proxy.clone()),
            first_party: saved.is_some_and(|s| s.first_party),
            // The daemon's journal is authoritative.
            kind: info.kind,
        },
        state: info.state,
        title: info.title.clone(),
        respawn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn first_party_is_saved_only_when_set() {
        let mut s = saved(Uuid::new_v4(), None, None);
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("first_party"), "{json}");

        s.first_party = true;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"first_party\":true"), "{json}");
        let back: SavedSession = serde_json::from_str(&json).unwrap();
        assert!(back.first_party);

        // A state.json written before the field reads as gateway.
        let old = json.replace(",\"first_party\":true", "");
        let back: SavedSession = serde_json::from_str(&old).unwrap();
        assert!(!back.first_party);
    }

    fn saved(id: Uuid, name: Option<&str>, csid: Option<&str>) -> SavedSession {
        SavedSession {
            id,
            name: name.map(Into::into),
            cwd: "/old".into(),
            host: "local".into(),
            claude_session_id: csid.map(Into::into),
            created_at: 1,
            proxy: None,
            first_party: false,
            kind: SessionKind::Claude,
        }
    }

    fn info(id: Uuid, pid: Option<u32>, csid: Option<&str>) -> SessionInfo {
        SessionInfo {
            id,
            cwd: "/srv".into(),
            name: Some("daemon-name".into()),
            title: Some("title".into()),
            created_at: 100,
            ..crate::tui::test_support::info(pid, csid)
        }
    }

    fn mk_recent(host: &str, dirs: &[&str]) -> HashMap<String, Vec<String>> {
        let mut m = HashMap::new();
        m.insert(host.to_owned(), dirs.iter().map(|s| s.to_string()).collect());
        m
    }

    #[test]
    fn json_round_trip() {
        let a = Uuid::new_v4();
        let state = ClientState {
            sessions: vec![saved(a, Some("api"), Some("c1"))],
            active: Some(a),
            recent_dirs: mk_recent("local", &["/srv", "/tmp"]),
            killed: vec![],
            claude_skipped: HashMap::from([("devbox".to_owned(), "2.1.296".to_owned())]),
        };
        let dir = std::env::temp_dir().join(format!("claudio-state-test-{}", Uuid::new_v4()));
        let path = dir.join("state.json");
        state.save(&path).unwrap();
        assert_eq!(ClientState::load(&path), state);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_tolerates_missing_and_partial_files() {
        let missing = std::env::temp_dir().join(format!("claudio-none-{}.json", Uuid::new_v4()));
        assert_eq!(ClientState::load(&missing), ClientState::default());
        let partial: ClientState = serde_json::from_str(
            r#"{"sessions":[{"id":"00000000-0000-0000-0000-000000000000","cwd":"/w"}]}"#,
        )
        .unwrap();
        assert_eq!(partial.sessions[0].host, "local");
        assert!(partial.recent_dirs.is_empty());
        assert!(partial.killed.is_empty());
        assert!(partial.claude_skipped.is_empty());
    }

    #[test]
    fn legacy_flat_recent_dirs_migrated_to_local() {
        // Old state.json stores recent_dirs as a flat array.
        let json = r#"{"recent_dirs":["/srv","/tmp"]}"#;
        let state: ClientState = serde_json::from_str(json).unwrap();
        let local_dirs = state.recent_dirs.get("local").map(Vec::as_slice).unwrap_or(&[]);
        assert_eq!(local_dirs, &["/srv", "/tmp"][..]);
    }

    #[test]
    fn recent_dirs_move_to_front_and_cap() {
        let mut recent = HashMap::new();
        for i in 0..25 {
            push_recent(&mut recent, "local", &format!("/d{i}"));
        }
        let dirs = recent.get("local").unwrap();
        assert_eq!(dirs.len(), MAX_RECENT_DIRS);
        assert_eq!(dirs[0], "/d24");
        push_recent(&mut recent, "local", "/d20");
        let dirs = recent.get("local").unwrap();
        assert_eq!(dirs[0], "/d20");
        assert_eq!(dirs.iter().filter(|d| **d == "/d20").count(), 1);
    }

    #[test]
    fn recent_dirs_per_host() {
        let mut recent = HashMap::new();
        push_recent(&mut recent, "local", "/local/dir");
        push_recent(&mut recent, "myhost", "/remote/dir");
        assert_eq!(
            recent_for_host(&recent, "local"),
            &["/local/dir".to_owned()]
        );
        assert_eq!(
            recent_for_host(&recent, "myhost"),
            &["/remote/dir".to_owned()]
        );
        // A host with no entries returns an empty slice.
        assert!(recent_for_host(&recent, "unknown").is_empty());
    }

    #[test]
    fn merge_keeps_saved_order_and_names_appends_unknown_drops_stale() {
        let (a, b, c, stale) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let state = ClientState {
            sessions: vec![
                saved(b, Some("bee"), None),
                saved(stale, None, None),
                saved(a, None, None),
            ],
            active: Some(a),
            recent_dirs: HashMap::new(),
            killed: vec![],
            ..Default::default()
        };
        let live = vec![
            info(a, Some(1), None),
            info(c, Some(2), None),
            info(b, Some(3), None),
        ];
        let merged = merge_for_host("local", &state, &live);
        let ids: Vec<Uuid> = merged.iter().map(|r| r.saved.id).collect();
        assert_eq!(ids, vec![b, a, c]);
        assert_eq!(merged[0].saved.name.as_deref(), Some("bee"));
        // A cleared name in state.json stays cleared.
        assert_eq!(merged[1].saved.name, None);
        // Unknown to state.json: the daemon's name is all we have.
        assert_eq!(merged[2].saved.name.as_deref(), Some("daemon-name"));
        // The daemon's cwd and creation time are authoritative.
        assert_eq!(merged[0].saved.cwd, "/srv");
        assert!(merged.iter().all(|r| r.respawn.is_none()));
    }

    #[test]
    fn dormant_sessions_respawn_with_newest_resume_id() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let state = ClientState {
            sessions: vec![
                saved(a, None, Some("old-a")),
                saved(b, None, Some("only-saved")),
                saved(c, None, None),
            ],
            ..Default::default()
        };
        let live = vec![
            info(a, None, Some("new-a")),
            info(b, None, None),
            info(c, None, None),
        ];
        let merged = merge_for_host("local", &state, &live);
        assert_eq!(
            merged[0].respawn,
            Some(vec!["--resume".into(), "new-a".into()])
        );
        assert_eq!(merged[0].saved.claude_session_id.as_deref(), Some("new-a"));
        assert_eq!(
            merged[1].respawn,
            Some(vec!["--resume".into(), "only-saved".into()])
        );
        assert_eq!(merged[2].respawn, Some(vec![]));
    }

    #[test]
    fn dormant_terminals_respawn_without_resume_and_keep_their_kind() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let state = ClientState {
            sessions: vec![saved(a, None, Some("stale"))],
            ..Default::default()
        };
        let mut shell = info(a, None, Some("stale"));
        shell.kind = SessionKind::Shell;
        let mut unknown = info(b, None, None);
        unknown.kind = SessionKind::Shell;
        let merged = merge_for_host("local", &state, &[shell, unknown]);
        // The daemon's kind wins over the (older) saved record, and a
        // terminal never resumes a conversation.
        for r in &merged {
            assert_eq!(r.saved.kind, SessionKind::Shell);
            assert_eq!(r.respawn, Some(vec![]));
        }
    }

    #[test]
    fn saved_sessions_without_kind_are_claude() {
        let old: SavedSession =
            serde_json::from_str(r#"{"id":"00000000-0000-0000-0000-000000000000","cwd":"/w"}"#)
                .unwrap();
        assert_eq!(old.kind, SessionKind::Claude);
    }

    #[test]
    fn tombstoned_sessions_are_suppressed_in_merge() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let state = ClientState {
            sessions: vec![saved(a, None, None), saved(b, None, None)],
            killed: vec![KillTombstone {
                host: "local".into(),
                id: a,
            }],
            ..Default::default()
        };
        let live = vec![info(a, Some(1), None), info(b, Some(2), None)];
        let merged = merge_for_host("local", &state, &live);
        // Session `a` is tombstoned: must not appear.
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].saved.id, b);
    }

    #[test]
    fn merge_for_host_assigns_correct_host_to_unknown_sessions() {
        let id = Uuid::new_v4();
        let state = ClientState::default();
        let live = vec![info(id, Some(1), None)];
        let merged = merge_for_host("myserver.example.com", &state, &live);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].saved.host, "myserver.example.com");
    }
}
