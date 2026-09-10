//! The rules from the rendering spec's discovery section, exercised against a
//! real directory.

use std::path::PathBuf;
use vedavid_connector::dashboards::Dashboards;
use vedavid_connector::pb;

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("vedavid-dash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a temporary directory");
        Self(path)
    }

    fn write(&self, file: &str, body: &str) {
        std::fs::write(self.0.join(file), body).expect("writing a dashboard");
    }

    fn remove(&self, file: &str) {
        let _ = std::fs::remove_file(self.0.join(file));
    }

    fn store(&self) -> Dashboards {
        Dashboards::new(self.0.clone(), "test-cluster")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn entry<'a>(inv: &'a pb::DashboardInventory, id: &str) -> &'a pb::DashboardEntry {
    inv.dashboards
        .iter()
        .find(|d| d.id == id)
        .unwrap_or_else(|| panic!("{id} is not in the inventory"))
}

const GOOD: &str = "\
version: 1
id: team-api
title: Team API
elements:
  - type: stat
    title: Requests
    query: sum(rate(http_requests_total[5m]))
    unit: requests_per_sec
";

/// Same id, different title, so an override is distinguishable.
const OVERRIDE: &str = "\
version: 1
id: node-health
title: Node health, ours
elements:
  - type: stat
    title: Nodes
    query: count(kube_node_info)
    unit: count
";

