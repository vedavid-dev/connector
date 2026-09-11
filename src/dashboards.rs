//! Dashboards read from a mounted directory, compiled and held in memory.

use crate::pb;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;
use vedavid_dashboard_dsl as dsl;

pub(crate) struct Builtin {
    pub id: &'static str,
    pub title: &'static str,
    pub hash: &'static str,
    pub schema: u32,
    pub json: &'static str,
}

// Compiled by build.rs, so a built-in that stops compiling breaks the build.
include!(concat!(env!("OUT_DIR"), "/builtins.rs"));

pub const DEFAULT_DIR: &str = "/etc/vedavid/dashboards";
pub const DEFAULT_POLL: Duration = Duration::from_secs(30);
const DEBOUNCE: Duration = Duration::from_millis(500);
const MAX_DIAGNOSTICS: usize = 10;

/// Enough of a file to tell a rescan that nothing on disk moved.
type Stamp = (PathBuf, SystemTime, u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_dashboards: usize,
    pub max_dashboard_bytes: u64,
    pub max_directory_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_dashboards: 100,
            max_dashboard_bytes: 256 * 1024,
            max_directory_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    pub path: PathBuf,
    pub poll: Duration,
    pub builtins: bool,
    pub cluster_label: String,
    /// Counted, never read: the directory is flat and carries no provenance.
    pub sources: Vec<String>,
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            path: DEFAULT_DIR.into(),
            poll: DEFAULT_POLL,
            builtins: true,
            cluster_label: String::new(),
            sources: Vec::new(),
            limits: Limits::default(),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let d = Config::default();
        let num = |key: &str, fallback: u64| -> u64 {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(fallback)
        };
        Config {
            enabled: std::env::var("VEDAVID_DASHBOARDS_ENABLED")
                .map(|v| v != "false")
                .unwrap_or(d.enabled),
            path: std::env::var("VEDAVID_DASHBOARD_DIR")
                .map(PathBuf::from)
                .unwrap_or(d.path),
            poll: Duration::from_secs(num("VEDAVID_DASHBOARD_POLL_SECONDS", d.poll.as_secs())),
            builtins: std::env::var("VEDAVID_DASHBOARD_BUILTINS")
                .map(|v| v != "false")
                .unwrap_or(d.builtins),
            cluster_label: std::env::var("VEDAVID_CLUSTER_LABEL").unwrap_or_default(),
            sources: std::env::var("VEDAVID_DASHBOARD_SOURCES")
                .map(|v| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or(d.sources),
            limits: Limits {
                max_dashboards: num("VEDAVID_MAX_DASHBOARDS", d.limits.max_dashboards as u64)
                    as usize,
                max_dashboard_bytes: num(
                    "VEDAVID_MAX_DASHBOARD_BYTES",
                    d.limits.max_dashboard_bytes,
                ),
                max_directory_bytes: num(
                    "VEDAVID_MAX_DIRECTORY_BYTES",
                    d.limits.max_directory_bytes,
                ),
            },
        }
    }

    /// A connector that could serve no dashboard at all is a misconfiguration,
    /// not a degraded mode.
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && !self.builtins && self.sources.is_empty() {
            return Err(
                "dashboards.enabled is true with no sources and builtins disabled, so no \
                 dashboard could ever be served"
                    .into(),
            );
        }
        Ok(())
    }
}

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
    pub diagnostics: Vec<dsl::Diagnostic>,
    pub diagnostics_truncated: u32,
}

impl Entry {
    /// What the relay compares, so an identical recompile is not a change.
    fn identity(&self) -> (&str, &str, Status, usize) {
        (&self.id, &self.hash, self.status, self.diagnostics.len())
    }
}

#[derive(Debug, Default)]
struct State {
    generation: u64,
    entries: BTreeMap<String, Entry>,
    fingerprint: Vec<Stamp>,
    files_scanned: u32,
    fallback_label: String,
}

pub struct Dashboards {
    config: Config,
    state: RwLock<State>,
    changed: broadcast::Sender<()>,
}

