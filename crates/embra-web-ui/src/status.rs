//! Polls `GET /api/status` every 5 s into a reactive signal.

use leptos::prelude::*;
use serde::Deserialize;
use wasm_bindgen_futures::spawn_local;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Svc {
    pub name: String,
    pub state: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SystemMetrics {
    /// `None` on the first poll (no baseline) or non-Linux dev build.
    #[serde(default)]
    pub cpu_pct: Option<f64>,
    #[serde(default)]
    pub mem_total_bytes: Option<u64>,
    #[serde(default)]
    pub mem_used_bytes: Option<u64>,
    #[serde(default)]
    pub mem_pct: Option<f64>,
    #[serde(default)]
    pub load1: Option<f64>,
    #[serde(default)]
    pub load5: Option<f64>,
    #[serde(default)]
    pub load15: Option<f64>,
    /// Logical core count, for grading load average relative to capacity.
    #[serde(default)]
    pub cpu_count: Option<u32>,
    /// Worst-of-both partition usage percent (DATA vs STATE) — drives the
    /// DISK pill's bar fill and severity color. Bytes are sent through
    /// separately for the tooltip breakdown.
    #[serde(default)]
    pub disk_pct: Option<f64>,
    #[serde(default)]
    pub data_total_bytes: Option<u64>,
    #[serde(default)]
    pub data_used_bytes: Option<u64>,
    #[serde(default)]
    pub state_total_bytes: Option<u64>,
    #[serde(default)]
    pub state_used_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct StatusData {
    #[serde(default)]
    pub services: Vec<Svc>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub ts: u64,
    #[serde(default)]
    pub system: Option<SystemMetrics>,
}

/// Create the status signal and start the 5 s poll loop.
pub fn use_status() -> RwSignal<StatusData> {
    let sig = RwSignal::new(StatusData::default());
    spawn_local(async move {
        loop {
            if let Ok(resp) = gloo_net::http::Request::get("/api/status").send().await {
                if let Ok(data) = resp.json::<StatusData>().await {
                    sig.set(data);
                }
            }
            gloo_timers::future::TimeoutFuture::new(5_000).await;
        }
    });
    sig
}
