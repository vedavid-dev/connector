//! Prometheus HTTP API responses to protobuf, kept pure so the shapes are
//! covered by unit tests.

use crate::pb::{
    DownsampleInfo, LabelPair, QueryError, QueryErrorKind, QueryResult, ResultType, Sample,
    TimeSeries,
};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct Envelope<D> {
    status: String,
    data: Option<D>,
    #[serde(default)]
    warnings: Vec<String>,
    error: Option<String>,
    #[serde(rename = "errorType")]
    error_type: Option<String>,
}

#[derive(Deserialize)]
struct QueryData {
    #[serde(rename = "resultType")]
    result_type: String,
    result: serde_json::Value,
}

type Labels = BTreeMap<String, String>;

#[derive(Deserialize)]
struct VectorItem {
    metric: Labels,
    value: (f64, String),
}

#[derive(Deserialize)]
struct MatrixItem {
    metric: Labels,
    values: Vec<(f64, String)>,
}

/// Prometheus sends seconds; the wire contract is milliseconds.
fn to_ms(seconds: f64) -> i64 {
    (seconds * 1000.0).round() as i64
}

/// Values arrive as strings, including `NaN`, `+Inf` and `-Inf`.
fn to_f64(raw: &str) -> f64 {
    raw.parse().unwrap_or(f64::NAN)
}

fn pairs(labels: Labels) -> Vec<LabelPair> {
    labels
        .into_iter()
        .map(|(name, value)| LabelPair { name, value })
        .collect()
}

/// `errorType` wins over the status code, which a proxy may have rewritten.
fn kind_for(status: u16, error_type: Option<&str>) -> QueryErrorKind {
    use QueryErrorKind as K;
    match error_type {
        Some("bad_data" | "not_acceptable") => K::QueryErrorBadQuery,
        Some("timeout" | "canceled") => K::QueryErrorTimeout,
        Some("unavailable") => K::QueryErrorUpstreamUnreachable,
        Some("execution" | "internal") => K::QueryErrorUpstreamError,
        _ => match status {
            400 | 422 => K::QueryErrorBadQuery,
            401 | 403 => K::QueryErrorUnauthorized,
            503 => K::QueryErrorTimeout,
            s if s >= 500 => K::QueryErrorUpstreamError,
            _ => K::QueryErrorUnspecified,
        },
    }
}

/// A non-2xx or `status: error` body becomes the wire error type.
pub fn query_error(status: u16, body: &str) -> QueryError {
    let parsed: Option<Envelope<serde_json::Value>> = serde_json::from_str(body).ok();
    let (message, error_type) = match &parsed {
        Some(e) => (
            e.error.clone().unwrap_or_else(|| body.trim().to_string()),
            e.error_type.clone(),
        ),
        None => (body.trim().to_string(), None),
    };
    QueryError {
        kind: kind_for(status, error_type.as_deref()) as i32,
        message,
    }
}

pub fn unreachable(detail: &str) -> QueryError {
    QueryError {
        kind: QueryErrorKind::QueryErrorUpstreamUnreachable as i32,
        message: detail.to_string(),
    }
}

/// Converts a successful `/api/v1/query` or `/query_range` body.
pub fn query_result(
    body: &str,
    upstream_duration_ms: i64,
    downsample: Option<DownsampleInfo>,
) -> Result<QueryResult, QueryError> {
    let env: Envelope<QueryData> = serde_json::from_str(body).map_err(|e| QueryError {
        kind: QueryErrorKind::QueryErrorUpstreamError as i32,
        message: format!("unparsable upstream response: {e}"),
    })?;
    if env.status != "success" {
        return Err(query_error(200, body));
    }
    let data = env.data.ok_or_else(|| QueryError {
        kind: QueryErrorKind::QueryErrorUpstreamError as i32,
        message: "upstream reported success with no data".into(),
    })?;

    let (result_type, series) = match data.result_type.as_str() {
        "vector" => {
            let items: Vec<VectorItem> = serde_json::from_value(data.result).map_err(unparsable)?;
            (
                ResultType::Vector,
                items
                    .into_iter()
                    .map(|i| TimeSeries {
                        labels: pairs(i.metric),
                        samples: vec![Sample {
                            t_ms: to_ms(i.value.0),
                            v: to_f64(&i.value.1),
                        }],
                    })
                    .collect(),
            )
        }
        "matrix" => {
            let items: Vec<MatrixItem> = serde_json::from_value(data.result).map_err(unparsable)?;
            (
                ResultType::Matrix,
                items
                    .into_iter()
                    .map(|i| TimeSeries {
                        labels: pairs(i.metric),
                        samples: i
                            .values
                            .into_iter()
                            .map(|(t, v)| Sample {
                                t_ms: to_ms(t),
                                v: to_f64(&v),
                            })
                            .collect(),
                    })
                    .collect(),
            )
        }
        "scalar" | "string" => {
            let (t, v): (f64, String) = serde_json::from_value(data.result).map_err(unparsable)?;
            (
                ResultType::Scalar,
                vec![TimeSeries {
                    labels: Vec::new(),
                    samples: vec![Sample {
                        t_ms: to_ms(t),
                        v: to_f64(&v),
                    }],
                }],
            )
        }
        other => {
            return Err(QueryError {
                kind: QueryErrorKind::QueryErrorUpstreamError as i32,
                message: format!("unsupported resultType {other}"),
            })
        }
    };

    let points_returned = series.iter().map(|s| s.samples.len()).sum::<usize>() as i32;
    Ok(QueryResult {
        r#type: result_type as i32,
        series,
        warnings: surfaced(env.warnings),
        downsample: downsample.map(|d| DownsampleInfo {
            points_returned,
            ..d
        }),
        upstream_duration_ms,
    })
}

