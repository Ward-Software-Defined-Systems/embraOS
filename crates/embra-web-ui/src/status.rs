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
pub struct StatusData {
    #[serde(default)]
    pub services: Vec<Svc>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub ts: u64,
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
