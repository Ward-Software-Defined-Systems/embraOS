// HTTP-path benches (S2-18): the storage/bitmap benches call execute_query
// directly, so nothing guarded the axum + middleware + serde envelope until
// now. These drive a real router over a loopback socket with reqwest — the
// numbers include routing, extractors, the JSON (de)serialization of request
// and response, and the spawn_blocking offload, on top of the query itself.
use std::sync::Arc;
use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::runtime::Runtime;

use wardsondb::config::Config;
use wardsondb::engine::storage::Storage;
use wardsondb::server::metrics::Metrics;
use wardsondb::server::{AppState, build_router};

// Twin of storage_bench's generator so the shapes stay comparable across
// the two bench files.
fn create_siem_event(i: u64) -> Value {
    let event_types = ["firewall", "dns", "dhcp", "auth", "vpn"];
    let severities = ["low", "medium", "high", "critical"];
    let actions = ["allow", "block", "drop", "reject"];
    let ports = [22, 80, 443, 8080, 3306];

    let idx = i as usize;

    json!({
        "event_type": event_types[idx % event_types.len()],
        "severity": severities[idx % severities.len()],
        "network": {
            "src_ip": format!("192.168.{}.{}", (i / 256) % 256, i % 256),
            "dst_ip": format!("10.0.{}.{}", (i / 256) % 256, i % 256),
            "dst_port": ports[idx % ports.len()],
            "action": actions[idx % actions.len()],
        },
        "received_at": format!("2026-03-09T{:02}:{:02}:{:02}Z", (i / 3600) % 24, (i / 60) % 60, i % 60),
        "message": format!("Event number {i}"),
    })
}

// Mirrors tests/integration_test.rs::test_config — keep both in sync when a
// CLI flag is added (clippy --all-targets breaks the build here if a field
// goes missing, same as the test copy).
fn bench_config(tmp: &TempDir, port: u16) -> Config {
    Config {
        port,
        data_dir: tmp.path().to_string_lossy().to_string(),
        storage_engine: "rocksdb".to_string(),
        log_level: "error".to_string(),
        log_file: tmp.path().join("bench.log").to_string_lossy().to_string(),
        verbose: false,
        tls: false,
        tls_cert: None,
        tls_key: None,
        ttl_interval: 60,
        api_keys: vec![],
        api_key_file: None,
        query_timeout: 30,
        max_query_limit: 100_000,
        max_body_mb: 64,
        metrics_public: false,
        cache_size_mb: 64,
        write_buffer_mb: 64,
        memtable_mb: 8,
        flush_workers: 2,
        compaction_workers: 2,
        bitmap_fields: String::new(),
        bitmap_max_cardinality: 1000,
        bitmap_sample_size: 100,
        bitmap_memory_mb: 0,
        no_bitmap: false,
    }
}

/// Boot a real server over a seeded rocksdb Storage on an ephemeral port.
/// The Runtime must stay alive for the server task, so it is returned.
fn start_server(
    n_docs: u64,
    index: Option<(&str, &[&str])>,
) -> (Runtime, String, reqwest::Client, TempDir) {
    let rt = Runtime::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let storage = Storage::open(tmp.path()).unwrap();
    storage.create_collection("events").unwrap();

    let batch_size = 500;
    let mut i = 0u64;
    while i < n_docs {
        let end = std::cmp::min(i + batch_size, n_docs);
        let docs: Vec<Value> = (i..end).map(create_siem_event).collect();
        storage.bulk_insert_documents("events", docs).unwrap();
        i = end;
    }
    if let Some((name, fields)) = index {
        let fields: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        storage.create_index("events", name, &fields).unwrap();
    }

    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(AppState {
        storage,
        config: bench_config(&tmp, addr.port()),
        started_at: Instant::now(),
        metrics: Arc::new(Metrics::new()),
        api_keys: vec![],
    });
    let app = build_router(state);
    rt.spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (rt, format!("http://{addr}"), reqwest::Client::new(), tmp)
}

fn post_json(rt: &Runtime, client: &reqwest::Client, url: &str, body: &Value) -> Value {
    rt.block_on(async {
        client
            .post(url)
            .json(body)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    })
}

fn bench_http_pages(c: &mut Criterion) {
    let mut group = c.benchmark_group("http_path");

    let (rt, base_url, client, _tmp) =
        start_server(10_000, Some(("idx_event_type", &["event_type"])));
    let query_url = format!("{base_url}/events/query");

    // Indexed bare page — the windowed fast path behind the full HTTP stack.
    let indexed_page = json!({"filter": {"event_type": "firewall"}, "limit": 10});
    let probe = post_json(&rt, &client, &query_url, &indexed_page);
    assert_eq!(
        probe["meta"]["index_used"], "idx_event_type",
        "bench must run through the single-field index"
    );

    group.bench_function("indexed_page_10k", |b| {
        b.iter(|| post_json(&rt, &client, &query_url, &indexed_page));
    });

    // Unindexed filter — the FullScan materializing page.
    let full_scan_page = json!({"filter": {"network.action": "block"}, "limit": 10});
    let probe = post_json(&rt, &client, &query_url, &full_scan_page);
    assert!(
        probe["meta"]["index_used"].is_null(),
        "bench must run through the full scan"
    );
    assert_eq!(probe["meta"]["returned_count"], 10);

    group.bench_function("full_scan_page_10k", |b| {
        b.iter(|| post_json(&rt, &client, &query_url, &full_scan_page));
    });

    // Filtered + sorted on a non-indexed sort field — the in-memory sort path.
    let sorted_page = json!({
        "filter": {"event_type": "firewall"},
        "sort": [{"received_at": "desc"}],
        "limit": 10
    });
    let probe = post_json(&rt, &client, &query_url, &sorted_page);
    assert_eq!(probe["meta"]["returned_count"], 10);

    group.bench_function("sorted_page_10k", |b| {
        b.iter(|| post_json(&rt, &client, &query_url, &sorted_page));
    });

    group.finish();
}

fn bench_http_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("http_path");

    let (rt, base_url, client, _tmp) = start_server(0, None);
    let docs_url = format!("{base_url}/events/docs");

    let probe = post_json(&rt, &client, &docs_url, &create_siem_event(0));
    assert!(
        probe["data"]["_id"].is_string(),
        "insert must return the created doc"
    );

    let mut i = 1u64;
    group.bench_function("single_insert", |b| {
        b.iter(|| {
            let doc = create_siem_event(i);
            i += 1;
            post_json(&rt, &client, &docs_url, &doc)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_http_pages, bench_http_insert);
criterion_main!(benches);
