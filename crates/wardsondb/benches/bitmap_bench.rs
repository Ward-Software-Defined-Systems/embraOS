use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::{Value, json};
use tempfile::TempDir;

use wardsondb::engine::storage::Storage;
use wardsondb::query::executor::execute_query;
use wardsondb::query::parser::{QueryRequest, parse_query};

fn create_siem_event(i: u64) -> Value {
    let event_types = ["firewall", "dns", "dhcp", "auth", "vpn"];
    let severities = ["low", "medium", "high", "critical"];
    let actions = ["allow", "block", "drop", "reject"];

    let idx = i as usize;

    json!({
        "event_type": event_types[idx % event_types.len()],
        "severity": severities[idx % severities.len()],
        "network": {
            "action": actions[idx % actions.len()],
        },
        "message": format!("Event number {i}"),
    })
}

fn setup_storage_with_bitmap(n: u64, bitmap_fields: Vec<String>) -> (Storage, TempDir) {
    let tmp = TempDir::new().unwrap();
    let storage = Storage::open(tmp.path()).unwrap();
    storage.create_collection("events").unwrap();

    // Configure bitmap accelerator
    storage
        .scan_accelerator
        .configure_fields(bitmap_fields.clone());
    storage.scan_accelerator.set_max_cardinality(1000);

    // Insert in batches
    let batch_size = 500;
    let mut i = 0u64;
    while i < n {
        let end = std::cmp::min(i + batch_size, n);
        let docs: Vec<Value> = (i..end).map(create_siem_event).collect();
        storage.bulk_insert_documents("events", docs).unwrap();
        i = end;
    }

    // Rebuild accelerator from storage
    let all_docs: Vec<(String, Value)> = storage
        .scan_all_documents("events")
        .unwrap()
        .into_iter()
        .filter_map(|doc| {
            doc.get("_id")
                .and_then(|v| v.as_str())
                .map(|id| (id.to_string(), doc.clone()))
        })
        .collect();
    storage
        .scan_accelerator
        .rebuild_from_storage("events", &all_docs);
    storage.scan_accelerator.set_ready(true);

    (storage, tmp)
}

fn setup_storage_no_bitmap(n: u64) -> (Storage, TempDir) {
    let tmp = TempDir::new().unwrap();
    let storage = Storage::open(tmp.path()).unwrap();
    storage.create_collection("events").unwrap();

    let batch_size = 500;
    let mut i = 0u64;
    while i < n {
        let end = std::cmp::min(i + batch_size, n);
        let docs: Vec<Value> = (i..end).map(create_siem_event).collect();
        storage.bulk_insert_documents("events", docs).unwrap();
        i = end;
    }

    (storage, tmp)
}

fn bench_bitmap_vs_full_scan_10k(c: &mut Criterion) {
    let mut group = c.benchmark_group("bitmap_vs_full_10k");
    group.sample_size(20);

    let bitmap_fields = vec![
        "event_type".to_string(),
        "severity".to_string(),
        "network.action".to_string(),
    ];
    let (storage_bm, _tmp1) = setup_storage_with_bitmap(10_000, bitmap_fields);
    let (storage_fs, _tmp2) = setup_storage_no_bitmap(10_000);

    group.bench_function("bitmap_eq", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({"event_type": "firewall"})),
                    sort: None,
                    limit: Some(50),
                    offset: Some(0),
                    fields: None,
                    count_only: None,
                    cursor: None,
                },
                100_000,
                "events",
            )
            .unwrap();
            execute_query(&storage_bm, "events", &query).unwrap();
        });
    });

    group.bench_function("full_scan_eq", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({"event_type": "firewall"})),
                    sort: None,
                    limit: Some(50),
                    offset: Some(0),
                    fields: None,
                    count_only: None,
                    cursor: None,
                },
                100_000,
                "events",
            )
            .unwrap();
            execute_query(&storage_fs, "events", &query).unwrap();
        });
    });

    group.finish();
}

