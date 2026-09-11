//! The rules from the rendering spec's discovery section, exercised against a
//! real directory.

use std::path::PathBuf;
use vedavid_connector::dashboards::{Config, Dashboards, Limits};
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
        self.store_with(|_| {})
    }

    fn store_with(&self, tweak: impl FnOnce(&mut Config)) -> Dashboards {
        let mut config = Config {
            path: self.0.clone(),
            cluster_label: "test-cluster".into(),
            sources: vec!["vedavid-dashboards".into()],
            ..Config::default()
        };
        tweak(&mut config);
        Dashboards::new(config)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn entry<'a>(inv: &'a pb::DashboardInventory, id: &str) -> &'a pb::DashboardEntry {
    inv.entries
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
    let store = Dashboards::new(Config {
        path: "/nonexistent/vedavid/dashboards".into(),
        sources: vec!["vedavid-dashboards".into()],
        ..Config::default()
    });
    store.scan();
    let inv = store.inventory();
    assert!(!inv.entries.is_empty(), "built-ins should still load");
    assert!(inv
        .entries
        .iter()
        .all(|d| d.source == pb::DashboardSource::Builtin as i32));
    assert!(inv
        .entries
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

    let doc = store.render_tree("team-api").expect("a held render tree");
    assert_eq!(doc.hash, e.hash);
    let json: serde_json::Value = serde_json::from_slice(&doc.json).expect("valid JSON");
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
        inv.entries.iter().filter(|d| d.id == "node-health").count(),
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
    assert_eq!(e.diagnostics[0].code, "E-010");
    assert!(store.render_tree("team-api").is_ok());
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
    assert_eq!(e.diagnostics[0].code, "E-010");
    assert!(store.render_tree("team-api").is_err());
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
    assert!(store.render_tree("team-api").is_err());
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
        inv.entries.iter().all(|d| d.id != "junk"),
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
        .entries
        .iter()
        .filter(|d| d.status == pb::DashboardStatus::Ok as i32)
        .count();
    assert_eq!(ok, inv.entries.len() - 1);
}

/// Filename order carries no meaning here.
#[test]
fn two_files_declaring_one_id_admit_neither_and_name_both() {
    let dir = Dir::new("duplicate");
    dir.write("a.yaml", GOOD);
    dir.write("b.yaml", &GOOD.replace("Team API", "Team API elsewhere"));
    let store = dir.store();
    store.scan();

    let inv = store.inventory();
    let e = entry(&inv, "team-api");
    assert_eq!(e.status, pb::DashboardStatus::Failed as i32);
    let message = &e.diagnostics[0].message;
    assert!(message.contains("a.yaml"), "{message}");
    assert!(message.contains("b.yaml"), "{message}");
    assert!(store.render_tree("team-api").is_err());
}

/// The counters are the only signal for a chart and generator name mismatch.
#[test]
fn counters_expose_a_mount_that_delivered_nothing() {
    let dir = Dir::new("counters");
    let store = dir.store();
    store.scan();

    let inv = store.inventory();
    assert_eq!(inv.mounted_sources_seen, 1, "one source is configured");
    assert_eq!(inv.files_scanned, 0, "and it delivered nothing");
    assert!(!inv.entries.is_empty(), "built-ins are still served");

    dir.write("team-api.yaml", GOOD);
    store.scan();
    assert_eq!(store.inventory().files_scanned, 1);
}

#[test]
fn a_file_larger_than_its_limit_is_rejected_without_disturbing_the_others() {
    let dir = Dir::new("file-size");
    dir.write("team-api.yaml", GOOD);
    dir.write("huge.yaml", &format!("{GOOD}# {}\n", "x".repeat(4096)));
    let store = dir.store_with(|c| {
        c.limits = Limits {
            max_dashboard_bytes: 512,
            ..Limits::default()
        }
    });
    store.scan();

    let inv = store.inventory();
    assert_eq!(
        entry(&inv, "team-api").status,
        pb::DashboardStatus::Ok as i32
    );
    assert_eq!(inv.files_scanned, 1, "the oversized file is not scanned");
}

/// A projection far larger than expected must not empty a working inventory.
#[test]
fn a_directory_over_its_limit_retains_the_previous_inventory() {
    let dir = Dir::new("dir-size");
    dir.write("team-api.yaml", GOOD);
    let store = dir.store_with(|c| {
        c.limits = Limits {
            max_directory_bytes: 100_000,
            ..Limits::default()
        }
    });
    store.scan();
    let before = store.inventory().generation;
    assert!(store.render_tree("team-api").is_ok());

    dir.write("bulk.yaml", &"# padding\n".repeat(20_000));
    store.scan();

    let inv = store.inventory();
    assert_eq!(inv.generation, before, "the scan aborted");
    assert!(
        store.render_tree("team-api").is_ok(),
        "the good tree survives"
    );
}

#[test]
fn more_dashboards_than_the_limit_allows_are_dropped_by_sorted_id() {
    let dir = Dir::new("count-limit");
    for n in 0..6 {
        dir.write(
            &format!("d{n}.yaml"),
            &GOOD.replace("id: team-api", &format!("id: dash-{n}")),
        );
    }
    let store = dir.store_with(|c| {
        c.builtins = false;
        c.limits = Limits {
            max_dashboards: 3,
            ..Limits::default()
        };
    });
    store.scan();

    let inv = store.inventory();
    assert_eq!(inv.entries.len(), 3);
    let ids: Vec<&str> = inv.entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["dash-0", "dash-1", "dash-2"]);
}

#[test]
fn built_ins_can_be_turned_off_entirely() {
    let dir = Dir::new("no-builtins");
    dir.write("team-api.yaml", GOOD);
    let store = dir.store_with(|c| c.builtins = false);
    store.scan();

    let inv = store.inventory();
    assert_eq!(inv.entries.len(), 1);
    assert_eq!(inv.entries[0].id, "team-api");
}

/// A connector that could serve nothing at all is a misconfiguration.
#[test]
fn a_configuration_that_can_serve_nothing_is_refused() {
    let bad = Config {
        builtins: false,
        sources: vec![],
        ..Config::default()
    };
    assert!(bad.validate().is_err());

    let first_install = Config {
        builtins: true,
        sources: vec![],
        ..Config::default()
    };
    assert!(
        first_install.validate().is_ok(),
        "no dashboards yet is fine"
    );

    let disabled = Config {
        enabled: false,
        builtins: false,
        sources: vec![],
        ..Config::default()
    };
    assert!(disabled.validate().is_ok());
}

#[test]
fn the_cluster_label_falls_back_to_something_identifying() {
    let dir = Dir::new("fallback-label");
    let store = dir.store_with(|c| c.cluster_label = String::new());
    store.scan();
    assert_eq!(store.inventory().cluster_label, "");

    store.set_fallback_label("22222222");
    assert_eq!(store.inventory().cluster_label, "22222222");
}

/// Built-ins are compiled by build.rs, so their trees are present without any
/// YAML being parsed at startup.
#[test]
fn built_in_trees_are_embedded_already_compiled() {
    let store = Dashboards::new(Config {
        path: "/nonexistent".into(),
        ..Config::default()
    });
    store.scan();
    let tree = store
        .render_tree("cluster-health")
        .expect("a built-in tree");
    assert!(!tree.hash.is_empty());
    assert_eq!(tree.schema, 1);
    let json: serde_json::Value = serde_json::from_slice(&tree.json).unwrap();
    assert_eq!(json["id"], "cluster-health");
}
