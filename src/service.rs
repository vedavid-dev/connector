//! The `Connector` service. Every RPC is a translation of one Prometheus call;
//! nothing here holds state.

use crate::convert;
use crate::dashboards::Dashboards;
use crate::pb::{
    connector_server::Connector, ConnectorEvent, DashboardDocumentRequest,
    DashboardDocumentResponse, DownsampleInfo, DrainRequest, DrainResponse, EventsRequest,
    InstallCertificateRequest, InstallCertificateResponse, InstantQueryRequest, LabelValuesRequest,
    LabelValuesResponse, LabelsRequest, LabelsResponse, PanelResult, QueryError, QueryErrorKind,
    QueryResult, RangeQueryRequest, SeriesRequest, SeriesResponse,
};
use crate::prom::Prometheus;
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Request, Response, Status};

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
                return;
            }
            loop {
                match changed.recv().await {
                    Ok(()) => {
                        if tx.send(Ok(announce(dashboards.inventory()))).await.is_err() {
                            return;
                        }
                    }
                    // The current inventory is all the relay needs.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if tx.send(Ok(announce(dashboards.inventory()))).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn dashboard_document(
        &self,
        request: Request<DashboardDocumentRequest>,
    ) -> Result<Response<DashboardDocumentResponse>, Status> {
        let id = request.into_inner().id;
        match self.dashboards.document(&id) {
            Some(doc) => Ok(Response::new(doc)),
            None => Err(Status::not_found(format!("no render tree for `{id}`"))),
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