fn bench_bitmap_vs_full_scan_100k(c: &mut Criterion) {
    let mut group = c.benchmark_group("bitmap_vs_full_100k");
    group.sample_size(10);

    let bitmap_fields = vec![
        "event_type".to_string(),
        "severity".to_string(),
        "network.action".to_string(),
    ];
    let (storage_bm, _tmp1) = setup_storage_with_bitmap(100_000, bitmap_fields);
    let (storage_fs, _tmp2) = setup_storage_no_bitmap(100_000);

    group.bench_function("bitmap_eq", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({"event_type": "firewall"})),
                    sort: None,
                    limit: Some(50),
                    offset: Some(0),
                    fields: None,
                    count_only: None,
                    cursor: None,
                },
                100_000,
                "events",
            )
            .unwrap();
            execute_query(&storage_bm, "events", &query).unwrap();
        });
    });

    group.bench_function("full_scan_eq", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({"event_type": "firewall"})),
                    sort: None,
                    limit: Some(50),
                    offset: Some(0),
                    fields: None,
                    count_only: None,
                    cursor: None,
                },
                100_000,
                "events",
            )
            .unwrap();
            execute_query(&storage_fs, "events", &query).unwrap();
        });
    });

    group.finish();
}

fn bench_bitmap_and_two_fields(c: &mut Criterion) {
    let mut group = c.benchmark_group("bitmap_and_100k");
    group.sample_size(10);

    let bitmap_fields = vec![
        "event_type".to_string(),
        "severity".to_string(),
        "network.action".to_string(),
    ];
    let (storage_bm, _tmp1) = setup_storage_with_bitmap(100_000, bitmap_fields);
    let (storage_fs, _tmp2) = setup_storage_no_bitmap(100_000);

    group.bench_function("bitmap_and", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({
                        "$and": [
                            {"event_type": "firewall"},
                            {"severity": "high"}
                        ]
                    })),
                    sort: None,
                    limit: Some(50),
                    offset: Some(0),
                    fields: None,
                    count_only: None,
                    cursor: None,
                },
                100_000,
                "events",
            )
            .unwrap();
            execute_query(&storage_bm, "events", &query).unwrap();
        });
    });

    group.bench_function("full_scan_and", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({
                        "$and": [
                            {"event_type": "firewall"},
                            {"severity": "high"}
                        ]
                    })),
                    sort: None,
                    limit: Some(50),
                    offset: Some(0),
                    fields: None,
                    count_only: None,
                    cursor: None,
                },
                100_000,
                "events",
            )
            .unwrap();
            execute_query(&storage_fs, "events", &query).unwrap();
        });
    });

    group.finish();
}

fn bench_bitmap_count_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("bitmap_count_100k");
    group.sample_size(10);

    let bitmap_fields = vec![
        "event_type".to_string(),
        "severity".to_string(),
        "network.action".to_string(),
    ];
    let (storage_bm, _tmp1) = setup_storage_with_bitmap(100_000, bitmap_fields);
    let (storage_fs, _tmp2) = setup_storage_no_bitmap(100_000);

    group.bench_function("bitmap_count", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({"event_type": "firewall"})),
                    sort: None,
                    limit: None,
                    offset: None,
                    fields: None,
                    count_only: Some(true),
                    cursor: None,
                },
                100_000,
                "events",
            )
            .unwrap();
            execute_query(&storage_bm, "events", &query).unwrap();
        });
    });

    group.bench_function("full_scan_count", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({"event_type": "firewall"})),
                    sort: None,
                    limit: None,
                    offset: None,
                    fields: None,
                    count_only: Some(true),
                    cursor: None,
                },
                100_000,
                "events",
            )
            .unwrap();
            execute_query(&storage_fs, "events", &query).unwrap();
        });
    });

    group.finish();
}

fn bench_insert_with_bitmap(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_overhead");
    group.sample_size(20);

    // With bitmap accelerator
    group.bench_function("insert_with_bitmap", |b| {
        let tmp = TempDir::new().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        storage.create_collection("events").unwrap();
        storage.scan_accelerator.configure_fields(vec![
            "event_type".to_string(),
            "severity".to_string(),
            "network.action".to_string(),
        ]);
        storage.scan_accelerator.set_ready(true);

        let mut i = 0u64;
        b.iter(|| {
            let doc = create_siem_event(i);
            storage.insert_document("events", doc).unwrap();
            i += 1;
        });
    });

    // Without bitmap accelerator
    group.bench_function("insert_without_bitmap", |b| {
        let tmp = TempDir::new().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        storage.create_collection("events").unwrap();

        let mut i = 0u64;
        b.iter(|| {
            let doc = create_siem_event(i);
            storage.insert_document("events", doc).unwrap();
            i += 1;
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_bitmap_vs_full_scan_10k,
    bench_bitmap_vs_full_scan_100k,
    bench_bitmap_and_two_fields,
    bench_bitmap_count_only,
    bench_insert_with_bitmap,
);
criterion_main!(benches);
