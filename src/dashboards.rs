//! Dashboards read from a mounted directory, compiled and held in memory.

use crate::pb;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;
use vedavid_dashboard_dsl as dsl;

pub const DEFAULT_DIR: &str = "/etc/vedavid/dashboards";
pub const DEFAULT_POLL: Duration = Duration::from_secs(30);

/// Shipped so the app works on first install with no YAML written.
const BUILTINS: &[(&str, &str)] = &[
    (
        "cluster-health",
        include_str!("../builtins/cluster-health.yaml"),
    ),
    ("node-health", include_str!("../builtins/node-health.yaml")),
];

/// Enough of a file to tell a rescan that nothing on disk moved.
type Stamp = (PathBuf, SystemTime, u64);

/// A dashboard's path, for reporting, and its contents.
type SourceFile = (String, String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Builtin,
    Mounted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Serving the last tree that compiled; the newest source did not.
    Stale,
    /// Never compiled, so there is nothing to serve.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub hash: String,
    pub schema: u32,
    pub source: Source,
    pub status: Status,
    pub tree_json: Option<String>,
    pub error: Option<dsl::Diagnostic>,
}

impl Entry {
    /// What the relay compares, so an identical recompile is not a change.
    fn identity(&self) -> (&str, &str, Status, Option<&str>) {
        (
            &self.id,
            &self.hash,
            self.status,
            self.error.as_ref().map(|d| d.code),
        )
    }
}

#[derive(Debug, Default)]
struct State {
    generation: i64,
    entries: BTreeMap<String, Entry>,
    fingerprint: Vec<Stamp>,
}

pub struct Dashboards {
    dir: PathBuf,
    cluster_label: String,
    state: RwLock<State>,
    changed: broadcast::Sender<()>,
}

impl Dashboards {
    pub fn new(dir: impl Into<PathBuf>, cluster_label: impl Into<String>) -> Self {
        let (changed, _) = broadcast::channel(4);
        Self {
            dir: dir.into(),
            cluster_label: cluster_label.into(),
            state: RwLock::new(State::default()),
            changed,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changed.subscribe()
    }

    pub fn inventory(&self) -> pb::DashboardInventory {
        let state = self
            .state
            .read()
            .expect("the inventory lock is never poisoned");
        pb::DashboardInventory {
            cluster_label: self.cluster_label.clone(),
            generation: state.generation,
            dashboards: state.entries.values().map(entry_to_pb).collect(),
        }
    }

    pub fn document(&self, id: &str) -> Option<pb::DashboardDocumentResponse> {
        let state = self
            .state
            .read()
            .expect("the inventory lock is never poisoned");
        let entry = state.entries.get(id)?;
        let tree = entry.tree_json.as_ref()?;
        Some(pb::DashboardDocumentResponse {
            id: entry.id.clone(),
            hash: entry.hash.clone(),
            schema: entry.schema,
            render_tree: tree.clone().into_bytes(),
        })
    }

    /// `generation` advances only when the result differs.
    pub fn scan(&self) -> bool {
        let (files, fingerprint) = self.read_dir();
        let held = {
            let state = self
                .state
                .read()
                .expect("the inventory lock is never poisoned");
            state.entries.clone()
        };
        let entries = compile_all(&files, &held);

        let mut state = self
            .state
            .write()
            .expect("the inventory lock is never poisoned");
        state.fingerprint = fingerprint;
        let same = state.entries.len() == entries.len()
            && state
                .entries
                .values()
                .zip(entries.values())
                .all(|(a, b)| a.identity() == b.identity());
        if same {
            state.entries = entries;
            return false;
        }
        state.entries = entries;
        state.generation += 1;
        let generation = state.generation;
        drop(state);

        tracing::info!(generation, "dashboard inventory changed");
        let _ = self.changed.send(());
        true
    }

    /// A ConfigMap update lands as an atomic symlink swap of the directory.
    fn read_dir(&self) -> (Vec<SourceFile>, Vec<Stamp>) {
        let mut files: Vec<SourceFile> = Vec::new();
        let mut fingerprint: Vec<Stamp> = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            tracing::debug!(dir = %self.dir.display(), "no dashboard directory; built-ins only");
            return (files, fingerprint);
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("yaml") | Some("yml")
                )
            })
            .collect();
        paths.sort();
        for path in paths {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    if let Ok(meta) = std::fs::metadata(&path) {
                        fingerprint.push((
                            path.clone(),
                            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                            meta.len(),
                        ));
                    }
                    files.push((path.display().to_string(), text));
                }
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "unreadable"),
            }
        }
        (files, fingerprint)
    }

    fn unchanged_on_disk(&self) -> bool {
        let (_, fingerprint) = self.read_dir();
        let state = self
            .state
            .read()
            .expect("the inventory lock is never poisoned");
        state.fingerprint == fingerprint
    }

    /// Kubelet's own propagation delay is larger than this interval.
    pub async fn poll_forever(self: std::sync::Arc<Self>, every: Duration) {
        loop {
            tokio::time::sleep(every).await;
            let store = self.clone();
            let changed = tokio::task::spawn_blocking(move || {
                (!store.unchanged_on_disk()).then(|| store.scan())
            })
            .await;
            if let Err(e) = changed {
                tracing::warn!(error = %e, "dashboard scan panicked");
            }
        }
    }
}

