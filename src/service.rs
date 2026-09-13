//! The `Connector` service. Every RPC is a translation of one Prometheus call;
//! nothing here holds state.

use crate::convert;
use crate::dashboards::{Dashboards, TreeError};
use crate::pb;
use crate::pb::{
    connector_server::Connector, ConnectorEvent, DownsampleInfo, DrainRequest, DrainResponse,
    EventsRequest, GetRenderTreeRequest, InstallCertificateRequest, InstallCertificateResponse,
    InstantQueryRequest, LabelValuesRequest, LabelValuesResponse, LabelsRequest, LabelsResponse,
    PanelResult, QueryError, QueryErrorKind, QueryResult, RangeQueryRequest, RenderTree,
    SeriesRequest, SeriesResponse,
};
use crate::prom::Prometheus;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Request, Response, Status};

const DEFAULT_HEARTBEAT_SECONDS: u64 = 30;

/// Often enough that the relay can call a connector gone within a couple of
/// minutes, and cheap enough to be free.
fn heartbeat_interval() -> Duration {
    Duration::from_secs(
        std::env::var("VEDAVID_HEARTBEAT_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_HEARTBEAT_SECONDS),
    )
}

fn heartbeat() -> ConnectorEvent {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    ConnectorEvent {
        event: Some(crate::pb::connector_event::Event::Heartbeat(
            pb::Heartbeat {
                sent_at: Some(prost_types::Timestamp {
                    seconds: now.as_secs() as i64,
                    nanos: now.subsec_nanos() as i32,
                }),
                cert_not_after: None,
            },
        )),
    }
}

pub struct QueryService {
    prom: Prometheus,
    dashboards: Arc<Dashboards>,
}

impl QueryService {
    pub fn new(prom: Prometheus, dashboards: Arc<Dashboards>) -> Self {
        Self { prom, dashboards }
    }
}

/// A query failure becomes a gRPC code so callers need not parse text.
fn status_for(e: QueryError) -> Status {
    let code = match QueryErrorKind::try_from(e.kind) {
        Ok(QueryErrorKind::QueryErrorBadQuery) => Code::InvalidArgument,
        Ok(QueryErrorKind::QueryErrorUnauthorized) => Code::PermissionDenied,
        Ok(QueryErrorKind::QueryErrorTimeout) => Code::DeadlineExceeded,
        Ok(QueryErrorKind::QueryErrorUpstreamUnreachable) => Code::Unavailable,
        Ok(QueryErrorKind::QueryErrorLimitExceeded) => Code::ResourceExhausted,
        _ => Code::Internal,
    };
    Status::new(code, e.message)
}

#[tonic::async_trait]
impl Connector for QueryService {
    async fn instant_query(
        &self,
        request: Request<InstantQueryRequest>,
    ) -> Result<Response<QueryResult>, Status> {
        let r = request.into_inner();
        let up = self
            .prom
            .instant(&r.query, r.time_ms, r.timeout_ms)
            .await
            .map_err(|e| status_for(convert::unreachable(&e.to_string())))?;
        if up.status >= 400 {
            return Err(status_for(convert::query_error(up.status, &up.body)));
        }
        convert::query_result(&up.body, up.took_ms, None)
            .map(Response::new)
            .map_err(status_for)
    }

    async fn range_query(
        &self,
        request: Request<RangeQueryRequest>,
    ) -> Result<Response<QueryResult>, Status> {
        let r = request.into_inner();
        let step = convert::step_secs(r.start_ms, r.end_ms, r.max_points);
        let up = self
            .prom
            .range(&r.query, r.start_ms, r.end_ms, step, r.timeout_ms)
            .await
            .map_err(|e| status_for(convert::unreachable(&e.to_string())))?;
        if up.status >= 400 {
            return Err(status_for(convert::query_error(up.status, &up.body)));
        }
        let downsample = DownsampleInfo {
            step_ms: step * 1000,
            points_returned: 0,
            decimated: false,
        };
        convert::query_result(&up.body, up.took_ms, Some(downsample))
            .map(Response::new)
            .map_err(status_for)
    }

    async fn labels(
        &self,
        request: Request<LabelsRequest>,
    ) -> Result<Response<LabelsResponse>, Status> {
        let r = request.into_inner();
        let up = self
            .prom
            .labels(r.start_ms, r.end_ms, &r.r#match)
            .await
            .map_err(|e| status_for(convert::unreachable(&e.to_string())))?;
        if up.status >= 400 {
            return Err(status_for(convert::query_error(up.status, &up.body)));
        }
        let (names, warnings) = convert::string_list(&up.body).map_err(status_for)?;
        Ok(Response::new(LabelsResponse { names, warnings }))
    }

    async fn label_values(
        &self,
        request: Request<LabelValuesRequest>,
    ) -> Result<Response<LabelValuesResponse>, Status> {
        let r = request.into_inner();
        let up = self
            .prom
            .label_values(&r.label, r.start_ms, r.end_ms, &r.r#match)
            .await
            .map_err(|e| status_for(convert::unreachable(&e.to_string())))?;
        if up.status >= 400 {
            return Err(status_for(convert::query_error(up.status, &up.body)));
        }
        let (values, warnings) = convert::string_list(&up.body).map_err(status_for)?;
        Ok(Response::new(LabelValuesResponse { values, warnings }))
    }

    async fn series(
        &self,
        request: Request<SeriesRequest>,
    ) -> Result<Response<SeriesResponse>, Status> {
        let r = request.into_inner();
        let up = self
            .prom
            .series(&r.r#match, r.start_ms, r.end_ms)
            .await
            .map_err(|e| status_for(convert::unreachable(&e.to_string())))?;
        if up.status >= 400 {
            return Err(status_for(convert::query_error(up.status, &up.body)));
        }
        let series = convert::series_list(&up.body).map_err(status_for)?;
        Ok(Response::new(SeriesResponse { series }))
    }

    type BatchQueryStream = ReceiverStream<Result<PanelResult, Status>>;

    async fn batch_query(
        &self,
        _request: Request<crate::pb::BatchQueryRequest>,
    ) -> Result<Response<Self::BatchQueryStream>, Status> {
        Err(Status::unimplemented("BatchQuery"))
    }

    type EventsStream = ReceiverStream<Result<ConnectorEvent, Status>>;

    /// The inventory is announced on connect and on every change, so the relay
    /// never polls for it.
    async fn events(
        &self,
        _request: Request<EventsRequest>,
    ) -> Result<Response<Self::EventsStream>, Status> {
        let dashboards = self.dashboards.clone();
        let mut changed = dashboards.subscribe();
        let (tx, rx) = tokio::sync::mpsc::channel(4);

        tokio::spawn(async move {
            let announce = |inventory| ConnectorEvent {
                event: Some(crate::pb::connector_event::Event::Inventory(inventory)),
            };
            if tx.send(Ok(announce(dashboards.inventory()))).await.is_err() {
                crate::tunnel::lost("the relay took no inventory");
            }
            let mut beat = tokio::time::interval(heartbeat_interval());
            beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            beat.tick().await;

            loop {
                let event = tokio::select! {
                    _ = beat.tick() => Ok(heartbeat()),
                    changed = changed.recv() => match changed {
                        Ok(()) => Ok(announce(dashboards.inventory())),
                        // The current inventory is all the relay needs.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            Ok(announce(dashboards.inventory()))
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                };
                if tx.send(event).await.is_err() {
                    crate::tunnel::lost("the relay stopped reading events");
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// Never-compiled and unknown are conditions the app answers differently.
    async fn get_render_tree(
        &self,
        request: Request<GetRenderTreeRequest>,
    ) -> Result<Response<RenderTree>, Status> {
        let id = request.into_inner().id;
        match self.dashboards.render_tree(&id) {
            Ok(tree) => Ok(Response::new(tree)),
            Err(TreeError::Unknown) => Err(Status::not_found(format!("no dashboard `{id}`"))),
            Err(TreeError::NeverCompiled) => Err(Status::failed_precondition(format!(
                "`{id}` has never compiled, so there is no render tree to serve"
            ))),
        }
    }

    async fn install_certificate(
        &self,
        _request: Request<InstallCertificateRequest>,
    ) -> Result<Response<InstallCertificateResponse>, Status> {
        Err(Status::unimplemented("InstallCertificate"))
    }

    async fn drain(
        &self,
        _request: Request<DrainRequest>,
    ) -> Result<Response<DrainResponse>, Status> {
        Err(Status::unimplemented("Drain"))
    }
}