const BROKEN: &str = "\
version: 1
id: team-api
title: Team API
elements:
  - type: stat
    title: Requests
    query: sum(rate(http_requests_total{cluster=\"$cluster\"}[5m]))
    unit: requests_per_sec
";

#[test]
fn built_ins_are_served_with_no_mounted_directory_at_all() {
    let store = Dashboards::new("/nonexistent/vedavid/dashboards", "");
    store.scan();
    let inv = store.inventory();
    assert!(!inv.dashboards.is_empty(), "built-ins should still load");
    assert!(inv
        .dashboards
        .iter()
        .all(|d| d.source == pb::DashboardSource::Builtin as i32));
    assert!(inv
        .dashboards
        .iter()
        .all(|d| d.status == pb::DashboardStatus::Ok as i32));
}

#[test]
fn a_mounted_file_is_added_and_its_tree_is_held() {
    let dir = Dir::new("mounted");
    dir.write("team-api.yaml", GOOD);
    let store = dir.store();
    store.scan();

    let inv = store.inventory();
    let e = entry(&inv, "team-api");
    assert_eq!(e.title, "Team API");
    assert_eq!(e.source, pb::DashboardSource::Mounted as i32);
    assert_eq!(e.schema, 1);
    assert!(!e.hash.is_empty());

    let doc = store.document("team-api").expect("a held render tree");
    assert_eq!(doc.hash, e.hash);
    let json: serde_json::Value = serde_json::from_slice(&doc.render_tree).expect("valid JSON");
    assert_eq!(json["id"], "team-api");
    assert_eq!(json["sections"][0]["elements"][0]["type"], "stat");
}

#[test]
fn a_mounted_file_replaces_a_built_in_of_the_same_id() {
    let dir = Dir::new("override");
    dir.write("node-health.yaml", OVERRIDE);
    let store = dir.store();
    store.scan();

    let inv = store.inventory();
    let e = entry(&inv, "node-health");
    assert_eq!(e.title, "Node health, ours");
    assert_eq!(e.source, pb::DashboardSource::Mounted as i32);
    assert_eq!(
        inv.dashboards
            .iter()
            .filter(|d| d.id == "node-health")
            .count(),
        1,
        "the built-in is replaced, not duplicated"
    );
}

/// Requirement 4: a bad merge must not remove a working dashboard.
#[test]
fn a_file_that_stops_compiling_keeps_serving_its_last_good_tree() {
    let dir = Dir::new("stale");
    dir.write("team-api.yaml", GOOD);
    let store = dir.store();
    store.scan();
    let good_hash = entry(&store.inventory(), "team-api").hash.clone();

    dir.write("team-api.yaml", BROKEN);
    store.scan();

    let inv = store.inventory();
    let e = entry(&inv, "team-api");
    assert_eq!(e.status, pb::DashboardStatus::Stale as i32);
    assert_eq!(e.hash, good_hash, "the last good tree is still served");
    assert_eq!(e.error.as_ref().expect("a diagnostic").code, "E-010");
    assert!(store.document("team-api").is_some());
}

#[test]
fn a_file_that_never_compiled_is_reported_as_failed_with_no_tree() {
    let dir = Dir::new("failed");
    dir.write("team-api.yaml", BROKEN);
    let store = dir.store();
    store.scan();

    let inv = store.inventory();
    let e = entry(&inv, "team-api");
    assert_eq!(e.status, pb::DashboardStatus::Failed as i32);
    assert_eq!(e.error.as_ref().expect("a diagnostic").code, "E-010");
    assert!(store.document("team-api").is_none());
}

/// Failure granularity is per dashboard.
#[test]
fn one_broken_file_does_not_disturb_the_others() {
    let dir = Dir::new("granularity");
    dir.write("team-api.yaml", BROKEN);
    dir.write("node-health.yaml", OVERRIDE);
    let store = dir.store();
    store.scan();

    let inv = store.inventory();
    assert_eq!(
        entry(&inv, "team-api").status,
        pb::DashboardStatus::Failed as i32
    );
    assert_eq!(
        entry(&inv, "node-health").status,
        pb::DashboardStatus::Ok as i32
    );
    assert_eq!(
        entry(&inv, "cluster-health").status,
        pb::DashboardStatus::Ok as i32
    );
}

/// The relay detects change by comparing one integer, so it must not move
/// when nothing changed.
#[test]
fn the_generation_advances_only_on_a_real_change() {
    let dir = Dir::new("generation");
    dir.write("team-api.yaml", GOOD);
    let store = dir.store();

    store.scan();
    let first = store.inventory().generation;
    store.scan();
    assert_eq!(store.inventory().generation, first, "a no-op rescan");

    dir.write(
        "team-api.yaml",
        GOOD.replace("Team API", "Team API v2").as_str(),
    );
    store.scan();
    let second = store.inventory().generation;
    assert!(second > first, "a changed title is a change");

    dir.remove("team-api.yaml");
    store.scan();
    assert!(
        store.inventory().generation > second,
        "a removed file is a change"
    );
    assert!(store.document("team-api").is_none());
}

#[test]
fn the_cluster_label_is_announced() {
    let dir = Dir::new("label");
    let store = dir.store();
    store.scan();
    assert_eq!(store.inventory().cluster_label, "test-cluster");
}

#[test]
fn a_file_too_malformed_to_name_itself_is_ignored_rather_than_guessed_at() {
    let dir = Dir::new("nameless");
    dir.write("junk.yaml", "this: is not: valid yaml: at all\n  - [\n");
    dir.write("team-api.yaml", GOOD);
    let store = dir.store();
    store.scan();

    let inv = store.inventory();
    assert_eq!(
        entry(&inv, "team-api").status,
        pb::DashboardStatus::Ok as i32
    );
    assert!(
        inv.dashboards.iter().all(|d| d.id != "junk"),
        "the filename is not used as an id"
    );
}

#[test]
fn subscribers_are_woken_on_change_and_not_otherwise() {
    let dir = Dir::new("wake");
    dir.write("team-api.yaml", GOOD);
    let store = dir.store();
    store.scan();

    let mut rx = store.subscribe();
    assert!(rx.try_recv().is_err(), "no event before a change");

    dir.write("team-api.yaml", OVERRIDE);
    store.scan();
    assert!(rx.try_recv().is_ok(), "a change wakes the stream");
}

#[test]
fn status_reports_what_the_app_needs_to_say_three_of_four_loaded() {
    let dir = Dir::new("counts");
    dir.write("team-api.yaml", BROKEN);
    let store = dir.store();
    store.scan();

    let inv = store.inventory();
    let ok = inv
        .dashboards
        .iter()
        .filter(|d| d.status == pb::DashboardStatus::Ok as i32)
        .count();
    assert_eq!(ok, inv.dashboards.len() - 1);
}