fn compile_all(files: &[SourceFile], held: &BTreeMap<String, Entry>) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    for (id, yaml) in BUILTINS {
        if let Some(entry) = compile_one(yaml, Source::Builtin, Some(id), held) {
            out.insert(entry.id.clone(), entry);
        }
    }
    // A mounted file replaces a built-in of the same id.
    for (path, yaml) in files {
        match compile_one(yaml, Source::Mounted, None, held) {
            Some(entry) => {
                out.insert(entry.id.clone(), entry);
            }
            None => tracing::warn!(%path, "no dashboard id; the file is ignored"),
        }
    }
    out
}

fn compile_one(
    yaml: &str,
    source: Source,
    known_id: Option<&str>,
    held: &BTreeMap<String, Entry>,
) -> Option<Entry> {
    let (tree, diagnostics) = dsl::compile_with_diagnostics(yaml);
    let error = diagnostics.into_iter().find(|d| d.is_error());

    if let Some(tree) = tree {
        return Some(Entry {
            id: tree.id.clone(),
            title: tree.title.clone(),
            hash: tree.hash.clone(),
            schema: tree.schema,
            source,
            status: Status::Ok,
            tree_json: Some(dsl::to_json(&tree)),
            error: None,
        });
    }

    let id = known_id.map(str::to_string).or_else(|| declared_id(yaml))?;
    // A bad merge must not remove a working dashboard.
    match held.get(&id) {
        Some(good) if good.tree_json.is_some() => Some(Entry {
            status: Status::Stale,
            source,
            error,
            ..good.clone()
        }),
        _ => Some(Entry {
            id,
            title: String::new(),
            hash: String::new(),
            schema: 0,
            source,
            status: Status::Failed,
            tree_json: None,
            error,
        }),
    }
}

/// Read only to attribute a failure; a compiled document has an authoritative id.
fn declared_id(yaml: &str) -> Option<String> {
    yaml.lines()
        .find_map(|line| line.strip_prefix("id:"))
        .map(|rest| rest.trim().trim_matches(['"', '\''].as_ref()).to_string())
        .filter(|id| !id.is_empty())
}

fn entry_to_pb(entry: &Entry) -> pb::DashboardEntry {
    pb::DashboardEntry {
        id: entry.id.clone(),
        title: entry.title.clone(),
        hash: entry.hash.clone(),
        source: match entry.source {
            Source::Builtin => pb::DashboardSource::Builtin as i32,
            Source::Mounted => pb::DashboardSource::Mounted as i32,
        },
        schema: entry.schema,
        status: match entry.status {
            Status::Ok => pb::DashboardStatus::Ok as i32,
            Status::Stale => pb::DashboardStatus::Stale as i32,
            Status::Failed => pb::DashboardStatus::Failed as i32,
        },
        error: entry.error.as_ref().map(|d| pb::DashboardDiagnostic {
            code: d.code.to_string(),
            message: d.message.clone(),
            path: d.path.clone(),
        }),
    }
}

pub fn dir_from_env() -> PathBuf {
    std::env::var("VEDAVID_DASHBOARD_DIR")
        .unwrap_or_else(|_| DEFAULT_DIR.to_string())
        .into()
}

pub fn cluster_label_from_env() -> String {
    std::env::var("VEDAVID_CLUSTER_LABEL").unwrap_or_default()
}

impl Dashboards {
    pub fn path(&self) -> &Path {
        &self.dir
    }
}
