use std::sync::Arc;
use tonic::transport::Server;
use vedavid_connector::dashboards::{self, Dashboards};
use vedavid_connector::enrol::{self, Bootstrap, Identity};
use vedavid_connector::pb::connector_server::ConnectorServer;
use vedavid_connector::pb::ConnectorBuild;
use vedavid_connector::prom::Prometheus;
use vedavid_connector::service::QueryService;
use vedavid_connector::tunnel::{backoff, one_connection};

const DEFAULT_LISTEN: &str = "127.0.0.1:50051";
const DEFAULT_PROMETHEUS: &str = "http://127.0.0.1:9090";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn build() -> ConnectorBuild {
    ConnectorBuild {
        version: env!("CARGO_PKG_VERSION").to_string(),
        git_sha: option_env!("VEDAVID_GIT_SHA")
            .unwrap_or("unknown")
            .to_string(),
        platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
        k8s_version: std::env::var("KUBERNETES_SERVICE_HOST")
            .map(|_| String::new())
            .unwrap_or_default(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let upstream = env_or("VEDAVID_PROMETHEUS_URL", DEFAULT_PROMETHEUS);

    // Built once, so reconnecting to the relay does not recompile anything.
    let config = dashboards::Config::from_env();
    if let Err(why) = config.validate() {
        return Err(why.into());
    }
    if config.cluster_label.is_empty() {
        tracing::warn!("no clusterLabel is set; the app cannot name this cluster");
    }
    let poll = config.poll;
    let dir = config.path.clone();
    let dashboards = Arc::new(Dashboards::new(config));
    dashboards.scan();
    tracing::info!(
        dir = %dir.display(),
        dashboards = dashboards.inventory().entries.len(),
        "dashboards compiled"
    );
    tokio::spawn(dashboards::watch(dashboards.clone(), poll));

    match std::env::var("VEDAVID_RELAY_ADDR") {
        Ok(relay) => tunnel_forever(&relay, &upstream, dashboards).await,
        Err(_) => listen_locally(&upstream, dashboards).await,
    }
}

/// Enrol, dial, serve, repeat. A disconnection is routine: every relay deploy
/// causes one.
async fn tunnel_forever(
    relay: &str,
    upstream: &str,
    dashboards: Arc<Dashboards>,
) -> Result<(), Box<dyn std::error::Error>> {
    let server_name = env_or(
        "VEDAVID_RELAY_SERVER_NAME",
        relay.split(':').next().unwrap_or(relay),
    );
    let roots = std::fs::read_to_string(std::env::var("VEDAVID_RELAY_CA")?)?;
    let token_file = std::env::var("VEDAVID_ENROLMENT_TOKEN_FILE")?;
    // Refusing to start makes a missing credential visible as a crash loop.
    read_token(&token_file)?;

    let mut identity: Option<Identity> = None;
    let mut attempt = 0u32;
    loop {
        match connect_once(
            relay,
            &server_name,
            &roots,
            &token_file,
            upstream,
            &mut identity,
            dashboards.clone(),
        )
        .await
        {
            Ok(()) => {
                tracing::info!("the relay closed the tunnel");
                attempt = 0;
            }
            Err(e) => {
                tracing::warn!(error = %e, "tunnel attempt failed");
                attempt = attempt.saturating_add(1);
                // Only a transport failure invalidates the certificate held.
                if matches!(e, enrol::EnrolError::Transport(_)) {
                    identity = None;
                }
            }
        }
        let wait = backoff(attempt);
        tracing::info!(seconds = wait.as_secs(), "reconnecting");
        tokio::time::sleep(wait).await;
    }
}

async fn connect_once(
    relay: &str,
    server_name: &str,
    roots: &str,
    token_file: &str,
    upstream: &str,
    identity: &mut Option<Identity>,
    dashboards: Arc<Dashboards>,
) -> Result<(), enrol::EnrolError> {
    if identity.is_none() {
        // Read per attempt, so a rotated token is picked up without a restart.
        let token = read_token(token_file)?;
        let issued = enrol::enrol(Bootstrap {
            relay_addr: relay,
            server_name,
            roots_pem: roots,
            token: &token,
            build: build(),
        })
        .await?;
        tracing::info!(spiffe_id = %issued.spiffe_id, "enrolled");
        *identity = Some(issued);
    }
    let id = identity.as_ref().expect("just enrolled");

    let tls = enrol::connect(relay, server_name, roots, Some(id)).await?;
    tracing::info!(%relay, "tunnel established, serving queries");
    Server::builder()
        .add_service(ConnectorServer::new(QueryService::new(
            Prometheus::new(upstream),
            dashboards,
        )))
        .serve_with_incoming(one_connection(tls))
        .await
        .map_err(|e| enrol::EnrolError::Transport(e.to_string()))
}

/// A trailing newline is the usual shape of a mounted secret.
fn read_token(path: &str) -> Result<String, enrol::EnrolError> {
    let token = std::fs::read_to_string(path)
        .map_err(|e| enrol::EnrolError::Token(format!("reading {path}: {e}")))?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(enrol::EnrolError::Token(format!("{path} is empty")));
    }
    Ok(token)
}

/// Without a relay address the connector serves a plain listener, which is how
/// the query path is exercised on its own.
async fn listen_locally(
    upstream: &str,
    dashboards: Arc<Dashboards>,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = env_or("VEDAVID_LISTEN", DEFAULT_LISTEN).parse()?;
    tracing::info!(%addr, %upstream, "serving Connector on a local listener");
    Server::builder()
        .add_service(ConnectorServer::new(QueryService::new(
            Prometheus::new(upstream),
            dashboards,
        )))
        .serve_with_shutdown(addr, async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