impl Dashboards {
    pub fn new(config: Config) -> Self {
        let (changed, _) = broadcast::channel(4);
        Self {
            config,
            state: RwLock::new(State::default()),
            changed,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn path(&self) -> &Path {
        &self.config.path
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changed.subscribe()
    }

    /// The app cannot invent a name for a cluster.
    pub fn set_fallback_label(&self, label: &str) {
        let mut state = self
            .state
            .write()
            .expect("the inventory lock is never poisoned");
        state.fallback_label = label.to_string();
    }

    pub fn inventory(&self) -> pb::DashboardInventory {
        let state = self
            .state
            .read()
            .expect("the inventory lock is never poisoned");
        let label = if self.config.cluster_label.is_empty() {
            state.fallback_label.clone()
        } else {
            self.config.cluster_label.clone()
        };
        pb::DashboardInventory {
            generation: state.generation,
            cluster_label: label,
            entries: state.entries.values().map(entry_to_pb).collect(),
            mounted_sources_seen: self.config.sources.len() as u32,
            files_scanned: state.files_scanned,
        }
    }

    pub fn render_tree(&self, id: &str) -> Result<pb::RenderTree, TreeError> {
        let state = self
            .state
            .read()
            .expect("the inventory lock is never poisoned");
        let Some(entry) = state.entries.get(id) else {
            return Err(TreeError::Unknown);
        };
        let Some(json) = entry.tree_json.as_ref() else {
            return Err(TreeError::NeverCompiled);
        };
        Ok(pb::RenderTree {
            id: entry.id.clone(),
            hash: entry.hash.clone(),
            schema: entry.schema,
            json: json.clone().into_bytes(),
        })
    }

    /// `generation` advances only when the result differs.
    pub fn scan(&self) -> bool {
        if !self.config.enabled {
            return false;
        }
        let held = {
            let state = self
                .state
                .read()
                .expect("the inventory lock is never poisoned");
            state.entries.clone()
        };
        let Some(read) = self.read_dir() else {
            tracing::warn!(
                limit = self.config.limits.max_directory_bytes,
                "the dashboard directory exceeds its size limit; keeping the previous inventory"
            );
            return false;
        };

        let files_scanned = read.files.len() as u32;
        let entries = self.assemble(&read.files, &held);

        let mut state = self
            .state
            .write()
            .expect("the inventory lock is never poisoned");
        state.fingerprint = read.fingerprint;
        state.files_scanned = files_scanned;
        let same = state.entries.len() == entries.len()
            && state
                .entries
                .values()
                .zip(entries.values())
                .all(|(a, b)| a.identity() == b.identity());
        state.entries = entries;
        if same {
            return false;
        }
        state.generation += 1;
        let generation = state.generation;
        let admitted = state.entries.len();
        let failed = state
            .entries
            .values()
            .filter(|e| e.status != Status::Ok)
            .count();
        drop(state);

        tracing::info!(
            generation,
            sources = self.config.sources.len(),
            files_scanned,
            admitted,
            failed,
            "dashboard inventory changed"
        );
        let _ = self.changed.send(());
        true
    }

    /// A ConfigMap update lands as an atomic symlink swap of the directory.
    fn read_dir(&self) -> Option<Scan> {
        let mut scan = Scan::default();
        let Ok(entries) = std::fs::read_dir(&self.config.path) else {
            tracing::debug!(dir = %self.config.path.display(), "no dashboard directory");
            return Some(scan);
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                // The projection's own `..data` and timestamped dirs are dotfiles.
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                !name.starts_with('.')
                    && matches!(
                        p.extension().and_then(|e| e.to_str()),
                        Some("yaml") | Some("yml")
                    )
            })
            .collect();
        paths.sort();

        let mut total = 0u64;
        for path in paths {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            total = total.saturating_add(meta.len());
            if total > self.config.limits.max_directory_bytes {
                return None;
            }
            if meta.len() > self.config.limits.max_dashboard_bytes {
                scan.oversized.push(path.display().to_string());
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    scan.fingerprint.push((
                        path.clone(),
                        meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                        meta.len(),
                    ));
                    scan.files.push((path.display().to_string(), text));
                }
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "unreadable"),
            }
        }
        Some(scan)
    }

    fn assemble(
        &self,
        files: &[(String, String)],
        held: &BTreeMap<String, Entry>,
    ) -> BTreeMap<String, Entry> {
        let mut out = BTreeMap::new();
        if self.config.builtins {
            for b in BUILTINS {
                out.insert(
                    b.id.to_string(),
                    Entry {
                        id: b.id.to_string(),
                        title: b.title.to_string(),
                        hash: b.hash.to_string(),
                        schema: b.schema,
                        source: Source::Builtin,
                        status: Status::Ok,
                        tree_json: Some(b.json.to_string()),
                        diagnostics: Vec::new(),
                        diagnostics_truncated: 0,
                    },
                );
            }
        }

        let mut by_id: BTreeMap<String, Vec<(&str, Compiled)>> = BTreeMap::new();
        for (path, yaml) in files {
            let compiled = compile(yaml);
            let Some(id) = compiled.id.clone() else {
                tracing::warn!(%path, "no dashboard id; the file is ignored");
                continue;
            };
            by_id.entry(id).or_default().push((path.as_str(), compiled));
        }

        for (id, mut candidates) in by_id {
            if candidates.len() > 1 {
                // Filename order is not identity, so neither file wins.
                let paths: Vec<&str> = candidates.iter().map(|(p, _)| *p).collect();
                tracing::warn!(%id, files = ?paths, "duplicate dashboard id; neither is admitted");
                out.insert(
                    id.clone(),
                    Entry {
                        id: id.clone(),
                        title: String::new(),
                        hash: String::new(),
                        schema: 0,
                        source: Source::Mounted,
                        status: Status::Failed,
                        tree_json: None,
                        diagnostics: vec![dsl::Diagnostic::error(
                            "E-100",
                            "",
                            format!(
                                "`{id}` is declared by more than one file: {}",
                                paths.join(", ")
                            ),
                        )],
                        diagnostics_truncated: 0,
                    },
                );
                continue;
            }
            let (_, compiled) = candidates.remove(0);
            out.insert(id.clone(), entry_from(id, compiled, held));
        }

        // Sorted ids make which dashboards are dropped deterministic.
        if out.len() > self.config.limits.max_dashboards {
            let dropped = out.len() - self.config.limits.max_dashboards;
            tracing::warn!(
                dropped,
                limit = self.config.limits.max_dashboards,
                "more dashboards than the limit allows"
            );
            let keep: Vec<String> = out
                .keys()
                .take(self.config.limits.max_dashboards)
                .cloned()
                .collect();
            out.retain(|k, _| keep.contains(k));
        }
        out
    }
}

