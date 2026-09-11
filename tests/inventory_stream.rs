//! The inventory as the relay actually receives it: over gRPC, on the stream
//! the relay holds open.

use std::sync::Arc;
use std::time::Duration;
use vedavid_connector::dashboards::{Config, Dashboards};
use vedavid_connector::pb::connector_client::ConnectorClient;
use vedavid_connector::pb::connector_server::ConnectorServer;
use vedavid_connector::pb::{connector_event::Event, EventsRequest, GetRenderTreeRequest};
use vedavid_connector::prom::Prometheus;
use vedavid_connector::service::QueryService;

async fn serve(dashboards: Arc<Dashboards>) -> ConnectorClient<tonic::transport::Channel> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let svc = ConnectorServer::new(QueryService::new(
        Prometheus::new("http://127.0.0.1:1"),
        dashboards,
    ));
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    for _ in 0..50 {
        if let Ok(c) = ConnectorClient::connect(format!("http://{addr}")).await {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    panic!("service never accepted a connection");
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("vedavid-stream-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
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

#[tokio::test]
async fn the_inventory_arrives_without_being_asked_for() {
    let dir = temp_dir("announce");
    std::fs::write(dir.join("team-api.yaml"), GOOD).unwrap();
    let store = Arc::new(Dashboards::new(Config {
        path: dir.clone(),
        cluster_label: "prod-us-east".into(),
        sources: vec!["vedavid-dashboards".into()],
        ..Config::default()
    }));
    store.scan();

    let mut client = serve(store.clone()).await;
    let mut stream = client
        .events(EventsRequest::default())
        .await
        .expect("the events stream opens")
        .into_inner();

    let event = tokio::time::timeout(Duration::from_secs(5), stream.message())
        .await
        .expect("an event within five seconds")
        .expect("no transport error")
        .expect("an event, not end of stream");

    let Some(Event::Inventory(inv)) = event.event else {
        panic!(
            "the first event should be the inventory, got {:?}",
            event.event
        );
    };
    assert_eq!(inv.cluster_label, "prod-us-east");
    assert!(inv.generation > 0);
    assert!(inv.entries.iter().any(|d| d.id == "team-api"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_change_is_pushed_to_a_stream_already_open() {
    let dir = temp_dir("push");
    let store = Arc::new(Dashboards::new(Config {
        path: dir.clone(),
        cluster_label: "prod".into(),
        ..Config::default()
    }));
    store.scan();

    let mut client = serve(store.clone()).await;
    let mut stream = client
        .events(EventsRequest::default())
        .await
        .unwrap()
        .into_inner();

    let first = stream.message().await.unwrap().unwrap();
    let Some(Event::Inventory(before)) = first.event else {
        panic!("expected an inventory");
    };
    assert!(!before.entries.iter().any(|d| d.id == "team-api"));

    std::fs::write(dir.join("team-api.yaml"), GOOD).unwrap();
    store.scan();

    let second = tokio::time::timeout(Duration::from_secs(5), stream.message())
        .await
        .expect("a second event")
        .unwrap()
        .unwrap();
    let Some(Event::Inventory(after)) = second.event else {
        panic!("expected an inventory");
    };
    assert!(after.generation > before.generation);
    assert!(after.entries.iter().any(|d| d.id == "team-api"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_render_tree_can_be_fetched_and_a_missing_one_is_not_found() {
    let dir = temp_dir("document");
    std::fs::write(dir.join("team-api.yaml"), GOOD).unwrap();
    let store = Arc::new(Dashboards::new(Config {
        path: dir.clone(),
        ..Config::default()
    }));
    store.scan();

    let mut client = serve(store).await;
    let doc = client
        .get_render_tree(GetRenderTreeRequest {
            id: "team-api".into(),
        })
        .await
        .expect("the document is served")
        .into_inner();
    assert_eq!(doc.id, "team-api");
    assert_eq!(doc.schema, 1);
    let tree: serde_json::Value = serde_json::from_slice(&doc.json).unwrap();
    assert_eq!(tree["hash"], doc.hash);

    let missing = client
        .get_render_tree(GetRenderTreeRequest { id: "nope".into() })
        .await
        .expect_err("a missing dashboard is an error");
    assert_eq!(missing.code(), tonic::Code::NotFound);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A dashboard that never compiled is a different condition from one that does
/// not exist, and the app answers them differently.
#[tokio::test]
async fn a_never_compiled_dashboard_is_a_precondition_failure_not_a_missing_one() {
    let dir = temp_dir("precondition");
    std::fs::write(
        dir.join("broken.yaml"),
        "version: 1\nid: broken-one\ntitle: Broken\nelements:\n  - type: stat\n    title: X\n    query: sum(up{c=\"$nope\"})\n    unit: count\n",
    )
    .unwrap();
    let store = Arc::new(Dashboards::new(Config {
        path: dir.clone(),
        ..Config::default()
    }));
    store.scan();

    let mut client = serve(store).await;
    let failed = client
        .get_render_tree(GetRenderTreeRequest {
            id: "broken-one".into(),
        })
        .await
        .expect_err("a failed dashboard has no tree");
    assert_eq!(failed.code(), tonic::Code::FailedPrecondition);

    let unknown = client
        .get_render_tree(GetRenderTreeRequest {
            id: "never-heard-of-it".into(),
        })
        .await
        .expect_err("an unknown dashboard");
    assert_eq!(unknown.code(), tonic::Code::NotFound);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A poll this long means only a directory event can explain the update.
#[tokio::test]
async fn a_directory_event_triggers_a_rescan_without_waiting_for_the_poll() {
    let dir = temp_dir("watch");
    let store = Arc::new(Dashboards::new(Config {
        path: dir.clone(),
        poll: Duration::from_secs(3600),
        ..Config::default()
    }));
    store.scan();
    let before = store.inventory().generation;

    tokio::spawn(vedavid_connector::dashboards::watch(
        store.clone(),
        Duration::from_secs(3600),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(dir.join("team-api.yaml"), GOOD).unwrap();

    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if store.inventory().generation > before {
            assert!(store.inventory().entries.iter().any(|e| e.id == "team-api"));
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    }
    panic!("the watcher never noticed the new file");
}
