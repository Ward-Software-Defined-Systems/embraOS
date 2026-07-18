use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::{Value, json};
use tempfile::TempDir;

use wardsondb::engine::storage::{MemoryConfig, Storage};
use wardsondb::query::executor::execute_query;
use wardsondb::query::parser::{QueryRequest, parse_query};

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

fn setup_storage_with_docs(n: u64) -> (Storage, TempDir) {
    setup_storage_with_docs_on("rocksdb", n)
}

/// Engine-parameterized twin of `setup_storage_with_docs` — the fjall
/// benches (S2-18 parity slice) mirror their rocksdb guards exactly, so the
/// numbers are directly comparable.
fn setup_storage_with_docs_on(engine: &str, n: u64) -> (Storage, TempDir) {
    let tmp = TempDir::new().unwrap();
    let storage = Storage::open_with_config(tmp.path(), engine, MemoryConfig::default()).unwrap();
    storage.create_collection("events").unwrap();

    // Insert in batches of 500
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

fn bench_single_insert(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    let storage = Storage::open(tmp.path()).unwrap();
    storage.create_collection("events").unwrap();

    let mut i = 0u64;
    c.bench_function("single_insert", |b| {
        b.iter(|| {
            let doc = create_siem_event(i);
            storage.insert_document("events", doc).unwrap();
            i += 1;
        });
    });
}

fn bench_bulk_insert_500(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    let storage = Storage::open(tmp.path()).unwrap();
    storage.create_collection("events").unwrap();

    let mut i = 0u64;
    c.bench_function("bulk_insert_500", |b| {
        b.iter(|| {
            let docs: Vec<Value> = (i..i + 500).map(create_siem_event).collect();
            storage.bulk_insert_documents("events", docs).unwrap();
            i += 500;
        });
    });
}

fn bench_get_by_id(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    let storage = Storage::open(tmp.path()).unwrap();
    storage.create_collection("events").unwrap();

    // Insert 1000 docs and collect their IDs
    let mut ids = Vec::new();
    for i in 0..1000 {
        let doc = storage
            .insert_document("events", create_siem_event(i))
            .unwrap();
        ids.push(doc["_id"].as_str().unwrap().to_string());
    }

    let mut idx = 0;
    c.bench_function("get_by_id", |b| {
        b.iter(|| {
            let id = &ids[idx % ids.len()];
            storage.get_document("events", id).unwrap();
            idx += 1;
        });
    });
}

fn bench_query_10k(c: &mut Criterion) {
    let (storage, _tmp) = setup_storage_with_docs(10_000);

    c.bench_function("query_eq_filter_10k", |b| {
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
            execute_query(&storage, "events", &query).unwrap();
        });
    });

    c.bench_function("query_nested_filter_10k", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({"network.dst_port": 443})),
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
            execute_query(&storage, "events", &query).unwrap();
        });
    });

    c.bench_function("query_with_sort_10k", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({"event_type": "firewall"})),
                    sort: Some(json!([{"network.dst_port": "desc"}])),
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
            execute_query(&storage, "events", &query).unwrap();
        });
    });

    c.bench_function("query_count_only_10k", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({"severity": "high"})),
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
            execute_query(&storage, "events", &query).unwrap();
        });
    });
}

fn bench_query_100k(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_100k");
    group.sample_size(10); // Fewer samples for expensive benchmarks

    let (storage, _tmp) = setup_storage_with_docs(100_000);

    group.bench_function("eq_filter", |b| {
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
            execute_query(&storage, "events", &query).unwrap();
        });
    });

    group.bench_function("nested_eq_filter", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({"network.dst_port": 443})),
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
            execute_query(&storage, "events", &query).unwrap();
        });
    });

    group.bench_function("complex_filter_sort", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({
                        "$and": [
                            {"event_type": "firewall"},
                            {"network.action": "block"},
                            {"severity": {"$in": ["high", "critical"]}}
                        ]
                    })),
                    sort: Some(json!([{"received_at": "desc"}])),
                    limit: Some(100),
                    offset: Some(0),
                    fields: None,
                    count_only: None,
                    cursor: None,
                },
                100_000,
                "events",
            )
            .unwrap();
            execute_query(&storage, "events", &query).unwrap();
        });
    });

    group.bench_function("count_only", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({"severity": "high"})),
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
            execute_query(&storage, "events", &query).unwrap();
        });
    });

    group.bench_function("full_scan_no_filter", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: None,
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
            execute_query(&storage, "events", &query).unwrap();
        });
    });

    group.bench_function("projection", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({"event_type": "firewall"})),
                    sort: None,
                    limit: Some(50),
                    offset: Some(0),
                    fields: Some(vec![
                        "event_type".into(),
                        "network.src_ip".into(),
                        "severity".into(),
                    ]),
                    count_only: None,
                    cursor: None,
                },
                100_000,
                "events",
            )
            .unwrap();
            execute_query(&storage, "events", &query).unwrap();
        });
    });

    group.finish();
}

