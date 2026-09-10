//! The inventory as the relay actually receives it: over gRPC, on the stream
//! the relay holds open.

use std::sync::Arc;
use std::time::Duration;
use vedavid_connector::dashboards::Dashboards;
use vedavid_connector::pb::connector_client::ConnectorClient;
use vedavid_connector::pb::connector_server::ConnectorServer;
use vedavid_connector::pb::{connector_event::Event, DashboardDocumentRequest, EventsRequest};
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
    let store = Arc::new(Dashboards::new(dir.clone(), "prod-us-east"));
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
    assert!(inv.dashboards.iter().any(|d| d.id == "team-api"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_change_is_pushed_to_a_stream_already_open() {
    let dir = temp_dir("push");
    let store = Arc::new(Dashboards::new(dir.clone(), "prod"));
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
    assert!(!before.dashboards.iter().any(|d| d.id == "team-api"));

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
    assert!(after.dashboards.iter().any(|d| d.id == "team-api"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_render_tree_can_be_fetched_and_a_missing_one_is_not_found() {
    let dir = temp_dir("document");
    std::fs::write(dir.join("team-api.yaml"), GOOD).unwrap();
    let store = Arc::new(Dashboards::new(dir.clone(), ""));
    store.scan();

    let mut client = serve(store).await;
    let doc = client
        .dashboard_document(DashboardDocumentRequest {
            id: "team-api".into(),
        })
        .await
        .expect("the document is served")
        .into_inner();
    assert_eq!(doc.id, "team-api");
    assert_eq!(doc.schema, 1);
    let tree: serde_json::Value = serde_json::from_slice(&doc.render_tree).unwrap();
    assert_eq!(tree["hash"], doc.hash);

    let missing = client
        .dashboard_document(DashboardDocumentRequest { id: "nope".into() })
        .await
        .expect_err("a missing dashboard is an error");
    assert_eq!(missing.code(), tonic::Code::NotFound);
    let _ = std::fs::remove_dir_all(&dir);
}