/// Info annotations are advice to whoever wrote the query, not news for whoever
/// is on call; the most common one flags every `rate()` over an exporter counter
/// whose name lacks `_total`. Prometheus versions that return them inside
/// `warnings` rather than a separate `infos` field are handled by prefix.
fn surfaced(warnings: Vec<String>) -> Vec<String> {
    warnings
        .into_iter()
        .filter(|w| !w.starts_with("PromQL info:"))
        .collect()
}

fn unparsable(e: serde_json::Error) -> QueryError {
    QueryError {
        kind: QueryErrorKind::QueryErrorUpstreamError as i32,
        message: format!("unexpected result shape: {e}"),
    }
}

/// `/api/v1/labels` and `/api/v1/label/<name>/values` share this shape.
pub fn string_list(body: &str) -> Result<(Vec<String>, Vec<String>), QueryError> {
    let env: Envelope<Vec<String>> = serde_json::from_str(body).map_err(unparsable)?;
    if env.status != "success" {
        return Err(query_error(200, body));
    }
    Ok((env.data.unwrap_or_default(), env.warnings))
}

/// `/api/v1/series` returns label sets with no samples.
pub fn series_list(body: &str) -> Result<Vec<TimeSeries>, QueryError> {
    let env: Envelope<Vec<Labels>> = serde_json::from_str(body).map_err(unparsable)?;
    if env.status != "success" {
        return Err(query_error(200, body));
    }
    Ok(env
        .data
        .unwrap_or_default()
        .into_iter()
        .map(|m| TimeSeries {
            labels: pairs(m),
            samples: Vec::new(),
        })
        .collect())
}