#[derive(Debug)]
pub enum TreeError {
    Unknown,
    NeverCompiled,
}

#[derive(Default)]
struct Scan {
    files: Vec<(String, String)>,
    fingerprint: Vec<Stamp>,
    oversized: Vec<String>,
}

struct Compiled {
    id: Option<String>,
    tree: Option<dsl::RenderTree>,
    diagnostics: Vec<dsl::Diagnostic>,
}

fn compile(yaml: &str) -> Compiled {
    let (tree, diagnostics) = dsl::compile_with_diagnostics(yaml);
    let id = tree
        .as_ref()
        .map(|t| t.id.clone())
        .or_else(|| declared_id(yaml));
    Compiled {
        id,
        tree,
        diagnostics: diagnostics.into_iter().filter(|d| d.is_error()).collect(),
    }
}

fn entry_from(id: String, compiled: Compiled, held: &BTreeMap<String, Entry>) -> Entry {
    let truncated = compiled.diagnostics.len().saturating_sub(MAX_DIAGNOSTICS) as u32;
    let diagnostics: Vec<dsl::Diagnostic> = compiled
        .diagnostics
        .into_iter()
        .take(MAX_DIAGNOSTICS)
        .collect();

    if let Some(tree) = compiled.tree {
        return Entry {
            id: tree.id.clone(),
            title: tree.title.clone(),
            hash: tree.hash.clone(),
            schema: tree.schema,
            source: Source::Mounted,
            status: Status::Ok,
            tree_json: Some(dsl::to_json(&tree)),
            diagnostics: Vec::new(),
            diagnostics_truncated: 0,
        };
    }

    // A bad merge must not remove a working dashboard.
    match held.get(&id) {
        Some(good) if good.tree_json.is_some() => Entry {
            status: Status::Stale,
            source: Source::Mounted,
            diagnostics,
            diagnostics_truncated: truncated,
            ..good.clone()
        },
        _ => Entry {
            id,
            title: String::new(),
            hash: String::new(),
            schema: 0,
            source: Source::Mounted,
            status: Status::Failed,
            tree_json: None,
            diagnostics,
            diagnostics_truncated: truncated,
        },
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
        diagnostics: entry
            .diagnostics
            .iter()
            .map(|d| pb::DashboardDiagnostic {
                code: d.code.to_string(),
                message: d.message.clone(),
                path: d.path.clone(),
            })
            .collect(),
        diagnostics_truncated: entry.diagnostics_truncated,
    }
}

/// The poll is a backstop: a missed event is a dashboard that never updates.
pub async fn watch(store: std::sync::Arc<Dashboards>, poll: Duration) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(8);
    let watcher = spawn_watcher(store.path(), tx);
    if watcher.is_none() {
        tracing::info!(
            interval = poll.as_secs(),
            "watching unavailable; polling only"
        );
    }

    loop {
        let event = tokio::select! {
            _ = tokio::time::sleep(poll) => false,
            received = rx.recv() => {
                if received.is_none() {
                    tokio::time::sleep(poll).await;
                }
                true
            }
        };
        // One symlink swap raises several events; coalesce them into one scan.
        if event {
            tokio::time::sleep(DEBOUNCE).await;
            while rx.try_recv().is_ok() {}
        }
        let store = store.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || store.scan()).await {
            tracing::warn!(error = %e, "dashboard scan panicked");
        }
    }
}

fn spawn_watcher(
    dir: &Path,
    tx: tokio::sync::mpsc::Sender<()>,
) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.try_send(());
        }
    })
    .map_err(|e| tracing::warn!(error = %e, "cannot create a directory watcher"))
    .ok()?;
    watcher
        .watch(dir, notify::RecursiveMode::NonRecursive)
        .map_err(|e| tracing::warn!(dir = %dir.display(), error = %e, "cannot watch the directory"))
        .ok()?;
    Some(watcher)
}