fn bench_scan_all(c: &mut Criterion) {
    let mut group = c.benchmark_group("scan_all");
    group.sample_size(10);

    let (storage, _tmp) = setup_storage_with_docs(100_000);

    group.bench_function("scan_100k_docs", |b| {
        b.iter(|| {
            storage.scan_all_documents("events").unwrap();
        });
    });

    group.finish();
}

// DT-8: pins the reverse-bounded IndexSorted page walk (2169c82). The one-time
// strategy assertion guards against the planner silently degrading this to an
// in-memory sort — query_with_sort_10k sorts on a non-indexed field and never
// exercised this path.
fn bench_index_sorted_desc_page(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_sorted_desc_page");
    group.sample_size(10);

    let (storage, _tmp) = setup_storage_with_docs(100_000);
    storage
        .create_index(
            "events",
            "idx_type_time",
            &["event_type".into(), "received_at".into()],
        )
        .unwrap();

    let make_query = || {
        parse_query(
            QueryRequest {
                filter: Some(json!({"event_type": "firewall"})),
                sort: Some(json!([{"received_at": "desc"}])),
                limit: Some(20),
                offset: Some(0),
                fields: None,
                count_only: None,
                cursor: None,
            },
            100_000,
            "events",
        )
        .unwrap()
    };

    let probe = execute_query(&storage, "events", &make_query()).unwrap();
    assert_eq!(
        probe.scan_strategy.as_deref(),
        Some("index_sorted"),
        "bench must exercise the IndexSorted path"
    );

    group.bench_function("desc_limit_20", |b| {
        b.iter(|| {
            let query = make_query();
            execute_query(&storage, "events", &query).unwrap();
        });
    });

    group.finish();
}

