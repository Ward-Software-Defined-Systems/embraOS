//! `GET /api/status` — service-health dashboard feed.
//!
//! Re-runs the *same* probes the supervisor defines (raw TCP / raw
//! HTTP/1.0, no reqwest), independently. **Zero changes to PID-1 embrad**:
//! the supervisor's health logic is in-process and unexposed, so we just
//! observe the well-known localhost endpoints it manages. Frontend polls
//! this every 5 s.
//!
//! One row is not a localhost probe: `provider` — the active LLM
//! provider's endpoint health as the brain last measured it (5-minute
//! probe + after provider changes), read through apid's REST `/status`
//! (which proxies the brain's `GetSystemStatus`). It is omitted, not
//! shown red, when nothing is configured yet or apid cannot reach the
//! brain — a missing pill means "unknown", a red one means "down".

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::metrics;
use crate::state::AppState;

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Raw TCP connect check (what the supervisor's `Grpc` health does).
async fn tcp_ok(addr: &str) -> bool {
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// Raw HTTP/1.0 GET (what the supervisor's `Http` health does). Returns
/// the full response text if the status line carried 200.
async fn http_get(addr: &str, path: &str) -> Option<String> {
    let fut = async {
        let mut stream = TcpStream::connect(addr).await.ok()?;
        let req = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n");
        stream.write_all(req.as_bytes()).await.ok()?;
        let mut buf = Vec::with_capacity(4096);
        // HTTP/1.0 + Connection: close → server closes after the body.
        stream.read_to_end(&mut buf).await.ok()?;
        Some(String::from_utf8_lossy(&buf).into_owned())
    };
    match tokio::time::timeout(PROBE_TIMEOUT, fut).await {
        Ok(Some(resp)) if resp.contains(" 200") || resp.contains("200 OK") => Some(resp),
        _ => None,
    }
}

fn svc(name: &str, up: bool, detail: &str) -> Value {
    json!({ "name": name, "state": if up { "up" } else { "down" }, "detail": detail })
}

/// The active LLM provider as apid's `/status` reports it: a `provider`
/// pill when the brain has probed one (`up`/`down`), `None` for
/// unknown/unconfigured or a non-200 answer.
fn parse_provider_pill(resp: &str) -> Option<Value> {
    let body = resp.split("\r\n\r\n").nth(1)?;
    let v: Value = serde_json::from_str(body.trim()).ok()?;
    let services = v.get("data")?.get("services")?;
    let state = services.get("llm-provider")?.as_str()?;
    if state != "up" && state != "down" {
        return None;
    }
    let detail = services
        .get("llm-provider.detail")
        .and_then(Value::as_str)
        .unwrap_or("LLM provider");
    Some(svc("provider", state == "up", detail))
}

/// Pull `data.embraos_version` out of apid's `/version` HTTP response.
fn parse_version(resp: &str) -> Option<String> {
    let body = resp.split("\r\n\r\n").nth(1)?;
    let v: Value = serde_json::from_str(body.trim()).ok()?;
    v.get("data")?
        .get("embraos_version")?
        .as_str()
        .map(str::to_owned)
}

pub async fn api_status(State(state): State<AppState>) -> Json<Value> {
    // Probe concurrently; each is bounded by PROBE_TIMEOUT.
    let (wardson, trustd, apid_grpc, apid_http, brain) = tokio::join!(
        http_get("127.0.0.1:8090", "/_health"),
        tcp_ok("127.0.0.1:50001"),
        tcp_ok("127.0.0.1:50000"),
        http_get("127.0.0.1:8443", "/health"),
        tcp_ok("127.0.0.1:50002"),
    );
    let (version_resp, brain_status_resp) = tokio::join!(
        http_get("127.0.0.1:8443", "/version"),
        http_get("127.0.0.1:8443", "/status"),
    );

    let apid_up = apid_grpc && apid_http.is_some();
    let mut services = vec![
        svc("wardsondb", wardson.is_some(), "HTTP /_health :8090"),
        svc("embra-trustd", trustd, "gRPC :50001"),
        svc("embra-apid", apid_up, "gRPC :50000 + REST :8443"),
        svc("embra-brain", brain, "gRPC :50002"),
        svc("embra-web", true, "HTTPS :3345 (self)"),
    ];
    if let Some(provider) = brain_status_resp.as_deref().and_then(parse_provider_pill) {
        services.push(provider);
    }

    let version = version_resp
        .as_deref()
        .and_then(parse_version)
        .map(Value::from)
        .unwrap_or(Value::Null);

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let system = collect_system_metrics(&state);

    Json(json!({
        "services": services,
        "version": version,
        "ts": ts,
        "system": system,
    }))
}

/// Snapshot CPU / memory / load / disk. Returns `Value::Null` when nothing
/// useful is available (non-Linux dev build, /proc read failure, etc.)
/// so the frontend can hide the meters cleanly.
fn collect_system_metrics(state: &AppState) -> Value {
    let curr_cpu = metrics::read_cpu_snapshot();
    let mem = metrics::read_mem_info();
    let load = metrics::read_loadavg();
    let data = metrics::read_disk_info("/embra/data");
    let state_fs = metrics::read_disk_info("/embra/state");

    // CPU% needs two samples. Swap curr into shared state; if a prev
    // existed, compute the delta-based percent. First poll → `null`.
    let cpu_pct = match curr_cpu {
        Some(curr) => {
            let prev = state.cpu_snap.lock().ok().and_then(|mut g| g.replace(curr));
            prev.and_then(|p| metrics::compute_cpu_pct(&p, &curr))
        }
        None => None,
    };

    // Per-partition percents so DATA and STATE render as their own pills.
    let data_pct = data.and_then(|d| d.used_pct());
    let state_pct = state_fs.and_then(|s| s.used_pct());

    if cpu_pct.is_none()
        && mem.is_none()
        && load.is_none()
        && data_pct.is_none()
        && state_pct.is_none()
    {
        return Value::Null;
    }

    // `available_parallelism` works cross-platform without /proc — it lets
    // the frontend color-code the LOAD pill relative to the core count.
    let cpu_count = std::thread::available_parallelism().map(|n| n.get()).ok();

    json!({
        "cpu_pct": cpu_pct,
        "cpu_count": cpu_count,
        "mem_total_bytes": mem.map(|m| m.total_bytes),
        "mem_used_bytes": mem.map(|m| m.used_bytes()),
        "mem_pct": mem.and_then(|m| m.used_pct()),
        "load1": load.map(|l| l.load1),
        "load5": load.map(|l| l.load5),
        "load15": load.map(|l| l.load15),
        "data_pct": data_pct,
        "data_total_bytes": data.map(|d| d.total_bytes),
        "data_used_bytes": data.map(|d| d.used_bytes()),
        "state_pct": state_pct,
        "state_total_bytes": state_fs.map(|s| s.total_bytes),
        "state_used_bytes": state_fs.map(|s| s.used_bytes()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_200(body: &str) -> String {
        format!("HTTP/1.0 200 OK\r\ncontent-type: application/json\r\n\r\n{body}")
    }

    #[test]
    fn parse_version_reads_the_apid_payload() {
        let resp = http_200(r#"{"ok":true,"data":{"embraos_version":"0.14.0-phase1","service":"embra-apid"}}"#);
        assert_eq!(parse_version(&resp).as_deref(), Some("0.14.0-phase1"));
        assert_eq!(parse_version("HTTP/1.0 200 OK\r\n\r\nnot json"), None);
    }

    #[test]
    fn provider_pill_follows_the_brain_probe_state() {
        let up = http_200(
            r#"{"ok":true,"data":{"version":"x","services":{"llm-provider":"up","llm-provider.detail":"lm_studio · qwen/qwen3.8-27b · http://mac:1234 · model present · 12 ms · checked 3 s ago"}}}"#,
        );
        let pill = parse_provider_pill(&up).expect("pill");
        assert_eq!(pill["name"], "provider");
        assert_eq!(pill["state"], "up");
        assert!(pill["detail"].as_str().unwrap().contains("qwen3.8"));

        let down = http_200(r#"{"ok":true,"data":{"services":{"llm-provider":"down","llm-provider.detail":"anthropic · claude-opus-5 · api.anthropic.com · unreachable · checked 9 s ago · timeout"}}}"#);
        assert_eq!(parse_provider_pill(&down).unwrap()["state"], "down");

        // Unknown (nothing configured), no entry, or a 503 body → no pill.
        let unknown = http_200(r#"{"ok":true,"data":{"services":{"llm-provider":"unknown","llm-provider.detail":"no LLM provider configured yet"}}}"#);
        assert!(parse_provider_pill(&unknown).is_none());
        assert!(parse_provider_pill(&http_200(r#"{"ok":true,"data":{"services":{}}}"#)).is_none());
        assert!(parse_provider_pill(r#"HTTP/1.0 503 Service Unavailable\r\n\r\n{"ok":false,"error":"x"}"#).is_none());
    }
}