/// Prometheus needs a `step`; the wire contract carries a point budget instead.
pub fn step_secs(start_ms: i64, end_ms: i64, max_points: i32) -> i64 {
    let span = (end_ms - start_ms).max(0) / 1000;
    let budget = max_points.max(1) as i64;
    (span / budget).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bodies below are captured verbatim from Prometheus 3.14.
    const VECTOR: &str = r#"{"status":"success","data":{"resultType":"vector","result":[{"metric":{"__name__":"up","instance":"127.0.0.1:9099","job":"prometheus"},"value":[1788347462.466,"1"]}]}}"#;
    const MATRIX: &str = r#"{"status":"success","data":{"resultType":"matrix","result":[{"metric":{"__name__":"up","instance":"127.0.0.1:9099","job":"prometheus"},"values":[[1788347458,"1"],[1788347473,"1"]]}]}}"#;
    const SCALAR: &str =
        r#"{"status":"success","data":{"resultType":"scalar","result":[1788347473.866,"42"]}}"#;
    const NAN: &str =
        r#"{"status":"success","data":{"resultType":"scalar","result":[1788347474.105,"NaN"]}}"#;
    const INF: &str =
        r#"{"status":"success","data":{"resultType":"scalar","result":[1788347474.14,"+Inf"]}}"#;
    const BAD_QUERY: &str = r#"{"status":"error","errorType":"bad_data","error":"invalid parameter \"query\": 1:4: parse error: unexpected end of input inside braces"}"#;
    const LABELS: &str = r#"{"status":"success","data":["__name__","instance","job"]}"#;
    const SERIES: &str = r#"{"status":"success","data":[{"__name__":"up","instance":"127.0.0.1:9099","job":"prometheus"}]}"#;

    #[test]
    fn a_vector_becomes_one_sample_per_series() {
        let r = query_result(VECTOR, 7, None).unwrap();
        assert_eq!(r.r#type, ResultType::Vector as i32);
        assert_eq!(r.series.len(), 1);
        assert_eq!(
            r.series[0].samples,
            vec![Sample {
                t_ms: 1788347462466,
                v: 1.0
            }]
        );
        assert_eq!(r.upstream_duration_ms, 7);
        assert!(
            r.warnings.is_empty(),
            "absent warnings must default to empty"
        );
    }

    #[test]
    fn info_annotations_are_not_surfaced_but_warnings_are() {
        let body = r#"{"status":"success","data":{"resultType":"vector","result":[]},"warnings":["PromQL info: metric might not be a counter, name does not end in _total/_sum/_count/_bucket: \"pg_stat_database_tup_fetched\" (1:24)","PromQL warning: encountered a mix of histograms and floats for metric name \"x\""]}"#;
        let r = query_result(body, 0, None).unwrap();
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].starts_with("PromQL warning:"));
    }

    #[test]
    fn labels_are_carried_as_pairs_in_a_stable_order() {
        let r = query_result(VECTOR, 0, None).unwrap();
        let names: Vec<&str> = r.series[0].labels.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["__name__", "instance", "job"]);
    }

    /// Matrix timestamps arrive as JSON integers, unlike vector's floats.
    #[test]
    fn a_matrix_keeps_every_sample() {
        let r = query_result(MATRIX, 0, None).unwrap();
        assert_eq!(r.r#type, ResultType::Matrix as i32);
        let s = &r.series[0].samples;
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].t_ms, 1788347458000);
        assert_eq!(s[1].t_ms, 1788347473000);
    }

    #[test]
    fn a_scalar_becomes_a_labelless_series() {
        let r = query_result(SCALAR, 0, None).unwrap();
        assert_eq!(r.r#type, ResultType::Scalar as i32);
        assert!(r.series[0].labels.is_empty());
        assert_eq!(r.series[0].samples[0].v, 42.0);
    }

    /// Prometheus sends these as strings; losing them would silently read as 0.
    #[test]
    fn nan_and_infinity_survive() {
        assert!(query_result(NAN, 0, None).unwrap().series[0].samples[0]
            .v
            .is_nan());
        assert_eq!(
            query_result(INF, 0, None).unwrap().series[0].samples[0].v,
            f64::INFINITY
        );
    }

    #[test]
    fn a_bad_query_keeps_the_upstream_message_verbatim() {
        let e = query_error(400, BAD_QUERY);
        assert_eq!(e.kind, QueryErrorKind::QueryErrorBadQuery as i32);
        assert!(e.message.contains("unexpected end of input inside braces"));
        assert!(
            !e.message.contains("status"),
            "the envelope must not leak in"
        );
    }

    #[test]
    fn status_codes_map_to_kinds() {
        use QueryErrorKind;
        let k = |s, t| kind_for(s, t);
        assert_eq!(k(400, None), QueryErrorKind::QueryErrorBadQuery);
        assert_eq!(k(422, None), QueryErrorKind::QueryErrorBadQuery);
        assert_eq!(k(401, None), QueryErrorKind::QueryErrorUnauthorized);
        assert_eq!(k(403, None), QueryErrorKind::QueryErrorUnauthorized);
        assert_eq!(k(503, None), QueryErrorKind::QueryErrorTimeout);
        assert_eq!(k(500, None), QueryErrorKind::QueryErrorUpstreamError);
        assert_eq!(k(200, Some("timeout")), QueryErrorKind::QueryErrorTimeout);
        // errorType wins: a proxy may have rewritten the status code.
        assert_eq!(k(200, Some("bad_data")), QueryErrorKind::QueryErrorBadQuery);
        assert_eq!(k(500, Some("bad_data")), QueryErrorKind::QueryErrorBadQuery);
        assert_eq!(
            k(200, Some("unavailable")),
            QueryErrorKind::QueryErrorUpstreamUnreachable
        );
        assert_eq!(
            k(200, Some("execution")),
            QueryErrorKind::QueryErrorUpstreamError
        );
    }

    #[test]
    fn a_success_envelope_reporting_an_error_is_still_an_error() {
        let e = query_result(BAD_QUERY, 0, None).unwrap_err();
        assert_eq!(e.kind, QueryErrorKind::QueryErrorBadQuery as i32);
    }

    #[test]
    fn an_unparsable_body_is_an_upstream_error_not_a_panic() {
        let e = query_result("<html>502 Bad Gateway</html>", 0, None).unwrap_err();
        assert_eq!(e.kind, QueryErrorKind::QueryErrorUpstreamError as i32);
    }

    #[test]
    fn labels_and_series_bodies_convert() {
        let (names, warnings) = string_list(LABELS).unwrap();
        assert_eq!(names, vec!["__name__", "instance", "job"]);
        assert!(warnings.is_empty());

        let series = series_list(SERIES).unwrap();
        assert_eq!(series.len(), 1);
        assert!(series[0].samples.is_empty(), "series carries labels only");
        assert_eq!(series[0].labels.len(), 3);
    }

    #[test]
    fn downsample_info_reports_the_points_actually_returned() {
        let d = DownsampleInfo {
            step_ms: 15000,
            points_returned: 0,
            decimated: false,
        };
        let r = query_result(MATRIX, 0, Some(d)).unwrap();
        let got = r.downsample.unwrap();
        assert_eq!(got.step_ms, 15000);
        assert_eq!(got.points_returned, 2);
    }

    #[test]
    fn step_honours_the_point_budget_and_never_reaches_zero() {
        assert_eq!(step_secs(0, 3_600_000, 240), 15);
        assert_eq!(
            step_secs(0, 60_000, 240),
            1,
            "a short span still steps by 1s"
        );
        assert_eq!(
            step_secs(0, 3_600_000, 0),
            3600,
            "a zero budget must not divide by zero"
        );
        assert_eq!(
            step_secs(5_000, 0, 100),
            1,
            "an inverted range must not go negative"
        );
    }
}