// Brackets the $regex execution cost on a full scan (one compile per document
// today vs one per query once compiled at parse time).
fn bench_regex_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("regex_scan");
    group.sample_size(10);

    let (storage, _tmp) = setup_storage_with_docs(10_000);

    group.bench_function("full_scan_10k", |b| {
        b.iter(|| {
            let query = parse_query(
                QueryRequest {
                    filter: Some(json!({"message": {"$regex": "^Event number 1[0-9]{3}$"}})),
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
            execute_query(&storage, "events", &query).unwrap();
        });
    });

    group.finish();
}

// Brackets the page-window load on index scans: an eq filter matching ~50k of
// 250k docs with limit 10 loads every candidate document today; with windowed
// loading it should fetch only the page.
fn bench_index_eq_page(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_eq_page");
    group.sample_size(10);

    let (storage, _tmp) = setup_storage_with_docs(250_000);
    storage
        .create_index("events", "idx_event_type", &["event_type".into()])
        .unwrap();

    let make_query = || {
        parse_query(
            QueryRequest {
                filter: Some(json!({"event_type": "firewall"})),
                sort: None,
                limit: Some(10),
                offset: Some(0),
                fields: None,
                count_only: None,
                cursor: None,
            },
            100_000,
            "events",
        )
        .unwrap()
    };

    let probe = execute_query(&storage, "events", &make_query()).unwrap();
    assert_eq!(
        probe.index_used.as_deref(),
        Some("idx_event_type"),
        "bench must run through the single-field index"
    );

    group.bench_function("limit_10_of_50k", |b| {
        b.iter(|| {
            let query = make_query();
            execute_query(&storage, "events", &query).unwrap();
        });
    });

    group.finish();
}

// Brackets H-P3.1: a two-arm $or over an indexed field, limit 10 of 100k
// docs (~20k matches per arm). Pre-union this always full-scanned; the union
// plans one index lookup per arm, dedups, and windows the page. The
// unindexed twin measures the full-scan cost the union replaces.
fn bench_or_union(c: &mut Criterion) {
    let mut group = c.benchmark_group("or_union");
    group.sample_size(10);

    let filter = json!({"$or": [{"event_type": "firewall"}, {"event_type": "dns"}]});
    let make_query = |filter: &Value| {
        parse_query(
            QueryRequest {
                filter: Some(filter.clone()),
                sort: None,
                limit: Some(10),
                offset: Some(0),
                fields: None,
                count_only: None,
                cursor: None,
            },
            100_000,
            "events",
        )
        .unwrap()
    };

    let (indexed, _tmp_a) = setup_storage_with_docs(100_000);
    indexed
        .create_index("events", "idx_event_type", &["event_type".into()])
        .unwrap();
    let probe = execute_query(&indexed, "events", &make_query(&filter)).unwrap();
    assert_eq!(
        probe.scan_strategy.as_deref(),
        Some("or_union"),
        "bench must run through the $or index union"
    );

    group.bench_function("two_eq_arms_indexed_100k", |b| {
        b.iter(|| {
            let query = make_query(&filter);
            execute_query(&indexed, "events", &query).unwrap();
        });
    });

    let (plain, _tmp_b) = setup_storage_with_docs(100_000);
    group.bench_function("two_eq_arms_full_scan_100k", |b| {
        b.iter(|| {
            let query = make_query(&filter);
            execute_query(&plain, "events", &query).unwrap();
        });
    });

    group.finish();
}

// Brackets the residual-filter page (H-P2's target shape): an indexed eq
// narrows to ~20k candidates, and EVERY candidate is hydrated to apply the
// unindexed residual ({network.action}) before the limit-10 slice. With
// filter-during-scan streaming the hydration stops at limit+1 matches.
fn bench_residual_filter_page(c: &mut Criterion) {
    let mut group = c.benchmark_group("residual_filter_page");
    group.sample_size(10);

    let (storage, _tmp) = setup_storage_with_docs(100_000);
    storage
        .create_index("events", "idx_event_type", &["event_type".into()])
        .unwrap();

    let make_query = || {
        parse_query(
            QueryRequest {
                filter: Some(json!({
                    "$and": [
                        {"event_type": "firewall"},
                        {"network.action": "block"}
                    ]
                })),
                sort: None,
                limit: Some(10),
                offset: Some(0),
                fields: None,
                count_only: None,
                cursor: None,
            },
            100_000,
            "events",
        )
        .unwrap()
    };

    // Doc-returning index scans deliberately report scan_strategy: None
    // (R9 kept the None-vs-Some sites) — index_used is the strategy probe
    // here, same as bench_index_eq_page.
    let probe = execute_query(&storage, "events", &make_query()).unwrap();
    assert_eq!(
        probe.index_used.as_deref(),
        Some("idx_event_type"),
        "bench must run through the single-field index"
    );

    group.bench_function("eq_plus_residual_limit_10_100k", |b| {
        b.iter(|| {
            let query = make_query();
            execute_query(&storage, "events", &query).unwrap();
        });
    });

    group.finish();
}

// Fjall twins of the two guard benches (S2-18 parity slice: fjall previously
// had ZERO bench coverage). Same shapes and sizes as the rocksdb versions so
// the engines are directly comparable and both stay guarded through H-P2's
// BackendIterator rewrite.
fn bench_fjall_pages(c: &mut Criterion) {
    let mut group = c.benchmark_group("fjall_index_sorted_desc_page");
    group.sample_size(10);

    let (storage, _tmp) = setup_storage_with_docs_on("fjall", 100_000);
    storage
        .create_index(
            "events",
            "idx_type_time",
            &["event_type".into(), "received_at".into()],
        )
        .unwrap();

    let make_sorted_query = || {
        parse_query(
            QueryRequest {
                filter: Some(json!({"event_type": "firewall"})),
                sort: Some(json!([{"received_at": "desc"}])),
                limit: Some(20),
                offset: Some(0),
                fields: None,
                count_only: None,
                cursor: None,
            },
            100_000,
            "events",
        )
        .unwrap()
    };
    let probe = execute_query(&storage, "events", &make_sorted_query()).unwrap();
    assert_eq!(
        probe.scan_strategy.as_deref(),
        Some("index_sorted"),
        "bench must exercise the IndexSorted path"
    );
    group.bench_function("desc_limit_20", |b| {
        b.iter(|| {
            let query = make_sorted_query();
            execute_query(&storage, "events", &query).unwrap();
        });
    });
    group.finish();
    drop(storage);

    let mut group = c.benchmark_group("fjall_index_eq_page");
    group.sample_size(10);

    let (storage, _tmp) = setup_storage_with_docs_on("fjall", 250_000);
    storage
        .create_index("events", "idx_event_type", &["event_type".into()])
        .unwrap();

    let make_eq_query = || {
        parse_query(
            QueryRequest {
                filter: Some(json!({"event_type": "firewall"})),
                sort: None,
                limit: Some(10),
                offset: Some(0),
                fields: None,
                count_only: None,
                cursor: None,
            },
            100_000,
            "events",
        )
        .unwrap()
    };
    let probe = execute_query(&storage, "events", &make_eq_query()).unwrap();
    assert_eq!(
        probe.index_used.as_deref(),
        Some("idx_event_type"),
        "bench must run through the single-field index"
    );
    group.bench_function("limit_10_of_50k", |b| {
        b.iter(|| {
            let query = make_eq_query();
            execute_query(&storage, "events", &query).unwrap();
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_single_insert,
    bench_bulk_insert_500,
    bench_get_by_id,
    bench_query_10k,
    bench_query_100k,
    bench_scan_all,
    bench_index_sorted_desc_page,
    bench_regex_scan,
    bench_index_eq_page,
    bench_or_union,
    bench_residual_filter_page,
    bench_fjall_pages,
);
criterion_main!(benches);
