//! Runs the service against the Prometheus at VEDAVID_TEST_PROMETHEUS; see
//! README.md for what happens when it is unset.

use vedavid_connector::pb::connector_client::ConnectorClient;
use vedavid_connector::pb::connector_server::ConnectorServer;
use vedavid_connector::pb::*;
use vedavid_connector::prom::Prometheus;
use vedavid_connector::service::QueryService;

fn upstream() -> Option<String> {
    std::env::var("VEDAVID_TEST_PROMETHEUS").ok()
}

/// Serves on an ephemeral port and returns a client wired to it.
async fn serve(base: &str) -> ConnectorClient<tonic::transport::Channel> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let svc = ConnectorServer::new(QueryService::new(
        Prometheus::new(base),
        std::sync::Arc::new(vedavid_connector::dashboards::Dashboards::new(
            vedavid_connector::dashboards::Config::default(),
        )),
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
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }
    panic!("service never accepted a connection");
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

macro_rules! prom_test {
    ($name:ident, $client:ident, $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some(base) = upstream() else {
                eprintln!("skipped: set VEDAVID_TEST_PROMETHEUS");
                return;
            };
            let mut $client = serve(&base).await;
            $body
        }
    };
}

prom_test!(an_instant_query_returns_a_real_sample, client, {
    let r = client
        .instant_query(InstantQueryRequest {
            query: "up".into(),
            time_ms: 0,
            timeout_ms: 5000,
        })
        .await
        .expect("instant query must succeed")
        .into_inner();

    assert_eq!(r.r#type, ResultType::Vector as i32);
    assert!(
        !r.series.is_empty(),
        "prometheus scrapes itself, so up exists"
    );
    let s = &r.series[0];
    assert_eq!(s.samples.len(), 1, "a vector carries one sample per series");
    assert_eq!(s.samples[0].v, 1.0, "up is 1 for a healthy target");
    assert!(s.samples[0].t_ms > 1_700_000_000_000, "timestamp is in ms");
    assert!(
        s.labels
            .iter()
            .any(|l| l.name == "__name__" && l.value == "up"),
        "labels must survive"
    );
});

prom_test!(a_range_query_reports_the_step_it_chose, client, {
    let end = now_ms();
    let r = client
        .range_query(RangeQueryRequest {
            query: "up".into(),
            start_ms: end - 60_000,
            end_ms: end,
            max_points: 4,
            timeout_ms: 5000,
        })
        .await
        .expect("range query must succeed")
        .into_inner();

    assert_eq!(r.r#type, ResultType::Matrix as i32);
    let d = r
        .downsample
        .expect("a range query must report downsampling");
    assert_eq!(d.step_ms, 15_000, "60s over a 4-point budget is a 15s step");
    assert!(d.points_returned > 0);
    assert_eq!(
        d.points_returned,
        r.series.iter().map(|s| s.samples.len()).sum::<usize>() as i32
    );
});

prom_test!(a_malformed_promql_becomes_invalid_argument, client, {
    let e = client
        .instant_query(InstantQueryRequest {
            query: "up{".into(),
            time_ms: 0,
            timeout_ms: 5000,
        })
        .await
        .expect_err("a parse error must not succeed");

    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    assert!(
        e.message().contains("parse error"),
        "the upstream message must reach the caller: {}",
        e.message()
    );
});

prom_test!(labels_label_values_and_series_all_answer, client, {
    let end = now_ms();
    let names = client
        .labels(LabelsRequest {
            start_ms: end - 300_000,
            end_ms: end,
            r#match: vec![],
        })
        .await
        .unwrap()
        .into_inner()
        .names;
    assert!(names.contains(&"job".to_string()), "got {names:?}");

    let values = client
        .label_values(LabelValuesRequest {
            label: "job".into(),
            start_ms: end - 300_000,
            end_ms: end,
            r#match: vec![],
        })
        .await
        .unwrap()
        .into_inner()
        .values;
    assert!(!values.is_empty());

    let series = client
        .series(SeriesRequest {
            r#match: vec!["up".into()],
            start_ms: end - 300_000,
            end_ms: end,
        })
        .await
        .unwrap()
        .into_inner()
        .series;
    assert!(!series.is_empty());
    assert!(
        series[0].samples.is_empty(),
        "series returns labels only, per the contract"
    );
});

/// Port 1 has no listener, so this exercises the unreachable-upstream path.
#[tokio::test]
async fn an_unreachable_prometheus_is_unavailable() {
    let mut client = serve("http://127.0.0.1:1").await;
    let e = client
        .instant_query(InstantQueryRequest {
            query: "up".into(),
            time_ms: 0,
            timeout_ms: 2000,
        })
        .await
        .expect_err("must not succeed");
    assert_eq!(e.code(), tonic::Code::Unavailable, "{}", e.message());
}
