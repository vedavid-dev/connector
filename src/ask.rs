//! The operator's say over Ask: on or off, and corrections to how Ask reads
//! this cluster. Both come from the chart's values and are announced to the
//! relay once per connection. Overrides are checked for shape only; the
//! connector parses no PromQL, so whether an expression is right is the
//! relay's to decide.

use std::collections::BTreeMap;

use serde::Deserialize;

pub const MAX_OVERRIDES: usize = 20;
pub const MAX_EXPRESSION: usize = 1000;
pub const PROVIDABLE: [&str; 4] = ["rate", "total", "quantile", "value"];
pub const PLACEHOLDERS: [&str; 6] = ["by", "window", "offset", "filter", "and_filter", "q"];
pub const ENTITY_KINDS: [&str; 6] = [
    "service",
    "pod",
    "namespace",
    "node",
    "container",
    "database",
];

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Override {
    pub signal: String,
    pub entities: BTreeMap<String, String>,
    pub provides: BTreeMap<String, String>,
    pub describe: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskPolicy {
    pub enabled: bool,
    pub overrides: Vec<Override>,
    /// What was given, re-sent to the relay as is.
    pub overrides_json: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AskConfigError {
    #[error("VEDAVID_ASK_ENABLED must be true or false, not `{0}`")]
    Enabled(String),
    #[error("VEDAVID_ASK_SIGNALS is not a JSON list of overrides: {0}")]
    Parse(String),
    #[error("VEDAVID_ASK_SIGNALS has {0} overrides; at most {MAX_OVERRIDES} are allowed")]
    TooMany(usize),
    #[error("ask.signals[{index}]: {message}")]
    Invalid { index: usize, message: String },
}

impl AskPolicy {
    pub fn from_env() -> Result<AskPolicy, AskConfigError> {
        let enabled = std::env::var("VEDAVID_ASK_ENABLED").unwrap_or_else(|_| "true".into());
        let signals = std::env::var("VEDAVID_ASK_SIGNALS").unwrap_or_else(|_| "[]".into());
        AskPolicy::parse(&enabled, &signals)
    }

    pub fn parse(enabled: &str, overrides_json: &str) -> Result<AskPolicy, AskConfigError> {
        let enabled = match enabled.trim() {
            "true" | "1" => true,
            "false" | "0" => false,
            other => return Err(AskConfigError::Enabled(other.to_string())),
        };
        let json = if overrides_json.trim().is_empty() {
            "[]"
        } else {
            overrides_json
        };
        let overrides: Vec<Override> =
            serde_json::from_str(json).map_err(|e| AskConfigError::Parse(e.to_string()))?;
        if overrides.len() > MAX_OVERRIDES {
            return Err(AskConfigError::TooMany(overrides.len()));
        }
        for (index, o) in overrides.iter().enumerate() {
            o.validate()
                .map_err(|message| AskConfigError::Invalid { index, message })?;
        }
        Ok(AskPolicy {
            enabled,
            overrides,
            overrides_json: json.to_string(),
        })
    }

    pub fn to_pb(&self) -> crate::pb::AskPolicy {
        crate::pb::AskPolicy {
            enabled: self.enabled,
            signal_overrides_json: self.overrides_json.clone(),
        }
    }

    /// The effective configuration, without expressions: an override may
    /// name a customer's labels and values, which do not belong at INFO.
    pub fn log(&self) {
        tracing::info!(
            enabled = self.enabled,
            overrides = self.overrides.len(),
            "ask"
        );
        for o in &self.overrides {
            tracing::info!(signal = %o.signal, describe = %o.describe, "ask override");
            for (function, expr) in &o.provides {
                tracing::debug!(signal = %o.signal, %function, %expr, "ask override expression");
            }
        }
    }
}

fn is_signal_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

fn is_label_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// Every `$name` in an expression, as written.
fn placeholders_in(expr: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = expr;
    while let Some(i) = rest.find('$') {
        rest = &rest[i + 1..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    out
}

impl Override {
    pub fn validate(&self) -> Result<(), String> {
        if !is_signal_name(&self.signal) {
            return Err(format!(
                "signal `{}` must match ^[a-z][a-z0-9_]{{0,31}}$",
                self.signal
            ));
        }
        if self.entities.is_empty() {
            return Err("`entities` is empty".to_string());
        }
        for (kind, label) in &self.entities {
            if !ENTITY_KINDS.contains(&kind.as_str()) {
                return Err(format!(
                    "entity kind `{kind}` is not one of {}",
                    ENTITY_KINDS.join(", ")
                ));
            }
            if !is_label_name(label) {
                return Err(format!("entities.{kind}: `{label}` is not a label name"));
            }
        }
        if self.provides.is_empty() {
            return Err("`provides` is empty".to_string());
        }
        for (function, expr) in &self.provides {
            if function == "ratio" {
                return Err(
                    "provides.ratio cannot be set; Ask derives a ratio from `rate` and `total`, so provide both".to_string(),
                );
            }
            if !PROVIDABLE.contains(&function.as_str()) {
                return Err(format!(
                    "provides.{function}: unknown function; use one of {}",
                    PROVIDABLE.join(", ")
                ));
            }
            let len = expr.chars().count();
            if len > MAX_EXPRESSION {
                return Err(format!(
                    "provides.{function}: {len} characters, over the {MAX_EXPRESSION} limit"
                ));
            }
            for name in placeholders_in(expr) {
                if !PLACEHOLDERS.contains(&name) {
                    return Err(format!(
                        "provides.{function}: `${name}` is not a placeholder; use $by, $window, $offset, $filter, $and_filter or $q"
                    ));
                }
            }
            if function == "quantile" && !placeholders_in(expr).contains(&"q") {
                return Err("provides.quantile must use $q".to_string());
            }
        }
        if self.describe.trim().is_empty() {
            return Err("`describe` is empty".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"[{"signal":"errors","entities":{"service":"component"},
      "provides":{"rate":"sum by ($by) (rate(myapp_failures_total{$filter}[$window]$offset))",
                  "total":"sum by ($by) (rate(myapp_requests_total{$filter}[$window]$offset))"},
      "describe":"myapp_failures_total over myapp_requests_total"}]"#;

    #[test]
    fn defaults_are_enabled_with_no_overrides() {
        let p = AskPolicy::parse("true", "").unwrap();
        assert!(p.enabled);
        assert!(p.overrides.is_empty());
        assert_eq!(p.to_pb().signal_overrides_json, "[]");
        assert!(!AskPolicy::parse("false", "[]").unwrap().enabled);
        assert!(matches!(
            AskPolicy::parse("maybe", "[]"),
            Err(AskConfigError::Enabled(_))
        ));
    }

    #[test]
    fn a_good_override_passes_and_travels_as_given() {
        let p = AskPolicy::parse("true", GOOD).unwrap();
        assert_eq!(p.overrides[0].entities["service"], "component");
        assert_eq!(p.to_pb().signal_overrides_json, GOOD);
    }

    #[test]
    fn ratio_is_refused_naming_rate_and_total() {
        let bad = GOOD.replace(r#""total""#, r#""ratio""#);
        let err = AskPolicy::parse("true", &bad).unwrap_err().to_string();
        assert!(err.contains("ask.signals[0]"), "{err}");
        assert!(err.contains("`rate` and `total`"), "{err}");
    }

    #[test]
    fn unknown_placeholders_fields_and_kinds_are_refused() {
        let bad = GOOD
            .replace("$and_filter", "$foo")
            .replace("$filter", "$foo");
        let err = AskPolicy::parse("true", &bad).unwrap_err().to_string();
        assert!(err.contains("$foo"), "{err}");

        let bad = GOOD.replace(r#""describe""#, r#""when":{},"describe""#);
        assert!(matches!(
            AskPolicy::parse("true", &bad),
            Err(AskConfigError::Parse(_))
        ));

        let bad = GOOD.replace(r#""service""#, r#""team""#);
        let err = AskPolicy::parse("true", &bad).unwrap_err().to_string();
        assert!(err.contains("entity kind `team`"), "{err}");
    }

    #[test]
    fn too_many_or_too_long_are_refused() {
        let one: serde_json::Value = serde_json::from_str(GOOD).unwrap();
        let many: Vec<_> = std::iter::repeat_n(one[0].clone(), 21).collect();
        let json = serde_json::to_string(&many).unwrap();
        assert_eq!(
            AskPolicy::parse("true", &json),
            Err(AskConfigError::TooMany(21))
        );

        let long = GOOD.replace("myapp_requests_total", &"x".repeat(1001));
        let err = AskPolicy::parse("true", &long).unwrap_err().to_string();
        assert!(err.contains("over the 1000 limit"), "{err}");
    }
}
