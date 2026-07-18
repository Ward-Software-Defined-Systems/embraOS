use reqwest::Client;
use serde_json::{Value, json};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;

use wardsondb::config::Config;
use wardsondb::engine::storage::Storage;
use wardsondb::server::metrics::Metrics;
use wardsondb::server::{AppState, build_router};

fn test_config(tmp: &TempDir, port: u16) -> Config {
    Config {
        port,
        data_dir: tmp.path().to_string_lossy().to_string(),
        storage_engine: "rocksdb".to_string(),
        log_level: "error".to_string(),
        log_file: tmp.path().join("test.log").to_string_lossy().to_string(),
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

async fn start_test_server() -> (String, TempDir) {
    start_test_server_with_keys(vec![]).await
}

async fn start_test_server_with_bitmap(bitmap_fields: &str) -> (String, TempDir) {
    let tmp = TempDir::new().unwrap();
    let storage = Storage::open(tmp.path()).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut config = test_config(&tmp, port);
    config.bitmap_fields = bitmap_fields.to_string();

    // Configure the scan accelerator with explicit fields
    if !bitmap_fields.is_empty() {
        let fields: Vec<String> = bitmap_fields
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        storage.scan_accelerator.configure_fields(fields);
        storage.scan_accelerator.set_ready(true);
    }

    let state = Arc::new(AppState {
        storage,
        config,
        started_at: Instant::now(),
        metrics: Arc::new(Metrics::new()),
        api_keys: vec![],
    });

    let app = build_router(state);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    let base_url = format!("http://{addr}");

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    (base_url, tmp)
}

async fn start_test_server_with_keys(api_keys: Vec<String>) -> (String, TempDir) {
    let tmp = TempDir::new().unwrap();
    let storage = Storage::open(tmp.path()).unwrap();

    // Find a free port
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let config = test_config(&tmp, port);

    let state = Arc::new(AppState {
        storage,
        config,
        started_at: Instant::now(),
        metrics: Arc::new(Metrics::new()),
        api_keys,
    });

    let app = build_router(state);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    let base_url = format!("http://{addr}");

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give the server a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (base_url, tmp)
}

async fn start_test_server_with_max_query_limit(max: u64) -> (String, TempDir) {
    let tmp = TempDir::new().unwrap();
    let storage = Storage::open(tmp.path()).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut config = test_config(&tmp, port);
    config.max_query_limit = max;

    let state = Arc::new(AppState {
        storage,
        config,
        started_at: Instant::now(),
        metrics: Arc::new(Metrics::new()),
        api_keys: vec![],
    });

    let app = build_router(state);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    let base_url = format!("http://{addr}");

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    (base_url, tmp)
}

#[tokio::test]
async fn test_health_and_info() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Health check
    let resp = client
        .get(format!("{base_url}/_health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["status"], "healthy");

    // Server info
    let resp = client.get(&base_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["name"], "WardSONDB");

    // Stats
    let resp = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["collection_count"], 0);
}

#[tokio::test]
async fn test_collection_lifecycle() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Create collection
    let resp = client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["name"], "events");

    // Duplicate collection → 409
    let resp = client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    // List collections
    let resp = client
        .get(format!("{base_url}/_collections"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let collections = body["data"].as_array().unwrap();
    assert_eq!(collections.len(), 1);
    assert_eq!(collections[0]["name"], "events");

    // Get collection info
    let resp = client
        .get(format!("{base_url}/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Drop collection
    let resp = client
        .delete(format!("{base_url}/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Verify dropped
    let resp = client
        .get(format!("{base_url}/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn test_document_crud() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Create collection
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "users"}))
        .send()
        .await
        .unwrap();

    // Insert document
    let resp = client
        .post(format!("{base_url}/users/docs"))
        .json(&json!({"name": "Alice", "age": 30}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    let doc_id = body["data"]["_id"].as_str().unwrap().to_string();
    assert_eq!(body["data"]["name"], "Alice");
    assert_eq!(body["data"]["_rev"], 1);

    // Get by ID
    let resp = client
        .get(format!("{base_url}/users/docs/{doc_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["name"], "Alice");

    // Replace (PUT)
    let resp = client
        .put(format!("{base_url}/users/docs/{doc_id}"))
        .json(&json!({"name": "Alice Smith", "age": 31}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["name"], "Alice Smith");
    assert_eq!(body["data"]["_rev"], 2);

    // Partial update (PATCH)
    let resp = client
        .patch(format!("{base_url}/users/docs/{doc_id}"))
        .json(&json!({"email": "alice@example.com"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["email"], "alice@example.com");
    assert_eq!(body["data"]["name"], "Alice Smith");
    assert_eq!(body["data"]["_rev"], 3);

    // Delete
    let resp = client
        .delete(format!("{base_url}/users/docs/{doc_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Verify deleted
    let resp = client
        .get(format!("{base_url}/users/docs/{doc_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn test_bulk_insert() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "logs"}))
        .send()
        .await
        .unwrap();

    let docs = json!({
        "documents": [
            {"level": "info", "msg": "started"},
            {"level": "warn", "msg": "slow query"},
            {"level": "error", "msg": "connection lost"},
        ]
    });

    let resp = client
        .post(format!("{base_url}/logs/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["inserted"], 3);

    // Verify count
    let resp = client.get(format!("{base_url}/logs")).send().await.unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["doc_count"], 3);
}

#[tokio::test]
async fn test_query_filter_and_sort() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "products"}))
        .send()
        .await
        .unwrap();

    // Insert test data
    let docs = json!({
        "documents": [
            {"name": "Apple", "price": 1.5, "category": "fruit"},
            {"name": "Banana", "price": 0.5, "category": "fruit"},
            {"name": "Carrot", "price": 0.8, "category": "vegetable"},
            {"name": "Donut", "price": 2.0, "category": "pastry"},
            {"name": "Eggplant", "price": 1.2, "category": "vegetable"},
        ]
    });

    client
        .post(format!("{base_url}/products/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    // Query: filter by category
    let resp = client
        .post(format!("{base_url}/products/query"))
        .json(&json!({"filter": {"category": "fruit"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 2);

    // Query: comparison operator
    let resp = client
        .post(format!("{base_url}/products/query"))
        .json(&json!({"filter": {"price": {"$gt": 1.0}}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 3); // Apple, Donut, Eggplant

    // Query: sort by price descending
    let resp = client
        .post(format!("{base_url}/products/query"))
        .json(&json!({
            "sort": [{"price": "desc"}],
            "limit": 2
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 2);
    assert_eq!(docs[0]["name"], "Donut");
    assert_eq!(docs[1]["name"], "Apple");

    // Query: count_only
    let resp = client
        .post(format!("{base_url}/products/query"))
        .json(&json!({"count_only": true}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["count"], 5);

    // Query: projection
    let resp = client
        .post(format!("{base_url}/products/query"))
        .json(&json!({
            "filter": {"category": "fruit"},
            "fields": ["name"]
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let docs = body["data"].as_array().unwrap();
    for doc in docs {
        assert!(doc.get("_id").is_some());
        assert!(doc.get("name").is_some());
        assert!(doc.get("price").is_none());
    }
}

#[tokio::test]
async fn test_query_logical_operators() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();

    let docs = json!({
        "documents": [
            {"name": "A", "x": 1, "y": 10},
            {"name": "B", "x": 2, "y": 20},
            {"name": "C", "x": 3, "y": 30},
        ]
    });
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    // $or
    let resp = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"filter": {"$or": [{"x": 1}, {"x": 3}]}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 2);

    // $and
    let resp = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"filter": {"$and": [{"x": {"$gte": 2}}, {"y": {"$lte": 20}}]}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 1);

    // $not
    let resp = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"filter": {"$not": {"x": 2}}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 2);
}

#[tokio::test]
async fn test_nested_field_query() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let docs = json!({
        "documents": [
            {"event_type": "firewall", "network": {"src_ip": "10.0.0.1", "dst_port": 22}},
            {"event_type": "firewall", "network": {"src_ip": "10.0.0.2", "dst_port": 80}},
            {"event_type": "auth", "user": {"name": "admin"}},
        ]
    });
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    // Query nested field with dot notation
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"network.dst_port": 22}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 1);
    assert_eq!(body["data"][0]["network"]["src_ip"], "10.0.0.1");
}

#[tokio::test]
async fn test_error_cases() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Missing collection
    let resp = client
        .get(format!("{base_url}/nonexistent"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["code"], "COLLECTION_NOT_FOUND");

    // Insert into missing collection
    let resp = client
        .post(format!("{base_url}/nonexistent/docs"))
        .json(&json!({"foo": "bar"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Create collection then get missing doc
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "test"}))
        .send()
        .await
        .unwrap();

    let resp = client
        .get(format!("{base_url}/test/docs/nonexistent-id"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "DOCUMENT_NOT_FOUND");
}

#[tokio::test]
async fn test_pagination() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "pages"}))
        .send()
        .await
        .unwrap();

    // Insert 10 docs
    let docs: Vec<Value> = (0..10).map(|i| json!({"num": i})).collect();
    client
        .post(format!("{base_url}/pages/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Get first page
    let resp = client
        .post(format!("{base_url}/pages/query"))
        .json(&json!({"limit": 3, "offset": 0}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 10);
    assert_eq!(body["meta"]["returned_count"], 3);

    // Get second page
    let resp = client
        .post(format!("{base_url}/pages/query"))
        .json(&json!({"limit": 3, "offset": 3}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["returned_count"], 3);
}

#[tokio::test]
async fn test_invalid_json_returns_error_envelope() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "test"}))
        .send()
        .await
        .unwrap();

    // Send invalid JSON body
    let resp = client
        .post(format!("{base_url}/test/docs"))
        .header("content-type", "application/json")
        .body("not valid json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["code"], "INVALID_DOCUMENT");

    // Send non-object JSON
    let resp = client
        .post(format!("{base_url}/test/docs"))
        .json(&json!("just a string"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
}

#[tokio::test]
async fn test_collection_name_validation() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Underscore prefix not allowed
    let resp = client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "_internal"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Empty name not allowed
    let resp = client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": ""}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Invalid characters not allowed
    let resp = client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "foo:bar"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Valid name works
    let resp = client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "valid-name.123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
}

#[tokio::test]
async fn test_request_id_header() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    let resp = client
        .get(format!("{base_url}/_health"))
        .send()
        .await
        .unwrap();
    let request_id = resp
        .headers()
        .get("x-request-id")
        .expect("Missing x-request-id header");
    let id_str = request_id.to_str().unwrap();
    // UUIDv7 format: 8-4-4-4-12
    assert_eq!(id_str.len(), 36);
    assert!(id_str.contains('-'));
}

#[tokio::test]
async fn test_nested_field_sort() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let docs = json!({
        "documents": [
            {"event_type": "firewall", "network": {"src_ip": "10.0.0.1", "dst_port": 443}},
            {"event_type": "firewall", "network": {"src_ip": "10.0.0.2", "dst_port": 22}},
            {"event_type": "firewall", "network": {"src_ip": "10.0.0.3", "dst_port": 8080}},
            {"event_type": "auth", "user": {"name": "admin"}},
        ]
    });
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    // Sort by nested field ascending
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "firewall"},
            "sort": [{"network.dst_port": "asc"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 3);
    assert_eq!(docs[0]["network"]["dst_port"], 22);
    assert_eq!(docs[1]["network"]["dst_port"], 443);
    assert_eq!(docs[2]["network"]["dst_port"], 8080);

    // Sort by nested field descending
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "firewall"},
            "sort": [{"network.dst_port": "desc"}]
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs[0]["network"]["dst_port"], 8080);
    assert_eq!(docs[1]["network"]["dst_port"], 443);
    assert_eq!(docs[2]["network"]["dst_port"], 22);

    // Documents missing the sort field should sort to the beginning (None < Some)
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "sort": [{"network.dst_port": "asc"}]
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 4);
    // The auth event has no network.dst_port, should be first in ascending order
    assert_eq!(docs[0]["event_type"], "auth");
}

#[tokio::test]
async fn test_bulk_insert_partial_success() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Mix valid docs with invalid ones (non-objects)
    let resp = client
        .post(format!("{base_url}/events/docs/_bulk"))
        .header("content-type", "application/json")
        .body(
            r#"{"documents": [
            {"event_type": "firewall", "severity": "high"},
            "not an object",
            {"event_type": "dns", "query": "example.com"},
            42,
            {"event_type": "auth", "user": "admin"}
        ]}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();

    // 3 valid docs should be inserted
    assert_eq!(body["data"]["inserted"], 3);

    // 2 invalid docs should have per-doc errors
    let errors = body["data"]["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2);
    // Error messages should reference the document index
    assert!(errors[0].as_str().unwrap().contains("1"));
    assert!(errors[1].as_str().unwrap().contains("3"));

    // Verify the 3 valid docs are actually in the collection
    let resp = client
        .get(format!("{base_url}/events"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["doc_count"], 3);

    // Verify we can query the valid docs
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"event_type": "firewall"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 1);
}

#[tokio::test]
async fn test_request_tracing_logged() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Verify the server responds with proper headers on errors too
    let resp = client
        .get(format!("{base_url}/nonexistent"))
        .send()
        .await
        .unwrap();
    assert!(resp.headers().get("x-request-id").is_some());
}

#[tokio::test]
async fn test_aggregate_basic() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let docs = json!({
        "documents": [
            {"event_type": "firewall", "network": {"src_ip": "10.0.0.1", "action": "block"}, "severity": 8},
            {"event_type": "firewall", "network": {"src_ip": "10.0.0.1", "action": "block"}, "severity": 5},
            {"event_type": "firewall", "network": {"src_ip": "10.0.0.2", "action": "allow"}, "severity": 3},
            {"event_type": "dns", "query": "example.com", "severity": 1},
            {"event_type": "dns", "query": "test.com", "severity": 2},
            {"event_type": "auth", "user": "admin", "severity": 9},
        ]
    });
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    // Basic group by event_type with count
    let resp = client
        .post(format!("{base_url}/events/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$group": {
                    "_id": "event_type",
                    "count": {"$count": {}}
                }},
                {"$sort": {"count": "desc"}}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 3);
    // firewall has 3, dns has 2, auth has 1
    assert_eq!(data[0]["_id"], "firewall");
    assert_eq!(data[0]["count"], 3);
    assert_eq!(data[1]["_id"], "dns");
    assert_eq!(data[1]["count"], 2);
    assert_eq!(data[2]["_id"], "auth");
    assert_eq!(data[2]["count"], 1);
    // Meta should include groups count
    assert_eq!(body["meta"]["groups"], 3);
    assert_eq!(body["meta"]["docs_scanned"], 6);
}

#[tokio::test]
async fn test_aggregate_with_match_and_accumulators() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let docs = json!({
        "documents": [
            {"event_type": "firewall", "network": {"src_ip": "10.0.0.1", "action": "block"}, "severity": 8},
            {"event_type": "firewall", "network": {"src_ip": "10.0.0.1", "action": "block"}, "severity": 5},
            {"event_type": "firewall", "network": {"src_ip": "10.0.0.2", "action": "allow"}, "severity": 3},
            {"event_type": "dns", "query": "example.com", "severity": 1},
        ]
    });
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    // Match + group with $sum, $avg, $min, $max
    let resp = client
        .post(format!("{base_url}/events/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$match": {"event_type": "firewall"}},
                {"$group": {
                    "_id": "network.action",
                    "count": {"$count": {}},
                    "total_severity": {"$sum": "severity"},
                    "avg_severity": {"$avg": "severity"},
                    "min_severity": {"$min": "severity"},
                    "max_severity": {"$max": "severity"}
                }},
                {"$sort": {"count": "desc"}}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);

    // block: 2 events, severity 8+5=13, avg=6.5, min=5, max=8
    let block_group = &data[0];
    assert_eq!(block_group["_id"], "block");
    assert_eq!(block_group["count"], 2);
    assert_eq!(block_group["total_severity"], 13.0);
    assert_eq!(block_group["avg_severity"], 6.5);
    assert_eq!(block_group["min_severity"], 5);
    assert_eq!(block_group["max_severity"], 8);

    // allow: 1 event, severity 3
    let allow_group = &data[1];
    assert_eq!(allow_group["_id"], "allow");
    assert_eq!(allow_group["count"], 1);
}

#[tokio::test]
async fn test_aggregate_multi_field_group() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let docs = json!({
        "documents": [
            {"event_type": "firewall", "network": {"action": "block"}},
            {"event_type": "firewall", "network": {"action": "block"}},
            {"event_type": "firewall", "network": {"action": "allow"}},
            {"event_type": "dns", "network": {"action": "allow"}},
        ]
    });
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    // Multi-field _id grouping
    let resp = client
        .post(format!("{base_url}/events/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$group": {
                    "_id": {"type": "event_type", "action": "network.action"},
                    "count": {"$count": {}}
                }},
                {"$sort": {"count": "desc"}},
                {"$limit": 2}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    // firewall+block has 2 events
    assert_eq!(data[0]["_id"]["type"], "firewall");
    assert_eq!(data[0]["_id"]["action"], "block");
    assert_eq!(data[0]["count"], 2);
}

#[tokio::test]
async fn test_aggregate_null_id_groups_all() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let docs = json!({
        "documents": [
            {"severity": 10},
            {"severity": 20},
            {"severity": 30},
        ]
    });
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    // _id: null groups all documents into a single result
    let resp = client
        .post(format!("{base_url}/events/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$group": {
                    "_id": null,
                    "count": {"$count": {}},
                    "total": {"$sum": "severity"},
                    "avg": {"$avg": "severity"}
                }}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["_id"], Value::Null);
    assert_eq!(data[0]["count"], 3);
    assert_eq!(data[0]["total"], 60.0);
    assert_eq!(data[0]["avg"], 20.0);
}

#[tokio::test]
async fn test_aggregate_invalid_pipeline() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Empty pipeline
    let resp = client
        .post(format!("{base_url}/events/aggregate"))
        .json(&json!({"pipeline": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "INVALID_PIPELINE");

    // Unknown stage
    let resp = client
        .post(format!("{base_url}/events/aggregate"))
        .json(&json!({"pipeline": [{"$unknown": {}}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["error"]["code"],
        "INVALID_PIPELINE"
    );
}

// ===================== Phase 2: Index + Query Optimization Tests =====================

#[tokio::test]
async fn test_index_crud() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Create collection
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();

    // Create an index
    let resp = client
        .post(format!("{base_url}/items/indexes"))
        .json(&json!({"name": "idx_category", "field": "category"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());
    assert_eq!(body["data"]["name"], "idx_category");
    assert_eq!(body["data"]["field"], "category");

    // List indexes
    let resp = client
        .get(format!("{base_url}/items/indexes"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let indexes = body["data"].as_array().unwrap();
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0]["name"], "idx_category");

    // Duplicate index name returns 409
    let resp = client
        .post(format!("{base_url}/items/indexes"))
        .json(&json!({"name": "idx_category", "field": "other_field"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    // Drop index
    let resp = client
        .delete(format!("{base_url}/items/indexes/idx_category"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["data"]["dropped"].as_bool().unwrap());

    // List indexes after drop — empty
    let resp = client
        .get(format!("{base_url}/items/indexes"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_index_accelerates_query() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Setup: create collection + insert docs
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "products"}))
        .send()
        .await
        .unwrap();

    let mut docs = Vec::new();
    for i in 0..50 {
        let category = if i % 3 == 0 {
            "fruit"
        } else if i % 3 == 1 {
            "vegetable"
        } else {
            "dairy"
        };
        docs.push(json!({"name": format!("item_{i}"), "category": category, "price": i * 10}));
    }
    client
        .post(format!("{base_url}/products/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Create index on category
    client
        .post(format!("{base_url}/products/indexes"))
        .json(&json!({"name": "idx_category", "field": "category"}))
        .send()
        .await
        .unwrap();

    // Query using the indexed field
    let resp = client
        .post(format!("{base_url}/products/query"))
        .json(&json!({"filter": {"category": "fruit"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());
    assert_eq!(body["meta"]["index_used"], "idx_category");

    // All returned docs should have category == "fruit"
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 17); // ceil(50/3) = 17
    for doc in data {
        assert_eq!(doc["category"], "fruit");
    }
}

#[tokio::test]
async fn test_count_only_with_index() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Setup
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let mut docs = Vec::new();
    for i in 0..100 {
        let event_type = if i % 4 == 0 {
            "firewall"
        } else if i % 4 == 1 {
            "dns"
        } else if i % 4 == 2 {
            "dhcp"
        } else {
            "ids"
        };
        docs.push(json!({"event_type": event_type, "seq": i}));
    }
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Create index
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_event_type", "field": "event_type"}))
        .send()
        .await
        .unwrap();

    // Count-only query on indexed field
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"event_type": "firewall"}, "count_only": true}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());
    assert_eq!(body["data"]["count"], 25);
    assert_eq!(body["meta"]["index_used"], "idx_event_type");
    // docs_scanned should be 0 (index-only count)
    assert_eq!(body["meta"]["docs_scanned"], 0);
    assert_eq!(body["meta"]["scan_strategy"], "index_eq");
}

#[tokio::test]
async fn test_fast_stats() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Create collection + insert docs
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "test_stats"}))
        .send()
        .await
        .unwrap();

    // Stats should show 0 docs
    let resp = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["total_documents"], 0);

    // Insert 10 docs
    let docs: Vec<Value> = (0..10).map(|i| json!({"n": i})).collect();
    client
        .post(format!("{base_url}/test_stats/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Stats should show 10 docs
    let resp = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["total_documents"], 10);

    // Insert one more
    let resp = client
        .post(format!("{base_url}/test_stats/docs"))
        .json(&json!({"n": 99}))
        .send()
        .await
        .unwrap();
    let doc_id = resp.json::<Value>().await.unwrap()["data"]["_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Stats should show 11
    let resp = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["total_documents"], 11);

    // Delete one doc
    client
        .delete(format!("{base_url}/test_stats/docs/{doc_id}"))
        .send()
        .await
        .unwrap();

    // Stats should show 10 again
    let resp = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["total_documents"], 10);
}

#[tokio::test]
async fn test_delete_by_query() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Setup
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "logs"}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..20)
        .map(|i| {
            json!({
                "level": if i < 10 { "info" } else { "error" },
                "message": format!("log entry {i}")
            })
        })
        .collect();
    client
        .post(format!("{base_url}/logs/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Delete all "info" logs
    let resp = client
        .post(format!("{base_url}/logs/docs/_delete_by_query"))
        .json(&json!({"filter": {"level": "info"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());
    assert_eq!(body["data"]["deleted"], 10);

    // Verify remaining docs are all "error"
    let resp = client
        .post(format!("{base_url}/logs/query"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 10);
    for doc in body["data"].as_array().unwrap() {
        assert_eq!(doc["level"], "error");
    }
}

#[tokio::test]
async fn test_update_by_query() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Setup
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..10)
        .map(|i| {
            json!({
                "network": {"src_ip": if i < 5 { "1.2.3.4" } else { "5.6.7.8" }},
                "event_type": "firewall"
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Update all events from 1.2.3.4 with enrichment data
    let resp = client
        .post(format!("{base_url}/events/docs/_update_by_query"))
        .json(&json!({
            "filter": {"network.src_ip": "1.2.3.4"},
            "update": {
                "$set": {
                    "enrichment.src.geo_country": "US",
                    "enrichment.src.abuse_score": 42
                }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());
    assert_eq!(body["data"]["updated"], 5);

    // Verify the enrichment data was applied
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"network.src_ip": "1.2.3.4"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    for doc in body["data"].as_array().unwrap() {
        assert_eq!(doc["enrichment"]["src"]["geo_country"], "US");
        assert_eq!(doc["enrichment"]["src"]["abuse_score"], 42);
        // _rev should be incremented to 2
        assert_eq!(doc["_rev"], 2);
    }

    // Verify the other events were NOT updated
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"network.src_ip": "5.6.7.8"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    for doc in body["data"].as_array().unwrap() {
        assert!(doc.get("enrichment").is_none());
        assert_eq!(doc["_rev"], 1);
    }
}

#[tokio::test]
async fn test_index_maintained_on_update() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Setup
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "tasks"}))
        .send()
        .await
        .unwrap();

    // Create index on status
    client
        .post(format!("{base_url}/tasks/indexes"))
        .json(&json!({"name": "idx_status", "field": "status"}))
        .send()
        .await
        .unwrap();

    // Insert a doc with status "active"
    let resp = client
        .post(format!("{base_url}/tasks/docs"))
        .json(&json!({"status": "active", "name": "task1"}))
        .send()
        .await
        .unwrap();
    let doc_id = resp.json::<Value>().await.unwrap()["data"]["_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Query for active — should find it via index
    let resp = client
        .post(format!("{base_url}/tasks/query"))
        .json(&json!({"filter": {"status": "active"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 1);
    assert_eq!(body["meta"]["index_used"], "idx_status");

    // Update status to "inactive"
    client
        .patch(format!("{base_url}/tasks/docs/{doc_id}"))
        .json(&json!({"status": "inactive"}))
        .send()
        .await
        .unwrap();

    // Query for active — should find 0
    let resp = client
        .post(format!("{base_url}/tasks/query"))
        .json(&json!({"filter": {"status": "active"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 0);

    // Query for inactive — should find 1
    let resp = client
        .post(format!("{base_url}/tasks/query"))
        .json(&json!({"filter": {"status": "inactive"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 1);
    assert_eq!(body["meta"]["index_used"], "idx_status");
}

#[tokio::test]
async fn test_index_backfill() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Setup: insert docs FIRST, then create index
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..30)
        .map(|i| {
            json!({
                "color": if i % 3 == 0 { "red" } else if i % 3 == 1 { "blue" } else { "green" },
                "n": i
            })
        })
        .collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Now create index — should backfill existing docs
    let resp = client
        .post(format!("{base_url}/items/indexes"))
        .json(&json!({"name": "idx_color", "field": "color"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    // Query using the index
    let resp = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"filter": {"color": "red"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["index_used"], "idx_color");
    assert_eq!(body["meta"]["total_count"], 10); // 30/3 = 10 red items
}

#[tokio::test]
async fn test_index_with_compound_filter() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Setup
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let mut docs = Vec::new();
    for i in 0..40 {
        docs.push(json!({
            "event_type": if i % 2 == 0 { "firewall" } else { "dns" },
            "severity": if i % 4 == 0 { "high" } else { "low" },
            "seq": i
        }));
    }
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Create index on event_type
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_event_type", "field": "event_type"}))
        .send()
        .await
        .unwrap();

    // Compound filter: event_type (indexed) + severity (not indexed)
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {
                "event_type": "firewall",
                "severity": "high"
            }
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());
    // Index should be used for event_type, severity applied as post-filter
    assert_eq!(body["meta"]["index_used"], "idx_event_type");
    // 20 firewall events, 10 are high severity (every 4th out of 40 total)
    assert_eq!(body["meta"]["total_count"], 10);

    for doc in body["data"].as_array().unwrap() {
        assert_eq!(doc["event_type"], "firewall");
        assert_eq!(doc["severity"], "high");
    }
}

// ===================== Phase 2.5: Compound Indexes + Aggregation Index Tests =====================

#[tokio::test]
async fn test_compound_index_creation() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Insert docs
    let docs: Vec<Value> = (0..30)
        .map(|i| {
            json!({
                "event_type": if i % 3 == 0 { "firewall" } else if i % 3 == 1 { "dns" } else { "ids" },
                "severity": i % 5,
                "seq": i
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Create compound index using `fields` array
    let resp = client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({
            "name": "idx_type_severity",
            "fields": ["event_type", "severity"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());
    assert_eq!(body["data"]["name"], "idx_type_severity");
    let fields = body["data"]["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0], "event_type");
    assert_eq!(fields[1], "severity");

    // List indexes shows the compound index
    let resp = client
        .get(format!("{base_url}/events/indexes"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let indexes = body["data"].as_array().unwrap();
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0]["name"], "idx_type_severity");

    // A query on the first field ALONE is correct but not served from the
    // compound index (F2: compound indexes exclude docs missing other
    // components, so the old leading-field fallback could return wrong
    // results; single-field lookups now require a single-field index).
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"event_type": "firewall"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());
    assert_eq!(body["meta"]["index_used"], Value::Null);
    assert_eq!(body["meta"]["total_count"], 10); // 30/3 = 10 firewall

    // Both fields — the compound index's real contract (CompoundEq).
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"event_type": "firewall", "severity": 0}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["index_used"], "idx_type_severity");
    assert_eq!(body["meta"]["total_count"], 2); // firewall docs i ∈ {0, 15}
}

#[tokio::test]
async fn test_aggregate_uses_index_for_match() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Insert a mix of events
    let mut docs = Vec::new();
    for i in 0..60 {
        docs.push(json!({
            "event_type": if i % 3 == 0 { "firewall" } else if i % 3 == 1 { "dns" } else { "ids" },
            "severity": i % 5,
            "network": {"action": if i % 2 == 0 { "block" } else { "allow" }}
        }));
    }
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Create index on event_type
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_event_type", "field": "event_type"}))
        .send()
        .await
        .unwrap();

    // Aggregation with $match as first stage should use the index
    let resp = client
        .post(format!("{base_url}/events/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$match": {"event_type": "firewall"}},
                {"$group": {
                    "_id": "network.action",
                    "count": {"$count": {}}
                }},
                {"$sort": {"count": "desc"}}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());

    // Should use the index
    assert_eq!(body["meta"]["index_used"], "idx_event_type");
    // docs_scanned should be 20 (only firewall events), not 60
    assert_eq!(body["meta"]["docs_scanned"], 20);

    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2); // block and allow groups
    // Total count should be 20 (all firewall events)
    let total: u64 = data.iter().map(|d| d["count"].as_u64().unwrap()).sum();
    assert_eq!(total, 20);
}

#[tokio::test]
async fn test_compound_index_maintained_on_crud() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "tasks"}))
        .send()
        .await
        .unwrap();

    // Create compound index
    client
        .post(format!("{base_url}/tasks/indexes"))
        .json(&json!({
            "name": "idx_status_priority",
            "fields": ["status", "priority"]
        }))
        .send()
        .await
        .unwrap();

    // Insert a document
    let resp = client
        .post(format!("{base_url}/tasks/docs"))
        .json(&json!({"status": "active", "priority": 1, "title": "task1"}))
        .send()
        .await
        .unwrap();
    let doc_id = resp.json::<Value>().await.unwrap()["data"]["_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Both-field query exercises the compound index (leading-field-only
    // queries are no longer served from it — F2).
    let resp = client
        .post(format!("{base_url}/tasks/query"))
        .json(&json!({"filter": {"status": "active", "priority": 1}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["index_used"], "idx_status_priority");
    assert_eq!(body["meta"]["total_count"], 1);

    // Update the document's status
    client
        .patch(format!("{base_url}/tasks/docs/{doc_id}"))
        .json(&json!({"status": "done"}))
        .send()
        .await
        .unwrap();

    // Old status should return 0
    let resp = client
        .post(format!("{base_url}/tasks/query"))
        .json(&json!({"filter": {"status": "active"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 0);

    // New status should return 1
    let resp = client
        .post(format!("{base_url}/tasks/query"))
        .json(&json!({"filter": {"status": "done"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 1);

    // ...and the compound path still finds it with both fields.
    let resp = client
        .post(format!("{base_url}/tasks/query"))
        .json(&json!({"filter": {"status": "done", "priority": 1}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 1);
    assert_eq!(body["meta"]["index_used"], "idx_status_priority");

    // Delete the document
    client
        .delete(format!("{base_url}/tasks/docs/{doc_id}"))
        .send()
        .await
        .unwrap();

    // Should return 0 now
    let resp = client
        .post(format!("{base_url}/tasks/query"))
        .json(&json!({"filter": {"status": "done"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["total_count"], 0);
}

// ===================== Phase 3 Tests =====================

#[tokio::test]
async fn test_received_at_on_insert() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Insert a document
    let resp = client
        .post(format!("{base_url}/events/docs"))
        .json(&json!({"event_type": "firewall"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    let doc = &body["data"];
    assert!(doc["_received_at"].is_string());
    let received_at = doc["_received_at"].as_str().unwrap().to_string();
    let doc_id = doc["_id"].as_str().unwrap().to_string();

    // PUT replace — _received_at should be preserved
    let resp = client
        .put(format!("{base_url}/events/docs/{doc_id}"))
        .json(&json!({"event_type": "dns", "new_field": true}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["_received_at"], received_at);
    assert_eq!(body["data"]["event_type"], "dns");

    // PATCH — _received_at should be preserved
    let resp = client
        .patch(format!("{base_url}/events/docs/{doc_id}"))
        .json(&json!({"event_type": "ids"}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["_received_at"], received_at);
    assert_eq!(body["data"]["event_type"], "ids");
}

#[tokio::test]
async fn test_received_at_on_bulk_insert() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let docs = json!({
        "documents": [
            {"event_type": "firewall"},
            {"event_type": "dns"},
        ]
    });
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    // Query all docs and verify _received_at
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    for doc in body["data"].as_array().unwrap() {
        assert!(doc["_received_at"].is_string());
    }
}

#[tokio::test]
async fn test_ttl_crud() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // No TTL initially
    let resp = client
        .get(format!("{base_url}/events/ttl"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["enabled"], false);

    // Set TTL
    let resp = client
        .put(format!("{base_url}/events/ttl"))
        .json(&json!({"retention_days": 30, "field": "_created_at"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["retention_days"], 30);
    assert_eq!(body["data"]["field"], "_created_at");
    assert_eq!(body["data"]["enabled"], true);

    // Get TTL
    let resp = client
        .get(format!("{base_url}/events/ttl"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["retention_days"], 30);
    assert_eq!(body["data"]["enabled"], true);

    // Delete TTL
    let resp = client
        .delete(format!("{base_url}/events/ttl"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Verify deleted
    let resp = client
        .get(format!("{base_url}/events/ttl"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["enabled"], false);
}

#[tokio::test]
async fn test_ttl_in_stats() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Set TTL
    client
        .put(format!("{base_url}/events/ttl"))
        .json(&json!({"retention_days": 7}))
        .send()
        .await
        .unwrap();

    // Stats should show active TTL policies
    let resp = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["ttl"]["active_policies"], 1);
}

#[tokio::test]
async fn test_api_key_auth() {
    let keys = vec!["test-key-123".to_string(), "another-key".to_string()];
    let (base_url, _tmp) = start_test_server_with_keys(keys).await;
    let client = Client::new();

    // No key → 401
    let resp = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");

    // Invalid key → 401
    let resp = client
        .get(format!("{base_url}/_stats"))
        .header("Authorization", "Bearer wrong-key")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Valid key via Authorization header → 200
    let resp = client
        .get(format!("{base_url}/_stats"))
        .header("Authorization", "Bearer test-key-123")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Valid key via X-API-Key header → 200
    let resp = client
        .get(format!("{base_url}/_stats"))
        .header("X-API-Key", "another-key")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // /_health is always accessible without auth
    let resp = client
        .get(format!("{base_url}/_health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_collect_accumulator() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = vec![
        json!({"src_ip": "10.0.0.1", "dst_port": 80}),
        json!({"src_ip": "10.0.0.1", "dst_port": 443}),
        json!({"src_ip": "10.0.0.1", "dst_port": 80}), // duplicate port
        json!({"src_ip": "10.0.0.2", "dst_port": 22}),
        json!({"src_ip": "10.0.0.2", "dst_port": 443}),
    ];
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{base_url}/events/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$group": {
                    "_id": "src_ip",
                    "ports": {"$collect": "dst_port"},
                    "count": {"$count": {}}
                }},
                {"$sort": {"count": "desc"}}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let data = body["data"].as_array().unwrap();

    // 10.0.0.1 has 3 events, unique ports: [80, 443]
    let ip1 = &data[0];
    assert_eq!(ip1["_id"], "10.0.0.1");
    assert_eq!(ip1["count"], 3);
    let ports = ip1["ports"].as_array().unwrap();
    assert_eq!(ports.len(), 2); // deduplicated

    // 10.0.0.2 has 2 events, unique ports: [22, 443]
    let ip2 = &data[1];
    assert_eq!(ip2["_id"], "10.0.0.2");
    assert_eq!(ip2["count"], 2);
    let ports = ip2["ports"].as_array().unwrap();
    assert_eq!(ports.len(), 2);
}

#[tokio::test]
async fn test_distinct_endpoint() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..30)
        .map(|i| {
            json!({
                "event_type": match i % 3 { 0 => "firewall", 1 => "dns", _ => "ids" },
                "severity": i % 5,
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Distinct without filter
    let resp = client
        .post(format!("{base_url}/events/distinct"))
        .json(&json!({"field": "event_type"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["count"], 3);
    assert_eq!(body["data"]["truncated"], false);

    // Distinct with filter
    let resp = client
        .post(format!("{base_url}/events/distinct"))
        .json(&json!({"field": "severity", "filter": {"event_type": "firewall"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    // firewall events: i=0,3,6,9,12,15,18,21,24,27 → severities: 0,3,1,4,2,0,3,1,4,2 → unique: 0,1,2,3,4
    assert_eq!(body["data"]["count"], 5);

    // Distinct with limit
    let resp = client
        .post(format!("{base_url}/events/distinct"))
        .json(&json!({"field": "event_type", "limit": 2}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["count"], 2);
    assert_eq!(body["data"]["truncated"], true);
}

#[tokio::test]
async fn test_distinct_with_index() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..20)
        .map(|i| json!({"event_type": match i % 3 { 0 => "firewall", 1 => "dns", _ => "ids" }}))
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Create index
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_event_type", "field": "event_type"}))
        .send()
        .await
        .unwrap();

    // Distinct on indexed field — should use index
    let resp = client
        .post(format!("{base_url}/events/distinct"))
        .json(&json!({"field": "event_type"}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["count"], 3);
    assert_eq!(body["meta"]["docs_scanned"], 0); // index-only scan
    assert_eq!(body["meta"]["index_used"], "idx_event_type");
}

#[tokio::test]
async fn test_storage_info_endpoint() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Empty collection
    let resp = client
        .get(format!("{base_url}/events/storage"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["doc_count"], 0);
    assert!(body["data"]["oldest_doc"].is_null());
    assert!(body["data"]["newest_doc"].is_null());

    // Insert some docs
    let docs: Vec<Value> = (0..10).map(|i| json!({"n": i})).collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Create index
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_n", "field": "n"}))
        .send()
        .await
        .unwrap();

    // Set TTL
    client
        .put(format!("{base_url}/events/ttl"))
        .json(&json!({"retention_days": 30}))
        .send()
        .await
        .unwrap();

    let resp = client
        .get(format!("{base_url}/events/storage"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["doc_count"], 10);
    assert_eq!(body["data"]["index_count"], 1);
    assert!(body["data"]["oldest_doc"].is_string());
    assert!(body["data"]["newest_doc"].is_string());
    assert_eq!(body["data"]["ttl"]["retention_days"], 30);
}

#[tokio::test]
async fn test_prometheus_metrics() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    let resp = client
        .get(format!("{base_url}/_metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_type.contains("text/plain"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("wardsondb_uptime_seconds"));
    assert!(body.contains("wardsondb_documents_total"));
    assert!(body.contains("wardsondb_requests_total"));
    assert!(body.contains("wardsondb_storage_poisoned 0"));
}

// ── Optimization 1: IndexSorted — early termination with compound index ──

#[tokio::test]
async fn test_index_sorted_compound_scan() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Create collection
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Create compound index on [event_type, received_at]
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_time", "fields": ["event_type", "received_at"]}))
        .send()
        .await
        .unwrap();

    // Insert documents with varying received_at
    let docs: Vec<Value> = (0..20)
        .map(|i| {
            json!({
                "event_type": if i < 15 { "firewall" } else { "dns" },
                "received_at": format!("2026-03-{:02}T00:00:00Z", i + 1),
                "severity": if i % 2 == 0 { "high" } else { "low" }
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Query: event_type=firewall, sort by received_at desc, limit 5
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "firewall"},
            "sort": [{"received_at": "desc"}],
            "limit": 5
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);

    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 5);

    // Verify scan_strategy is index_sorted
    assert_eq!(body["meta"]["scan_strategy"], "index_sorted");
    assert_eq!(body["meta"]["index_used"], "idx_type_time");

    // Verify results are in desc order of received_at
    for i in 0..data.len() - 1 {
        let a = data[i]["received_at"].as_str().unwrap();
        let b = data[i + 1]["received_at"].as_str().unwrap();
        assert!(a >= b, "Results not in desc order: {a} vs {b}");
    }

    // All should be firewall
    for doc in data {
        assert_eq!(doc["event_type"], "firewall");
    }

    // has_more should be true (15 firewall docs, only returned 5)
    assert_eq!(body["meta"]["has_more"], true);

    // total_count should be null (unknown with early termination)
    assert!(body["meta"]["total_count"].is_null());

    // docs_scanned should be small (much less than 15)
    let docs_scanned = body["meta"]["docs_scanned"].as_u64().unwrap();
    assert!(
        docs_scanned <= 10,
        "Expected early termination but scanned {docs_scanned}"
    );
}

#[tokio::test]
async fn test_index_sorted_asc_with_offset() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_time", "fields": ["event_type", "received_at"]}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..10)
        .map(|i| {
            json!({
                "event_type": "firewall",
                "received_at": format!("2026-03-{:02}T00:00:00Z", i + 1),
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Query: asc sort, offset 3, limit 3
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "firewall"},
            "sort": [{"received_at": "asc"}],
            "limit": 3,
            "offset": 3
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["meta"]["scan_strategy"], "index_sorted");

    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 3);

    // With offset 3 in asc order, should get days 4, 5, 6
    assert!(data[0]["received_at"].as_str().unwrap().contains("03-04"));
    assert!(data[1]["received_at"].as_str().unwrap().contains("03-05"));
    assert!(data[2]["received_at"].as_str().unwrap().contains("03-06"));
}

#[tokio::test]
async fn test_index_sorted_correctness() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_time", "fields": ["event_type", "received_at"]}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..30)
        .map(|i| {
            json!({
                "event_type": if i % 3 == 0 { "firewall" } else { "dns" },
                "received_at": format!("2026-03-{:02}T{:02}:00:00Z", (i / 24) + 1, i % 24),
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Get all firewall docs via regular query (no sort = full scan)
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "firewall"},
            "sort": [{"received_at": "desc"}],
            "limit": 100
        }))
        .send()
        .await
        .unwrap();
    let full_body: Value = resp.json().await.unwrap();
    let full_data = full_body["data"].as_array().unwrap();

    // Get top 5 via index_sorted
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "firewall"},
            "sort": [{"received_at": "desc"}],
            "limit": 5
        }))
        .send()
        .await
        .unwrap();
    let sorted_body: Value = resp.json().await.unwrap();
    let sorted_data = sorted_body["data"].as_array().unwrap();

    // The first 5 from full scan should match the 5 from index_sorted
    let full_ids: Vec<&str> = full_data
        .iter()
        .take(5)
        .map(|d| d["_id"].as_str().unwrap())
        .collect();
    let sorted_ids: Vec<&str> = sorted_data
        .iter()
        .map(|d| d["_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        full_ids, sorted_ids,
        "IndexSorted results must match full scan results"
    );
}

// ── Optimization 2: Index-only aggregation ──

#[tokio::test]
async fn test_index_only_aggregate_count() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_event_type", "field": "event_type"}))
        .send()
        .await
        .unwrap();

    // Insert docs with different event types
    let docs: Vec<Value> = vec![
        json!({"event_type": "firewall", "x": 1}),
        json!({"event_type": "firewall", "x": 2}),
        json!({"event_type": "firewall", "x": 3}),
        json!({"event_type": "dns", "x": 4}),
        json!({"event_type": "dns", "x": 5}),
        json!({"event_type": "ids", "x": 6}),
    ];
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Aggregate: group by event_type, count
    let resp = client
        .post(format!("{base_url}/events/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$group": {"_id": "event_type", "count": {"$count": {}}}},
                {"$sort": {"count": "desc"}}
            ]
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);

    // Should use index-only aggregate
    assert_eq!(body["meta"]["scan_strategy"], "index_only_aggregate");
    assert_eq!(body["meta"]["docs_scanned"], 0);
    assert_eq!(body["meta"]["index_used"], "idx_event_type");

    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 3);
    // Sorted desc by count: firewall=3, dns=2, ids=1
    assert_eq!(data[0]["_id"], "firewall");
    assert_eq!(data[0]["count"], 3);
    assert_eq!(data[1]["_id"], "dns");
    assert_eq!(data[1]["count"], 2);
    assert_eq!(data[2]["_id"], "ids");
    assert_eq!(data[2]["count"], 1);
}

#[tokio::test]
async fn test_index_only_aggregate_fallback() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_event_type", "field": "event_type"}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = vec![
        json!({"event_type": "firewall", "score": 10}),
        json!({"event_type": "firewall", "score": 20}),
        json!({"event_type": "dns", "score": 5}),
    ];
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Aggregate with $sum — should fall back to standard path (can't do $sum from index)
    let resp = client
        .post(format!("{base_url}/events/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$group": {"_id": "event_type", "count": {"$count": {}}, "total_score": {"$sum": "score"}}},
                {"$sort": {"count": "desc"}}
            ]
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);

    // Should NOT use index-only aggregate (has $sum)
    assert!(body["meta"]["scan_strategy"].is_null());
    assert!(body["meta"]["docs_scanned"].as_u64().unwrap() > 0);
}

// ── Optimization 3: Compound index prefix for multi-field AND ──

#[tokio::test]
async fn test_compound_eq_multi_field() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Create compound index on [event_type, action]
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_action", "fields": ["event_type", "action"]}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = vec![
        json!({"event_type": "firewall", "action": "block", "src": "1.2.3.4"}),
        json!({"event_type": "firewall", "action": "allow", "src": "1.2.3.5"}),
        json!({"event_type": "firewall", "action": "block", "src": "1.2.3.6"}),
        json!({"event_type": "dns", "action": "block", "src": "1.2.3.7"}),
        json!({"event_type": "dns", "action": "allow", "src": "1.2.3.8"}),
    ];
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Query: event_type=firewall AND action=block — should use compound index
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "firewall", "action": "block"},
            "count_only": true
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["count"], 2);
    assert_eq!(body["meta"]["index_used"], "idx_type_action");
    assert_eq!(body["meta"]["scan_strategy"], "compound_eq");
    assert_eq!(body["meta"]["docs_scanned"], 0);
}

#[tokio::test]
async fn test_compound_eq_with_post_filter() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Compound index on [event_type, action]
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_action", "fields": ["event_type", "action"]}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = vec![
        json!({"event_type": "firewall", "action": "block", "severity": "high"}),
        json!({"event_type": "firewall", "action": "block", "severity": "low"}),
        json!({"event_type": "firewall", "action": "block", "severity": "high"}),
    ];
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Query: event_type=firewall AND action=block AND severity=high
    // Compound covers first two, severity is post-filter
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "firewall", "action": "block", "severity": "high"}
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["meta"]["total_count"], 2);
    assert_eq!(body["meta"]["index_used"], "idx_type_action");
}

// =============================================================================
// Security hardening tests (50-57)
// =============================================================================

/// Test 50: Regex DoS — catastrophic backtracking pattern completes quickly
#[tokio::test]
async fn test_regex_dos_prevention() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Create collection and insert a document
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "regextest"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/regextest/docs"))
        .json(&json!({"name": "aaaaaaaaaaaaaaaaab"}))
        .send()
        .await
        .unwrap();

    // This pattern causes catastrophic backtracking in naive regex engines
    let start = Instant::now();
    let resp = client
        .post(format!("{base_url}/regextest/query"))
        .json(&json!({
            "filter": {"name": {"$regex": "^(a|ab)*(b|bb)*(c|cc)*x$"}}
        }))
        .send()
        .await
        .unwrap();
    let elapsed = start.elapsed();

    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    // Must complete in under 1 second (not hang)
    assert!(
        elapsed.as_millis() < 1000,
        "Regex query took too long: {:?}",
        elapsed
    );
}

/// Test 51: Query limit capped at the configured ceiling (default 100,000)
#[tokio::test]
async fn test_query_limit_cap() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "limittest"}))
        .send()
        .await
        .unwrap();

    // Insert a few docs
    for i in 0..5 {
        client
            .post(format!("{base_url}/limittest/docs"))
            .json(&json!({"n": i}))
            .send()
            .await
            .unwrap();
    }

    // Request with absurdly high limit — should be clamped
    let resp = client
        .post(format!("{base_url}/limittest/query"))
        .json(&json!({"limit": 999999}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    // All 5 docs returned (within 100,000 cap)
    assert_eq!(body["data"].as_array().unwrap().len(), 5);
}

/// Verifies `--max-query-limit` is wired through to the parser by setting
/// a small custom ceiling and confirming the clamp applies at that value.
#[tokio::test]
async fn test_query_limit_cap_configurable() {
    let (base_url, _tmp) = start_test_server_with_max_query_limit(3).await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "cfglimit"}))
        .send()
        .await
        .unwrap();

    for i in 0..10 {
        client
            .post(format!("{base_url}/cfglimit/docs"))
            .json(&json!({"n": i}))
            .send()
            .await
            .unwrap();
    }

    let resp = client
        .post(format!("{base_url}/cfglimit/query"))
        .json(&json!({"limit": 50}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"].as_array().unwrap().len(), 3);
}

/// Test 52: Bulk insert rejects more than 10,000 documents
#[tokio::test]
async fn test_bulk_insert_cap() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "bulkcap"}))
        .send()
        .await
        .unwrap();

    // Create 10,001 small docs
    let docs: Vec<Value> = (0..10_001).map(|i| json!({"n": i})).collect();
    let resp = client
        .post(format!("{base_url}/bulkcap/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert!(body["error"]["message"].as_str().unwrap().contains("10000"));
}

/// Test 53: Aggregation pipeline rejects more than 100 stages
#[tokio::test]
async fn test_pipeline_stage_cap() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "pipecap"}))
        .send()
        .await
        .unwrap();

    // Build 101 stages
    let stages: Vec<Value> = (0..101).map(|_| json!({"$limit": 10})).collect();
    let resp = client
        .post(format!("{base_url}/pipecap/aggregate"))
        .json(&json!({"pipeline": stages}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert!(body["error"]["message"].as_str().unwrap().contains("100"));
}

/// Test 54: Filter with too many $or branches is rejected
#[tokio::test]
async fn test_filter_branch_limit() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "branchtest"}))
        .send()
        .await
        .unwrap();

    // Build $or with 1001 branches
    let branches: Vec<Value> = (0..1001).map(|i| json!({"n": i})).collect();
    let resp = client
        .post(format!("{base_url}/branchtest/query"))
        .json(&json!({"filter": {"$or": branches}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert!(body["error"]["message"].as_str().unwrap().contains("1000"));
}

/// Test 55: Deeply nested filter is rejected
#[tokio::test]
async fn test_filter_depth_limit() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "depthtest"}))
        .send()
        .await
        .unwrap();

    // Build nested $not chain of depth 21
    let mut filter = json!({"x": 1});
    for _ in 0..21 {
        filter = json!({"$not": filter});
    }
    let resp = client
        .post(format!("{base_url}/depthtest/query"))
        .json(&json!({"filter": filter}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert!(body["error"]["message"].as_str().unwrap().contains("depth"));
}

/// Test 56: Dot-notation path depth limit
#[tokio::test]
async fn test_dot_notation_depth_limit() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "dottest"}))
        .send()
        .await
        .unwrap();

    // 22-level deep path (21 dots)
    let deep_path = "a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p.q.r.s.t.u.v";
    let resp = client
        .post(format!("{base_url}/dottest/query"))
        .json(&json!({"filter": {deep_path: 1}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert!(body["error"]["message"].as_str().unwrap().contains("depth"));
}

/// Test 57: Invalid regex pattern returns INVALID_QUERY at parse time
#[tokio::test]
async fn test_invalid_regex_pattern() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "regexerr"}))
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{base_url}/regexerr/query"))
        .json(&json!({"filter": {"name": {"$regex": "[invalid("} }}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["code"], "INVALID_QUERY");
}

/// Test 58: Health endpoint includes write_pressure field
#[tokio::test]
async fn test_health_write_pressure() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    let resp = client
        .get(format!("{base_url}/_health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["status"], "healthy");
    // write_pressure must always be present and "normal" under no load
    assert_eq!(body["data"]["write_pressure"], "normal");
}

// ── Bitmap Scan Accelerator Tests ──────────────────────────────────────────

/// Helper: create a collection and insert docs with known field values for bitmap tests.
async fn setup_bitmap_test_data(base_url: &str, client: &Client) {
    // Create collection
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Insert 20 docs with various categories and statuses
    let docs: Vec<Value> = (0..20)
        .map(|i| {
            let category = match i % 4 {
                0 => "firewall",
                1 => "dhcp",
                2 => "threat",
                _ => "system",
            };
            let status = if i % 2 == 0 { "active" } else { "inactive" };
            let severity = i % 5;
            json!({
                "category": category,
                "status": status,
                "severity": severity,
                "name": format!("event_{i}"),
            })
        })
        .collect();

    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();
}

/// Test 59: Bitmap scan equality filter
#[tokio::test]
async fn test_bitmap_scan_equality() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category,status").await;
    let client = Client::new();
    setup_bitmap_test_data(&base_url, &client).await;

    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"category": "firewall"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 5); // 20/4 = 5 firewall events
    assert_eq!(body["meta"]["scan_strategy"], "bitmap");
    for doc in docs {
        assert_eq!(doc["category"], "firewall");
    }
}

/// Test 60: Bitmap scan AND filter
#[tokio::test]
async fn test_bitmap_scan_and() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category,status").await;
    let client = Client::new();
    setup_bitmap_test_data(&base_url, &client).await;

    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"category": "firewall", "status": "active"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    let docs = body["data"].as_array().unwrap();
    // firewall = indices 0,4,8,12,16 → active = even indices → all firewall are at even indices
    // so all 5 firewall events are active
    assert_eq!(docs.len(), 5);
    assert_eq!(body["meta"]["scan_strategy"], "bitmap");
}

/// Test 61: Bitmap scan OR filter
#[tokio::test]
async fn test_bitmap_scan_or() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category,status").await;
    let client = Client::new();
    setup_bitmap_test_data(&base_url, &client).await;

    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {
                "$or": [
                    {"category": "firewall"},
                    {"category": "threat"}
                ]
            }
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 10); // 5 firewall + 5 threat
    assert_eq!(body["meta"]["scan_strategy"], "bitmap");
}

/// Test 62: Bitmap scan $ne filter
#[tokio::test]
async fn test_bitmap_scan_ne() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category,status").await;
    let client = Client::new();
    setup_bitmap_test_data(&base_url, &client).await;

    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"category": {"$ne": "firewall"}}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 15); // 20 - 5 firewall
    assert_eq!(body["meta"]["scan_strategy"], "bitmap");
}

/// Test 63: Bitmap scan $in filter
#[tokio::test]
async fn test_bitmap_scan_in() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category,status").await;
    let client = Client::new();
    setup_bitmap_test_data(&base_url, &client).await;

    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"category": {"$in": ["firewall", "dhcp"]}}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 10); // 5 firewall + 5 dhcp
    assert_eq!(body["meta"]["scan_strategy"], "bitmap");
}

/// Test 64: Bitmap count_only — zero doc reads
#[tokio::test]
async fn test_bitmap_count_only() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category,status").await;
    let client = Client::new();
    setup_bitmap_test_data(&base_url, &client).await;

    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"category": "firewall"}, "count_only": true}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["count"], 5);
    assert_eq!(body["meta"]["docs_scanned"], 0);
    assert_eq!(body["meta"]["scan_strategy"], "bitmap");
}

/// Test 65: Bitmap aggregate count — zero doc reads
#[tokio::test]
async fn test_bitmap_aggregate_count() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category,status").await;
    let client = Client::new();
    setup_bitmap_test_data(&base_url, &client).await;

    let resp = client
        .post(format!("{base_url}/events/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$group": {"_id": "category", "count": {"$count": {}}}}
            ]
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["meta"]["docs_scanned"], 0);
    assert_eq!(body["meta"]["scan_strategy"], "bitmap_aggregate");
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 4); // 4 categories
    // Verify all groups have count 5
    for doc in docs {
        assert_eq!(doc["count"], 5);
    }
}

/// Test 66: Bitmap filtered aggregate — $match + $group with $count
#[tokio::test]
async fn test_bitmap_filtered_aggregate() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category,status").await;
    let client = Client::new();
    setup_bitmap_test_data(&base_url, &client).await;

    let resp = client
        .post(format!("{base_url}/events/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$match": {"status": "active"}},
                {"$group": {"_id": "category", "count": {"$count": {}}}}
            ]
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["meta"]["docs_scanned"], 0);
    assert_eq!(body["meta"]["scan_strategy"], "bitmap_filtered_aggregate");
}

/// Test 67: Bitmap partial coverage — AND with one bitmap and one non-bitmap field
#[tokio::test]
async fn test_bitmap_partial_coverage() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();
    setup_bitmap_test_data(&base_url, &client).await;

    // "category" has a bitmap, "severity" does not
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"category": "firewall", "severity": 0}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    // firewall events at indices 0,4,8,12,16 → severity = i%5 → severity 0 at indices 0, 20 (not exist) → just index 0
    // Actually: idx 0: sev=0, idx 4: sev=4, idx 8: sev=3, idx 12: sev=2, idx 16: sev=1
    // So only index 0 has severity 0
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(body["meta"]["scan_strategy"], "bitmap");
}

/// S3-1 regression: a $or with PARTIAL bitmap coverage must not be served from
/// bitmaps — the plan's residual is conjunctive, so pre-fix this returned the
/// INTERSECTION (2 docs here) instead of the union. Partial coverage now falls
/// back to a full scan; fully-covered $or (test_bitmap_scan_or) stays on bitmap.
#[tokio::test]
async fn test_bitmap_or_partial_coverage_falls_back() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category,status").await;
    let client = Client::new();
    setup_bitmap_test_data(&base_url, &client).await;

    // "category" has a bitmap, "severity" does not.
    // firewall = indices {0,4,8,12,16}; severity>2 = i%5∈{3,4} = 8 docs;
    // overlap = {4,8} → union 11 (pre-fix bitmap path returned the overlap: 2).
    let mixed_or = json!({
        "$or": [
            {"category": "firewall"},
            {"severity": {"$gt": 2}}
        ]
    });

    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": mixed_or}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 11);
    // Doc-returning full scans don't label scan_strategy (S3-9); the
    // load-bearing assertion is that the bitmap path was NOT taken.
    assert_ne!(body["meta"]["scan_strategy"], "bitmap");

    // count_only takes the same planning decision (count paths ARE labeled).
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": mixed_or, "count_only": true}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["count"], 11);
    assert_eq!(body["meta"]["scan_strategy"], "full_scan");

    // A child that itself resolves only PARTIALLY (And with a non-bitmap
    // conjunct) must also force the bail: firewall∧sev>2 = {4,8}, threat =
    // {2,6,10,14,18} → union 7.
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {
                "$or": [
                    {"$and": [{"category": "firewall"}, {"severity": {"$gt": 2}}]},
                    {"category": "threat"}
                ]
            }
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 7);
    assert_ne!(body["meta"]["scan_strategy"], "bitmap");
}

/// Test 68: Bitmap correctness — compare bitmap scan results with full scan results
#[tokio::test]
async fn test_bitmap_correctness() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category,status").await;
    let client = Client::new();
    setup_bitmap_test_data(&base_url, &client).await;

    // Query with bitmap (should use bitmap scan)
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"category": "threat"}, "sort": [{"name": "asc"}]}))
        .send()
        .await
        .unwrap();
    let bitmap_body: Value = resp.json().await.unwrap();
    assert_eq!(bitmap_body["meta"]["scan_strategy"], "bitmap");

    // Also run a query that definitely uses full scan (non-bitmap field)
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"name": {"$regex": "^event_[28]$"}}, "sort": [{"name": "asc"}]}))
        .send()
        .await
        .unwrap();
    let full_body: Value = resp.json().await.unwrap();
    // Full scan for non-bitmap field
    assert!(
        full_body["meta"]["scan_strategy"].is_null()
            || full_body["meta"]["scan_strategy"] != "bitmap"
    );

    // Verify bitmap results are correct
    let bitmap_docs = bitmap_body["data"].as_array().unwrap();
    assert_eq!(bitmap_docs.len(), 5);
    for doc in bitmap_docs {
        assert_eq!(doc["category"], "threat");
    }
}

/// Test 69: Bitmap CRUD consistency — insert, update, delete maintain correct bitmap results
#[tokio::test]
async fn test_bitmap_crud_consistency() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();

    // Create collection
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();

    // Insert 3 docs
    let resp = client
        .post(format!("{base_url}/items/docs"))
        .json(&json!({"category": "A"}))
        .send()
        .await
        .unwrap();
    let doc_a: Value = resp.json().await.unwrap();
    let id_a = doc_a["data"]["_id"].as_str().unwrap().to_string();

    client
        .post(format!("{base_url}/items/docs"))
        .json(&json!({"category": "B"}))
        .send()
        .await
        .unwrap();

    client
        .post(format!("{base_url}/items/docs"))
        .json(&json!({"category": "A"}))
        .send()
        .await
        .unwrap();

    // Verify: 2 A's, 1 B
    let resp = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"filter": {"category": "A"}, "count_only": true}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["count"], 2);

    // Update first doc: A -> C
    client
        .patch(format!("{base_url}/items/docs/{id_a}"))
        .json(&json!({"category": "C"}))
        .send()
        .await
        .unwrap();

    // Verify: 1 A, 1 B, 1 C
    let resp = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"filter": {"category": "A"}, "count_only": true}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["count"], 1);

    let resp = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"filter": {"category": "C"}, "count_only": true}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["count"], 1);

    // Delete the C doc
    client
        .delete(format!("{base_url}/items/docs/{id_a}"))
        .send()
        .await
        .unwrap();

    // Verify: 1 A, 1 B, 0 C
    let resp = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"filter": {"category": "C"}, "count_only": true}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["count"], 0);
}

/// Test 70: Bitmap cardinality cap — field exceeding max_cardinality is not fully tracked
#[tokio::test]
async fn test_bitmap_cardinality_cap() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();

    // Insert docs — the category field has max_cardinality=1000 by default,
    // and we're inserting well under that limit, so bitmap should work
    for i in 0..10 {
        client
            .post(format!("{base_url}/items/docs"))
            .json(&json!({"category": format!("type_{i}")}))
            .send()
            .await
            .unwrap();
    }

    let resp = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"filter": {"category": "type_0"}, "count_only": true}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["count"], 1);
    assert_eq!(body["meta"]["scan_strategy"], "bitmap");
}

/// Test 71: Bitmap auto-detection — insert > sample_size docs, verify low-cardinality fields
/// get auto-detected. Uses bitmap_sample_size=100 in test config.
#[tokio::test]
async fn test_bitmap_auto_detection() {
    // Server WITHOUT explicit bitmap fields, sample window shrunk to 100 so
    // detection completes inside the test (the profiler target is what
    // --bitmap-sample-size wires in main.rs).
    let tmp = TempDir::new().unwrap();
    let storage = Storage::open(tmp.path()).unwrap();
    storage.scan_accelerator.set_sample_size(100);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let state = Arc::new(AppState {
        storage,
        config: test_config(&tmp, port),
        started_at: Instant::now(),
        metrics: Arc::new(Metrics::new()),
        api_keys: vec![],
    });
    let app = build_router(state);
    let addr = format!("127.0.0.1:{port}");
    let tcp = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tokio::spawn(async move {
        axum::serve(tcp, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let base_url = format!("http://{addr}");
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // 120 docs (> sample target of 100): docs 1..=100 are only PROFILED —
    // detection fires at #100, so any columns created then would be missing
    // them forever.
    let docs: Vec<Value> = (0..120)
        .map(|i| {
            json!({
                "event_type": match i % 3 { 0 => "A", 1 => "B", _ => "C" },
                "severity": i % 5,
                "unique_id": format!("uid_{i}"),
            })
        })
        .collect();
    for chunk in docs.chunks(60) {
        client
            .post(format!("{base_url}/events/docs/_bulk"))
            .json(&json!({"documents": chunk}))
            .send()
            .await
            .unwrap();
    }

    // Detection is recommendation-only: nothing activates, no columns exist.
    let body: Value = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let accel = &body["data"]["scan_accelerator"];
    assert_eq!(accel["ready"], false);
    assert_eq!(accel["bitmap_columns"].as_array().unwrap().len(), 0);

    // The old landmine: create_collection re-arms (set_ready) whenever
    // columns exist. With auto-created columns that meant serving bitmaps
    // missing every pre-detection doc — silent false negatives. Pin that a
    // later collection creation neither activates the accelerator nor
    // changes query results.
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "other"}))
        .send()
        .await
        .unwrap();

    let body: Value = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["data"]["scan_accelerator"]["ready"], false);

    let body: Value = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"event_type": "A"}, "count_only": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // 0..120 step 3 → 40 matches. Pre-fix this returned only the ~7
    // post-detection docs via scan_strategy "bitmap".
    assert_eq!(body["data"]["count"], 40);
    assert_eq!(body["meta"]["scan_strategy"], "full_scan");

    let body: Value = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"event_type": "A"}, "limit": 200}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 40);
    assert_ne!(body["meta"]["scan_strategy"], "bitmap");
}

/// Test 72: Bitmap persistence — build accelerator, check stats show data
#[tokio::test]
async fn test_bitmap_persistence() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();
    setup_bitmap_test_data(&base_url, &client).await;

    // Verify bitmap is populated
    let resp = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let accel = &body["data"]["scan_accelerator"];
    assert_eq!(accel["ready"], true);
    assert!(accel["total_positions"].as_u64().unwrap() > 0);
    let cols = accel["bitmap_columns"].as_array().unwrap();
    assert!(!cols.is_empty());
    // Find the category column
    let cat_col = cols.iter().find(|c| c["field"] == "category");
    assert!(cat_col.is_some());
    assert!(cat_col.unwrap()["cardinality"].as_u64().unwrap() > 0);
}

/// Test 73: Bitmap stats in /_stats endpoint
#[tokio::test]
async fn test_bitmap_stats() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category,status").await;
    let client = Client::new();
    setup_bitmap_test_data(&base_url, &client).await;

    let resp = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let accel = &body["data"]["scan_accelerator"];
    assert_eq!(accel["ready"], true);
    assert_eq!(accel["total_positions"], 20);
    let cols = accel["bitmap_columns"].as_array().unwrap();
    assert_eq!(cols.len(), 2); // category and status

    // Check health endpoint too
    let resp = client
        .get(format!("{base_url}/_health"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["scan_accelerator_ready"], true);
}

// ── Compound Range Tests ────────────────────────────────────────────────────

#[tokio::test]
async fn test_compound_range_basic() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Create collection
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Insert docs with category and timestamp
    for i in 0..20 {
        let category = if i % 2 == 0 { "A" } else { "B" };
        let ts = format!("2026-03-12T{:02}:00:00Z", i);
        client
            .post(format!("{base_url}/events/docs"))
            .json(&json!({"category": category, "timestamp": ts, "seq": i}))
            .send()
            .await
            .unwrap();
    }

    // Create compound index on (category, timestamp)
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_cat_ts", "fields": ["category", "timestamp"]}))
        .send()
        .await
        .unwrap();

    // Query: category = "A" AND timestamp >= "2026-03-12T10:00:00Z"
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {
                "category": "A",
                "timestamp": {"$gte": "2026-03-12T10:00:00Z"}
            }
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);

    let docs = body["data"].as_array().unwrap();
    // Category A docs: i=0,2,4,6,8,10,12,14,16,18
    // Timestamps >= 10:00: i=10,12,14,16,18 → 5 docs
    assert_eq!(docs.len(), 5);
    for doc in docs {
        assert_eq!(doc["category"], "A");
        assert!(doc["timestamp"].as_str().unwrap() >= "2026-03-12T10:00:00Z");
    }

    // Verify compound_range strategy is used
    assert_eq!(body["meta"]["scan_strategy"], "compound_range");
    assert_eq!(body["meta"]["index_used"], "idx_cat_ts");
}

#[tokio::test]
async fn test_compound_range_with_upper_bound() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    for i in 0..20 {
        let category = if i % 2 == 0 { "A" } else { "B" };
        let ts = format!("2026-03-12T{:02}:00:00Z", i);
        client
            .post(format!("{base_url}/events/docs"))
            .json(&json!({"category": category, "timestamp": ts}))
            .send()
            .await
            .unwrap();
    }

    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_cat_ts", "fields": ["category", "timestamp"]}))
        .send()
        .await
        .unwrap();

    // Query: category = "A" AND timestamp >= "2026-03-12T06:00:00Z" AND timestamp < "2026-03-12T14:00:00Z"
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {
                "$and": [
                    {"category": "A"},
                    {"timestamp": {"$gte": "2026-03-12T06:00:00Z"}},
                    {"timestamp": {"$lt": "2026-03-12T14:00:00Z"}}
                ]
            }
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);

    let docs = body["data"].as_array().unwrap();
    // Category A: i=0,2,4,6,8,10,12,14,16,18
    // ts >= 06:00 AND ts < 14:00 → i=6,8,10,12 → 4 docs
    assert_eq!(docs.len(), 4);
    for doc in docs {
        assert_eq!(doc["category"], "A");
        let ts = doc["timestamp"].as_str().unwrap();
        assert!(ts >= "2026-03-12T06:00:00Z");
        assert!(ts < "2026-03-12T14:00:00Z");
    }

    assert_eq!(body["meta"]["scan_strategy"], "compound_range");
}

#[tokio::test]
async fn test_compound_range_with_post_filter() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    for i in 0..20 {
        let category = if i % 2 == 0 { "A" } else { "B" };
        let status = if i % 3 == 0 { "active" } else { "inactive" };
        let ts = format!("2026-03-12T{:02}:00:00Z", i);
        client
            .post(format!("{base_url}/events/docs"))
            .json(&json!({"category": category, "timestamp": ts, "status": status}))
            .send()
            .await
            .unwrap();
    }

    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_cat_ts", "fields": ["category", "timestamp"]}))
        .send()
        .await
        .unwrap();

    // Query: category = "A" AND timestamp >= "2026-03-12T06:00:00Z" AND status = "active"
    // status is NOT in the compound index → post-filter
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {
                "$and": [
                    {"category": "A"},
                    {"timestamp": {"$gte": "2026-03-12T06:00:00Z"}},
                    {"status": "active"}
                ]
            }
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);

    let docs = body["data"].as_array().unwrap();
    // Category A with ts >= 06:00: i=6,8,10,12,14,16,18
    // Active (i%3==0): i=6,12,18 → 3 docs
    assert_eq!(docs.len(), 3);
    for doc in docs {
        assert_eq!(doc["category"], "A");
        assert_eq!(doc["status"], "active");
    }

    assert_eq!(body["meta"]["scan_strategy"], "compound_range");
}

#[tokio::test]
async fn test_compound_range_count_only() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    for i in 0..20 {
        let category = if i % 2 == 0 { "A" } else { "B" };
        let ts = format!("2026-03-12T{:02}:00:00Z", i);
        client
            .post(format!("{base_url}/events/docs"))
            .json(&json!({"category": category, "timestamp": ts}))
            .send()
            .await
            .unwrap();
    }

    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_cat_ts", "fields": ["category", "timestamp"]}))
        .send()
        .await
        .unwrap();

    // count_only: category = "A" AND timestamp >= "2026-03-12T10:00:00Z"
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {
                "category": "A",
                "timestamp": {"$gte": "2026-03-12T10:00:00Z"}
            },
            "count_only": true
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["count"], 5);
    assert_eq!(body["meta"]["docs_scanned"], 0);
    assert_eq!(body["meta"]["scan_strategy"], "compound_range");
}

#[tokio::test]
async fn test_compound_range_planner_priority() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    for i in 0..20 {
        let category = if i % 4 == 0 {
            "A"
        } else if i % 4 == 1 {
            "B"
        } else if i % 4 == 2 {
            "C"
        } else {
            "D"
        };
        let ts = format!("2026-03-12T{:02}:00:00Z", i);
        client
            .post(format!("{base_url}/events/docs"))
            .json(&json!({"category": category, "timestamp": ts}))
            .send()
            .await
            .unwrap();
    }

    // Create BOTH a single-field index on category AND a compound index
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_cat", "fields": ["category"]}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_cat_ts", "fields": ["category", "timestamp"]}))
        .send()
        .await
        .unwrap();

    // Query with eq + range: should prefer CompoundRange over single-field IndexEq
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {
                "category": "A",
                "timestamp": {"$gte": "2026-03-12T10:00:00Z"}
            }
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["meta"]["scan_strategy"], "compound_range");
    assert_eq!(body["meta"]["index_used"], "idx_cat_ts");

    // Verify docs_scanned is less than total for the category (compound range narrows)
    let total_cat_a = body["meta"]["total_count"].as_u64().unwrap();
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len() as u64, total_cat_a);
    // All returned docs should be category A with ts >= 10:00
    for doc in docs {
        assert_eq!(doc["category"], "A");
        assert!(doc["timestamp"].as_str().unwrap() >= "2026-03-12T10:00:00Z");
    }
}

// ── Storage Endpoint Fix Tests ──────────────────────────────────────────────

#[tokio::test]
async fn test_storage_info_empty_collection() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Create empty collection
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "empty"}))
        .send()
        .await
        .unwrap();

    // Storage info must return immediately (not hang) with null timestamps
    let start = std::time::Instant::now();
    let resp = client
        .get(format!("{base_url}/empty/storage"))
        .send()
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 2000,
        "Storage info on empty collection took {}ms, expected <2000ms",
        elapsed.as_millis()
    );

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["doc_count"], 0);
    assert_eq!(body["data"]["oldest_doc"], Value::Null);
    assert_eq!(body["data"]["newest_doc"], Value::Null);
}

#[tokio::test]
async fn test_storage_info_with_docs_and_index() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Create collection with docs and an index
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "logs"}))
        .send()
        .await
        .unwrap();

    for i in 0..5 {
        client
            .post(format!("{base_url}/logs/docs"))
            .json(&json!({"level": "info", "seq": i}))
            .send()
            .await
            .unwrap();
    }

    client
        .post(format!("{base_url}/logs/indexes"))
        .json(&json!({"name": "idx_level", "fields": ["level"]}))
        .send()
        .await
        .unwrap();

    // Storage info should return with valid data, not hang
    let start = std::time::Instant::now();
    let resp = client
        .get(format!("{base_url}/logs/storage"))
        .send()
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 2000,
        "Storage info took {}ms, expected <2000ms",
        elapsed.as_millis()
    );

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["doc_count"], 5);
    assert!(body["data"]["oldest_doc"].is_string());
    assert!(body["data"]["newest_doc"].is_string());
    assert_eq!(body["data"]["index_count"], 1);
    let indexes = body["data"]["indexes"].as_array().unwrap();
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0], "idx_level");
}

// ── Bitmap Fixes Tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_bitmap_drop_collection_clears_persistence() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();

    // Create collection and insert docs to populate bitmap
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    for i in 0..10 {
        let cat = if i % 2 == 0 { "A" } else { "B" };
        client
            .post(format!("{base_url}/events/docs"))
            .json(&json!({"category": cat}))
            .send()
            .await
            .unwrap();
    }

    // Verify bitmap is populated
    let resp = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let positions = body["data"]["scan_accelerator"]["total_positions"]
        .as_u64()
        .unwrap_or(0);
    assert!(positions > 0, "Bitmap should be populated before drop");

    // Drop the collection
    client
        .delete(format!("{base_url}/events"))
        .send()
        .await
        .unwrap();

    // Re-create with same name
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Verify bitmap state is clean — total_positions should be 0
    let resp = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let positions = body["data"]["scan_accelerator"]["total_positions"]
        .as_u64()
        .unwrap_or(0);
    assert_eq!(
        positions, 0,
        "Bitmap positions should be 0 after drop + re-create"
    );
}

#[tokio::test]
async fn test_bitmap_count_only_priority_over_index() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();

    // Create collection with both an index and bitmap on the same field
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    for i in 0..20 {
        let cat = if i % 4 == 0 {
            "A"
        } else if i % 4 == 1 {
            "B"
        } else if i % 4 == 2 {
            "C"
        } else {
            "D"
        };
        client
            .post(format!("{base_url}/events/docs"))
            .json(&json!({"category": cat}))
            .send()
            .await
            .unwrap();
    }

    // Create a secondary index on the same field
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_category", "fields": ["category"]}))
        .send()
        .await
        .unwrap();

    // count_only query should prefer bitmap over index
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"category": "A"},
            "count_only": true
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["count"], 5);
    assert_eq!(body["meta"]["docs_scanned"], 0);
    assert_eq!(
        body["meta"]["scan_strategy"], "bitmap",
        "count_only should prefer bitmap over index"
    );

    // Non-count_only query should still use the index
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"category": "A"},
            "limit": 50
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 5);
    // Should use index, not bitmap, for document-returning queries
    let strategy = body["meta"]["scan_strategy"].as_str().unwrap_or("");
    assert_ne!(
        strategy, "bitmap",
        "Non-count_only should not use bitmap when index available"
    );
}

#[tokio::test]
async fn test_bitmap_rearm_after_drop_recreate() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();

    // Create collection and insert docs
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    for i in 0..20 {
        let cat = if i % 2 == 0 { "A" } else { "B" };
        client
            .post(format!("{base_url}/events/docs"))
            .json(&json!({"category": cat}))
            .send()
            .await
            .unwrap();
    }

    // Verify bitmap is populated
    let resp = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let positions = body["data"]["scan_accelerator"]["total_positions"]
        .as_u64()
        .unwrap_or(0);
    assert_eq!(positions, 20, "Bitmap should have 20 positions before drop");

    // Drop collection
    client
        .delete(format!("{base_url}/events"))
        .send()
        .await
        .unwrap();

    // Recreate same collection and insert new docs
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    for i in 0..20 {
        let cat = if i % 4 == 0 {
            "X"
        } else if i % 4 == 1 {
            "Y"
        } else {
            "Z"
        };
        client
            .post(format!("{base_url}/events/docs"))
            .json(&json!({"category": cat}))
            .send()
            .await
            .unwrap();
    }

    // Verify bitmap is re-armed with new data
    let resp = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let positions = body["data"]["scan_accelerator"]["total_positions"]
        .as_u64()
        .unwrap_or(0);
    assert_eq!(
        positions, 20,
        "Bitmap should have 20 positions after drop + recreate + insert"
    );

    // Verify bitmap scan actually works
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"category": "X"},
            "count_only": true
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["count"], 5);
    assert_eq!(body["meta"]["scan_strategy"], "bitmap");
    assert_eq!(body["meta"]["docs_scanned"], 0);
}

// ============================================================
// Custom _id tests
// ============================================================

#[tokio::test]
async fn test_custom_id_insert() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // Create collection
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Insert with custom _id
    let resp = client
        .post(format!("{base_url}/events/docs"))
        .json(&json!({
            "_id": "evt-firewall-001",
            "event_type": "firewall",
            "action": "block"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["_id"], "evt-firewall-001");
    assert_eq!(body["data"]["_rev"], 1);
    assert!(body["data"]["_created_at"].is_string());
    assert!(body["data"]["_received_at"].is_string());

    // GET by custom ID
    let resp = client
        .get(format!("{base_url}/events/docs/evt-firewall-001"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["_id"], "evt-firewall-001");
    assert_eq!(body["data"]["event_type"], "firewall");
}

#[tokio::test]
async fn test_custom_id_duplicate_rejected() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // First insert
    let resp = client
        .post(format!("{base_url}/events/docs"))
        .json(&json!({"_id": "abc", "val": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    // Duplicate insert → 409
    let resp = client
        .post(format!("{base_url}/events/docs"))
        .json(&json!({"_id": "abc", "val": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["code"], "DOCUMENT_CONFLICT");
    assert!(body["error"]["message"].as_str().unwrap().contains("abc"));
}

#[tokio::test]
async fn test_custom_id_validation() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Empty string
    let resp = client
        .post(format!("{base_url}/events/docs"))
        .json(&json!({"_id": "", "val": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("non-empty string")
    );

    // Non-string (number)
    let resp = client
        .post(format!("{base_url}/events/docs"))
        .json(&json!({"_id": 42, "val": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("non-empty string")
    );

    // Starts with underscore
    let resp = client
        .post(format!("{base_url}/events/docs"))
        .json(&json!({"_id": "_reserved", "val": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("underscore")
    );

    // Exceeds 512 bytes
    let long_id = "x".repeat(513);
    let resp = client
        .post(format!("{base_url}/events/docs"))
        .json(&json!({"_id": long_id, "val": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("maximum length")
    );

    // Contains null byte
    let resp = client
        .post(format!("{base_url}/events/docs"))
        .json(&json!({"_id": "has\x00null", "val": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid characters")
    );
}

#[tokio::test]
async fn test_custom_id_auto_generate_when_absent() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Insert without _id
    let resp = client
        .post(format!("{base_url}/events/docs"))
        .json(&json!({"event_type": "login"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    let auto_id = body["data"]["_id"].as_str().unwrap();
    // UUIDv7 format: 8-4-4-4-12 hex chars
    assert_eq!(auto_id.len(), 36);
    assert!(auto_id.contains('-'));
}

#[tokio::test]
async fn test_custom_id_bulk_insert() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Bulk: one custom, one auto, one duplicate within batch
    let resp = client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({
            "documents": [
                {"_id": "custom-1", "val": 1},
                {"val": 2},
                {"_id": "custom-1", "val": 3}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["inserted"], 2);
    let errors = body["data"]["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].as_str().unwrap().contains("duplicate _id"));

    // Verify the custom-id doc was inserted
    let resp = client
        .get(format!("{base_url}/events/docs/custom-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["val"], 1);
}

#[tokio::test]
async fn test_custom_id_bulk_duplicate_existing() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Pre-insert a document
    client
        .post(format!("{base_url}/events/docs"))
        .json(&json!({"_id": "existing-1", "val": 1}))
        .send()
        .await
        .unwrap();

    // Bulk insert with conflicting _id
    let resp = client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({
            "documents": [
                {"_id": "existing-1", "val": 2},
                {"_id": "new-1", "val": 3}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["inserted"], 1);
    let errors = body["data"]["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].as_str().unwrap().contains("existing-1"));

    // Original doc unchanged
    let resp = client
        .get(format!("{base_url}/events/docs/existing-1"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["val"], 1);
}

#[tokio::test]
async fn test_custom_id_index_maintenance() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Create index on event_type
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type", "fields": ["event_type"]}))
        .send()
        .await
        .unwrap();

    // Insert with custom _id
    client
        .post(format!("{base_url}/events/docs"))
        .json(&json!({"_id": "evt-001", "event_type": "login", "user": "alice"}))
        .send()
        .await
        .unwrap();

    // Query using the indexed field
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"event_type": "login"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["_id"], "evt-001");
    // Verify index was used
    assert!(
        body["meta"]["index_used"].is_string(),
        "Expected index to be used"
    );
}

#[tokio::test]
async fn test_custom_id_update_and_delete() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Insert with custom _id
    client
        .post(format!("{base_url}/events/docs"))
        .json(&json!({"_id": "doc-1", "val": 1, "extra": "a"}))
        .send()
        .await
        .unwrap();

    // PUT replace
    let resp = client
        .put(format!("{base_url}/events/docs/doc-1"))
        .json(&json!({"val": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["_id"], "doc-1");
    assert_eq!(body["data"]["_rev"], 2);
    assert_eq!(body["data"]["val"], 2);
    // extra field should be gone (full replace)
    assert!(body["data"]["extra"].is_null());

    // PATCH partial update
    let resp = client
        .patch(format!("{base_url}/events/docs/doc-1"))
        .json(&json!({"val": 3}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["_rev"], 3);
    assert_eq!(body["data"]["val"], 3);

    // DELETE
    let resp = client
        .delete(format!("{base_url}/events/docs/doc-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Verify gone
    let resp = client
        .get(format!("{base_url}/events/docs/doc-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn test_custom_id_bitmap_maintenance() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // Insert docs with custom _id and bitmap-tracked field
    for i in 0..5 {
        let resp = client
            .post(format!("{base_url}/events/docs"))
            .json(&json!({
                "_id": format!("bm-{i}"),
                "category": "A",
                "val": i
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "Insert bm-{i} failed");
    }
    // Insert some with category B
    for i in 5..8 {
        client
            .post(format!("{base_url}/events/docs"))
            .json(&json!({
                "_id": format!("bm-{i}"),
                "category": "B",
                "val": i
            }))
            .send()
            .await
            .unwrap();
    }

    // Bitmap count query
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"category": "A"},
            "count_only": true
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["count"], 5);
    assert_eq!(body["meta"]["scan_strategy"], "bitmap");

    // Verify full query returns the correct custom-ID docs
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"category": "B"}}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 3);
    let ids: Vec<&str> = docs.iter().map(|d| d["_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"bm-5"));
    assert!(ids.contains(&"bm-6"));
    assert!(ids.contains(&"bm-7"));
}

#[tokio::test]
async fn test_engine_marker_file() {
    use wardsondb::engine::storage::MemoryConfig;

    let tmp = TempDir::new().unwrap();

    // First open with rocksdb: marker written.
    {
        let _storage =
            Storage::open_with_config(tmp.path(), "rocksdb", MemoryConfig::default()).unwrap();
    }
    let marker = tmp.path().join(".engine");
    assert!(marker.exists(), "marker file should exist after first open");
    let contents = std::fs::read_to_string(&marker).unwrap();
    assert_eq!(contents.trim(), "rocksdb");

    // Reopening with a different engine on existing data must fail.
    let result = Storage::open_with_config(tmp.path(), "fjall", MemoryConfig::default());
    assert!(
        result.is_err(),
        "opening with mismatched engine must fail, got Ok"
    );
    let err = format!("{:?}", result.err().unwrap());
    assert!(
        err.contains("rocksdb") && err.contains("fjall"),
        "error should name both engines: {err}"
    );
}

#[tokio::test]
async fn test_fjall_backend_basic() {
    use wardsondb::engine::storage::MemoryConfig;

    let tmp = TempDir::new().unwrap();
    let storage = Storage::open_with_config(tmp.path(), "fjall", MemoryConfig::default()).unwrap();
    assert_eq!(storage.engine_name, "fjall");

    // Round-trip a collection + document through the fjall backend.
    storage.create_collection("fj").unwrap();
    let doc = serde_json::json!({"hello": "fjall"});
    let inserted = storage.insert_document("fj", doc).unwrap();
    let id = inserted["_id"].as_str().unwrap().to_string();

    let fetched = storage.get_document("fj", &id).unwrap();
    assert_eq!(fetched["hello"], "fjall");

    let docs = storage.scan_all_documents("fj").unwrap();
    assert_eq!(docs.len(), 1);

    storage.delete_document("fj", &id).unwrap();
    assert_eq!(storage.scan_all_documents("fj").unwrap().len(), 0);
}

// ─── Sort-spec unification (shared parser for /query sort and $sort) ─────────

/// A-1: aggregate $sort array form must respect written field order, not
/// alphabetical key order (field names chosen anti-alphabetically on purpose).
#[tokio::test]
async fn test_aggregate_sort_array_form_respects_written_order() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "orders"}))
        .send()
        .await
        .unwrap();

    // zeta ties force alpha to decide; alphabetical priority (alpha first)
    // would produce a different order than written priority (zeta first).
    let docs = json!({
        "documents": [
            {"tag": "a", "zeta": 1, "alpha": 9},
            {"tag": "b", "zeta": 2, "alpha": 1},
            {"tag": "c", "zeta": 1, "alpha": 5},
            {"tag": "d", "zeta": 2, "alpha": 7},
        ]
    });
    client
        .post(format!("{base_url}/orders/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{base_url}/orders/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$sort": [{"zeta": "desc"}, {"alpha": "asc"}]}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let tags: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["tag"].as_str().unwrap())
        .collect();
    // zeta desc first (2s before 1s), then alpha asc within ties.
    assert_eq!(tags, vec!["b", "d", "c", "a"]);
}

/// DT-11: `$sort: []` (array form) is an accepted no-op stage — 200, and the
/// document order is identical to the same pipeline without the stage.
#[tokio::test]
async fn test_aggregate_sort_empty_array_noop() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "orders"}))
        .send()
        .await
        .unwrap();

    let docs = json!({
        "documents": [
            {"tag": "a", "n": 3},
            {"tag": "b", "n": 1},
            {"tag": "c", "n": 2},
        ]
    });
    client
        .post(format!("{base_url}/orders/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    let tags_for = |pipeline: Value| {
        let client = client.clone();
        let url = format!("{base_url}/orders/aggregate");
        async move {
            let resp = client
                .post(url)
                .json(&json!({"pipeline": pipeline}))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let body: Value = resp.json().await.unwrap();
            body["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["tag"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };

    // Empty pipelines are rejected outright, so anchor both runs on a
    // match-all stage; the only difference is the no-op $sort.
    let baseline = tags_for(json!([{"$match": {}}])).await;
    let with_noop_sort = tags_for(json!([{"$match": {}}, {"$sort": []}])).await;

    assert_eq!(baseline.len(), 3);
    assert_eq!(with_noop_sort, baseline);
}

/// A-2: multi-key flat $sort object is ambiguous (JSON key order lost) → 400.
#[tokio::test]
async fn test_aggregate_sort_multi_key_object_rejected() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "orders"}))
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{base_url}/orders/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$sort": {"name": 1, "count": -1}}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "INVALID_PIPELINE");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("array form"),
        "message should point at the array form: {msg}"
    );
}

/// A-3: numeric 1/-1 directions now work on /query (previously -1 silently
/// sorted ascending). Floats 1.0/-1.0 are accepted too.
#[tokio::test]
async fn test_query_sort_numeric_directions() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "products"}))
        .send()
        .await
        .unwrap();

    let docs = json!({
        "documents": [
            {"name": "cheap", "price": 1},
            {"name": "mid", "price": 5},
            {"name": "dear", "price": 9},
        ]
    });
    client
        .post(format!("{base_url}/products/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    let names_for = |sort: Value| {
        let client = client.clone();
        let base_url = base_url.clone();
        async move {
            let resp = client
                .post(format!("{base_url}/products/query"))
                .json(&json!({"sort": sort}))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let body: Value = resp.json().await.unwrap();
            body["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };

    assert_eq!(
        names_for(json!([{"price": -1}])).await,
        vec!["dear", "mid", "cheap"]
    );
    assert_eq!(
        names_for(json!([{"price": 1}])).await,
        vec!["cheap", "mid", "dear"]
    );
    assert_eq!(
        names_for(json!([{"price": -1.0}])).await,
        vec!["dear", "mid", "cheap"]
    );
}

/// A-4: invalid direction values are rejected with 400 on both endpoints,
/// naming the offending field (previously they silently sorted ascending).
#[tokio::test]
async fn test_sort_invalid_direction_rejected_both_endpoints() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "products"}))
        .send()
        .await
        .unwrap();

    // /query endpoint: typo'd string direction
    let resp = client
        .post(format!("{base_url}/products/query"))
        .json(&json!({"sort": [{"price": "descending"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "INVALID_QUERY");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("'price'"),
        "message should name the field: {msg}"
    );

    // aggregate $sort: direction 0 is not a valid direction
    let resp = client
        .post(format!("{base_url}/products/aggregate"))
        .json(&json!({"pipeline": [{"$sort": {"price": 0}}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "INVALID_PIPELINE");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("'price'"),
        "message should name the field: {msg}"
    );
}

/// A-8: a single-field flat object is accepted as the sort spec on /query
/// (symmetric with the aggregate $sort stage).
#[tokio::test]
async fn test_query_sort_flat_object_form() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "products"}))
        .send()
        .await
        .unwrap();

    let docs = json!({
        "documents": [
            {"name": "cheap", "price": 1},
            {"name": "dear", "price": 9},
        ]
    });
    client
        .post(format!("{base_url}/products/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{base_url}/products/query"))
        .json(&json!({"sort": {"price": "desc"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs[0]["name"], "dear");
    assert_eq!(docs[1]["name"], "cheap");
}

/// A-9: `{}` sort is rejected on both endpoints (an empty object cannot
/// express intent); `[]` remains a no-op on /query.
#[tokio::test]
async fn test_sort_empty_object_rejected() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "products"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/products/docs"))
        .json(&json!({"name": "one"}))
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{base_url}/products/query"))
        .json(&json!({"sort": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "INVALID_QUERY");

    let resp = client
        .post(format!("{base_url}/products/aggregate"))
        .json(&json!({"pipeline": [{"$sort": {}}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "INVALID_PIPELINE");

    // [] is still a no-op sort on /query
    let resp = client
        .post(format!("{base_url}/products/query"))
        .json(&json!({"sort": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
}

// ─── Multi-field sort: IndexSorted gating + _id tiebreak ─────────────────────

/// A-5: a compound index covering eq-prefix + ALL sort fields in order serves
/// a multi-field sort via index_sorted, with correct secondary-field ordering.
/// (Previously the planner matched on the first sort field only and the scan
/// never re-sorted, silently ignoring secondary fields.)
#[tokio::test]
async fn test_multi_field_sort_served_by_compound_index() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_sev_time", "fields": ["event_type", "severity", "received_at"]}))
        .send()
        .await
        .unwrap();

    // severity ties force received_at to decide within each severity group.
    let docs = json!({
        "documents": [
            {"tag": "a", "event_type": "fw", "severity": "high", "received_at": "2026-03-03"},
            {"tag": "b", "event_type": "fw", "severity": "low",  "received_at": "2026-03-01"},
            {"tag": "c", "event_type": "fw", "severity": "high", "received_at": "2026-03-01"},
            {"tag": "d", "event_type": "fw", "severity": "low",  "received_at": "2026-03-04"},
            {"tag": "e", "event_type": "dns", "severity": "high", "received_at": "2026-03-02"},
        ]
    });
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "fw"},
            "sort": [{"severity": "asc"}, {"received_at": "asc"}],
            "limit": 10
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["scan_strategy"], "index_sorted");
    assert_eq!(body["meta"]["index_used"], "idx_type_sev_time");

    let tags: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["tag"].as_str().unwrap())
        .collect();
    // severity asc ("high" < "low"), then received_at asc within severity.
    assert_eq!(tags, vec!["c", "a", "b", "d"]);
}

/// A-6: mixed sort directions can't be served by one index scan direction —
/// the planner must fall back, and the in-memory sort must produce the
/// correct order.
#[tokio::test]
async fn test_multi_field_sort_mixed_direction_falls_back() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_sev_time", "fields": ["event_type", "severity", "received_at"]}))
        .send()
        .await
        .unwrap();

    let docs = json!({
        "documents": [
            {"tag": "a", "event_type": "fw", "severity": "high", "received_at": "2026-03-03"},
            {"tag": "b", "event_type": "fw", "severity": "low",  "received_at": "2026-03-01"},
            {"tag": "c", "event_type": "fw", "severity": "high", "received_at": "2026-03-01"},
            {"tag": "d", "event_type": "fw", "severity": "low",  "received_at": "2026-03-04"},
        ]
    });
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "fw"},
            "sort": [{"severity": "asc"}, {"received_at": "desc"}],
            "limit": 10
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_ne!(
        body["meta"]["scan_strategy"], "index_sorted",
        "mixed directions must not use index_sorted"
    );

    let tags: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["tag"].as_str().unwrap())
        .collect();
    // severity asc, received_at desc within severity.
    assert_eq!(tags, vec!["a", "c", "d", "b"]);
}

/// A-7: equal sort values order deterministically by _id — ascending for an
/// asc sort, descending for a desc sort (the tiebreak follows the last sort
/// field's direction) — and repeated queries return identical order.
#[tokio::test]
async fn test_sort_tiebreak_deterministic() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..6).map(|i| json!({"n": i, "price": 5})).collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    let ids_for = |dir: &'static str| {
        let client = client.clone();
        let base_url = base_url.clone();
        async move {
            let resp = client
                .post(format!("{base_url}/items/query"))
                .json(&json!({"sort": [{"price": dir}]}))
                .send()
                .await
                .unwrap();
            let body: Value = resp.json().await.unwrap();
            body["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["_id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };

    let asc_ids = ids_for("asc").await;
    let mut expected = asc_ids.clone();
    expected.sort();
    assert_eq!(
        asc_ids, expected,
        "asc-sort ties must order by _id ascending"
    );

    let desc_ids = ids_for("desc").await;
    let mut expected_desc = asc_ids.clone();
    expected_desc.reverse();
    assert_eq!(
        desc_ids, expected_desc,
        "desc-sort ties must order by _id descending"
    );

    // Repeatability
    assert_eq!(asc_ids, ids_for("asc").await);
}

/// A-10: an index with extra fields AFTER the sort fields still serves the
/// sort via index_sorted (extras only affect within-tie order).
#[tokio::test]
async fn test_index_sorted_extra_trailing_fields_still_used() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_time_sev", "fields": ["event_type", "received_at", "severity"]}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..10)
        .map(|i| {
            json!({
                "event_type": "fw",
                "received_at": format!("2026-03-{:02}", i + 1),
                "severity": if i % 2 == 0 { "high" } else { "low" }
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "fw"},
            "sort": [{"received_at": "asc"}],
            "limit": 5
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["scan_strategy"], "index_sorted");
    assert_eq!(body["meta"]["index_used"], "idx_type_time_sev");

    let times: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["received_at"].as_str().unwrap())
        .collect();
    assert_eq!(
        times,
        vec![
            "2026-03-01",
            "2026-03-02",
            "2026-03-03",
            "2026-03-04",
            "2026-03-05"
        ]
    );
}

// ─── Cursor pagination ────────────────────────────────────────────────────────

/// Walk a cursor-paginated query to exhaustion, returning all docs in page
/// order. `body` is the request WITHOUT a cursor; subsequent pages echo back
/// `meta.next_cursor`. Panics if the walk doesn't terminate.
async fn cursor_walk(client: &Client, base_url: &str, collection: &str, body: Value) -> Vec<Value> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..100 {
        let mut req = body.clone();
        if let Some(c) = &cursor {
            req["cursor"] = json!(c);
        }
        let resp = client
            .post(format!("{base_url}/{collection}/query"))
            .json(&req)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let resp_body: Value = resp.json().await.unwrap();
        assert_eq!(resp_body["ok"], true);
        all.extend(resp_body["data"].as_array().unwrap().iter().cloned());
        match resp_body["meta"]["next_cursor"].as_str() {
            Some(c) => {
                assert_eq!(
                    resp_body["meta"]["has_more"], true,
                    "next_cursor implies has_more"
                );
                cursor = Some(c.to_string());
            }
            None => return all,
        }
    }
    panic!("cursor walk did not terminate within 100 pages");
}

fn ids_of(docs: &[Value]) -> Vec<String> {
    docs.iter()
        .map(|d| d["_id"].as_str().unwrap().to_string())
        .collect()
}

/// Reference result: same query in one shot with a large limit.
async fn reference_ids(
    client: &Client,
    base_url: &str,
    collection: &str,
    mut body: Value,
) -> Vec<String> {
    body["limit"] = json!(10_000);
    let resp = client
        .post(format!("{base_url}/{collection}/query"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let resp_body: Value = resp.json().await.unwrap();
    ids_of(resp_body["data"].as_array().unwrap())
}

/// B-1: cursor walk over an unindexed sorted query (full scan + in-memory
/// sort). Includes duplicate sort values and docs missing the sort field —
/// pages must concatenate to exactly the one-shot result: no dups, no gaps.
#[tokio::test]
async fn test_cursor_walk_full_scan() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..25)
        .map(|i| {
            if i % 5 == 0 {
                json!({"n": i}) // missing sort field
            } else {
                json!({"n": i, "score": i % 4}) // lots of duplicate scores
            }
        })
        .collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    let body = json!({"sort": [{"score": "asc"}], "limit": 4});
    let walked = cursor_walk(&client, &base_url, "items", body.clone()).await;
    assert_eq!(walked.len(), 25);
    assert_eq!(
        ids_of(&walked),
        reference_ids(&client, &base_url, "items", body).await
    );
}

/// B-2: cursor walk where the filter uses a single-field index but the sort
/// runs in memory (IndexEq strategy).
#[tokio::test]
async fn test_cursor_walk_index_eq_in_memory_sort() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "products"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/products/indexes"))
        .json(&json!({"name": "idx_cat", "fields": ["category"]}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..20)
        .map(|i| {
            json!({
                "category": if i % 2 == 0 { "tools" } else { "toys" },
                "price": i % 3,
                "n": i
            })
        })
        .collect();
    client
        .post(format!("{base_url}/products/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    let body = json!({
        "filter": {"category": "tools"},
        "sort": [{"price": "desc"}],
        "limit": 3
    });
    let walked = cursor_walk(&client, &base_url, "products", body.clone()).await;
    assert_eq!(walked.len(), 10);
    for d in &walked {
        assert_eq!(d["category"], "tools");
    }
    assert_eq!(
        ids_of(&walked),
        reference_ids(&client, &base_url, "products", body).await
    );
}

/// B-4: cursor walk over a bitmap-accelerated filter. Bitmap scans surface
/// docs in insertion-position order, so the cursor layer must re-sort them
/// deterministically.
#[tokio::test]
async fn test_cursor_walk_bitmap() {
    let (base_url, _tmp) = start_test_server_with_bitmap("event_type").await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..30)
        .map(|i| {
            json!({
                "event_type": if i % 3 == 0 { "dns" } else { "firewall" },
                "value": (i * 7) % 10,
                "n": i
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Confirm the filter actually takes the bitmap path.
    let probe = client
        .post(format!("{base_url}/events/query"))
        .json(
            &json!({"filter": {"event_type": "firewall"}, "sort": [{"value": "asc"}], "limit": 4}),
        )
        .send()
        .await
        .unwrap();
    let probe_body: Value = probe.json().await.unwrap();
    assert_eq!(probe_body["meta"]["scan_strategy"], "bitmap");

    let body = json!({
        "filter": {"event_type": "firewall"},
        "sort": [{"value": "asc"}],
        "limit": 4
    });
    let walked = cursor_walk(&client, &base_url, "events", body.clone()).await;
    assert_eq!(walked.len(), 20);
    assert_eq!(
        ids_of(&walked),
        reference_ids(&client, &base_url, "events", body).await
    );
}

/// B-5: cursor walk over a compound-range scan (eq prefix + range suffix,
/// sorted in memory by an uncovered field).
#[tokio::test]
async fn test_cursor_walk_compound_range() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_ts", "fields": ["event_type", "ts"]}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..24)
        .map(|i| {
            json!({
                "event_type": if i % 2 == 0 { "fw" } else { "dns" },
                "ts": i,
                "other": (i * 5) % 7,
                "n": i
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Sort by a field the index tail doesn't cover → IndexSorted can't serve
    // it; the (eq + range) shape triggers CompoundRange with in-memory sort.
    let body = json!({
        "filter": {"event_type": "fw", "ts": {"$gte": 4, "$lte": 18}},
        "sort": [{"other": "asc"}],
        "limit": 3
    });

    let probe = client
        .post(format!("{base_url}/events/query"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let probe_body: Value = probe.json().await.unwrap();
    assert_eq!(probe_body["meta"]["scan_strategy"], "compound_range");

    let walked = cursor_walk(&client, &base_url, "events", body.clone()).await;
    assert_eq!(walked.len(), 8); // ts in {4,6,8,10,12,14,16,18}
    assert_eq!(
        ids_of(&walked),
        reference_ids(&client, &base_url, "events", body).await
    );
}

/// B-6: a tie run larger than the page size must split across pages without
/// duplication or loss, ascending and descending.
#[tokio::test]
async fn test_cursor_ties_span_page_boundary() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();

    // All docs share one sort value — the entire result is one tie run.
    let docs: Vec<Value> = (0..12).map(|i| json!({"grp": "same", "n": i})).collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    for dir in ["asc", "desc"] {
        let body = json!({"sort": [{"grp": dir}], "limit": 5});
        let walked = cursor_walk(&client, &base_url, "items", body.clone()).await;
        assert_eq!(walked.len(), 12, "direction {dir}");
        assert_eq!(
            ids_of(&walked),
            reference_ids(&client, &base_url, "items", body).await,
            "direction {dir}"
        );
    }
}

/// Cursor walk with an explicit _id sort — the deterministic way to walk a
/// whole collection.
#[tokio::test]
async fn test_cursor_walk_sort_by_id() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..17).map(|i| json!({"n": i})).collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    let body = json!({"sort": [{"_id": "asc"}], "limit": 4});
    let walked = cursor_walk(&client, &base_url, "items", body).await;
    assert_eq!(walked.len(), 17);
    let ids = ids_of(&walked);
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "ids must come back in ascending order");
}

/// B-8/B-9: cursor is mutually exclusive with offset and count_only.
#[tokio::test]
async fn test_cursor_mutual_exclusions_rejected() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();

    for bad in [
        json!({"cursor": "abc", "offset": 5}),
        json!({"cursor": "abc", "count_only": true}),
        json!({"cursor": "abc", "limit": 0}),
    ] {
        let resp = client
            .post(format!("{base_url}/items/query"))
            .json(&bad)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "request {bad} should be rejected");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "INVALID_QUERY");
    }
}

/// B-10: garbage cursor tokens are rejected with a clean 400.
#[tokio::test]
async fn test_cursor_garbage_rejected() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();

    // Not base64; valid base64 of the wrong shape; oversize token.
    let huge = "A".repeat(5000);
    for bad_token in ["!!!", "eyJoZWxsbyI6IndvcmxkIn0", huge.as_str()] {
        let resp = client
            .post(format!("{base_url}/items/query"))
            .json(&json!({"cursor": bad_token}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "INVALID_QUERY");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("cursor"),
            "message should mention the cursor"
        );
    }
}

/// B-11: a cursor is bound to (collection, sort spec) — replaying it against
/// a different sort or collection is a clean 400, not garbage results.
#[tokio::test]
async fn test_cursor_spec_mismatch_rejected() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    for coll in ["alpha", "beta"] {
        client
            .post(format!("{base_url}/_collections"))
            .json(&json!({"name": coll}))
            .send()
            .await
            .unwrap();
        let docs: Vec<Value> = (0..6).map(|i| json!({"score": i})).collect();
        client
            .post(format!("{base_url}/{coll}/docs/_bulk"))
            .json(&json!({"documents": docs}))
            .send()
            .await
            .unwrap();
    }

    // Obtain a genuine cursor from alpha, sorted by score asc.
    let resp = client
        .post(format!("{base_url}/alpha/query"))
        .json(&json!({"sort": [{"score": "asc"}], "limit": 2}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let token = body["meta"]["next_cursor"].as_str().unwrap().to_string();

    // Same collection, different sort spec → 400.
    let resp = client
        .post(format!("{base_url}/alpha/query"))
        .json(&json!({"sort": [{"score": "desc"}], "limit": 2, "cursor": token}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["code"], "INVALID_QUERY");

    // Same sort, different collection → 400.
    let resp = client
        .post(format!("{base_url}/beta/query"))
        .json(&json!({"sort": [{"score": "asc"}], "limit": 2, "cursor": token}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Sanity: the token still works where it came from.
    let resp = client
        .post(format!("{base_url}/alpha/query"))
        .json(&json!({"sort": [{"score": "asc"}], "limit": 2, "cursor": token}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// B-12: deleting documents between pages — both already-returned and
/// not-yet-returned — must not repeat or skip any surviving document.
#[tokio::test]
async fn test_cursor_delete_mid_pagination() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..10).map(|i| json!({"score": i})).collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Page 1 (scores 0,1,2).
    let resp = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"sort": [{"score": "asc"}], "limit": 3}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let page1 = body["data"].as_array().unwrap().clone();
    let token = body["meta"]["next_cursor"].as_str().unwrap().to_string();
    assert_eq!(page1.len(), 3);

    // Delete one already-returned doc (score 1) and one future doc (score 5).
    let full = reference_ids(
        &client,
        &base_url,
        "items",
        json!({"sort": [{"score": "asc"}]}),
    )
    .await;
    for idx in [1usize, 5usize] {
        let id = &full[idx];
        let resp = client
            .delete(format!("{base_url}/items/docs/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // Resume the walk from the page-1 cursor.
    let mut rest = Vec::new();
    let mut cursor = Some(token);
    while let Some(c) = cursor {
        let resp = client
            .post(format!("{base_url}/items/query"))
            .json(&json!({"sort": [{"score": "asc"}], "limit": 3, "cursor": c}))
            .send()
            .await
            .unwrap();
        let body: Value = resp.json().await.unwrap();
        rest.extend(body["data"].as_array().unwrap().iter().cloned());
        cursor = body["meta"]["next_cursor"].as_str().map(String::from);
    }

    // Scores 3,4,6,7,8,9 — deleted 5 gone, deleted 1 not repeated.
    let scores: Vec<i64> = rest.iter().map(|d| d["score"].as_i64().unwrap()).collect();
    assert_eq!(scores, vec![3, 4, 6, 7, 8, 9]);

    // No overlap with page 1.
    let page1_ids = ids_of(&page1);
    for d in &rest {
        assert!(!page1_ids.contains(&d["_id"].as_str().unwrap().to_string()));
    }
}

/// B-13: exact boundary — when matches == limit, has_more and next_cursor
/// must be absent (materializing paths compute this exactly).
#[tokio::test]
async fn test_has_more_exact_boundary_materialized() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..6).map(|i| json!({"score": i})).collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Exactly limit docs → no has_more, no next_cursor.
    let resp = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"sort": [{"score": "asc"}], "limit": 6}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 6);
    assert!(body["meta"]["has_more"].is_null());
    assert!(body["meta"]["next_cursor"].is_null());

    // One fewer → both present; the final page then ends the walk.
    let resp = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"sort": [{"score": "asc"}], "limit": 5}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["has_more"], true);
    let token = body["meta"]["next_cursor"].as_str().unwrap().to_string();

    let resp = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"sort": [{"score": "asc"}], "limit": 5, "cursor": token}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert!(body["meta"]["has_more"].is_null());
    assert!(body["meta"]["next_cursor"].is_null());
}

/// B-17: projection that drops the sort field must not break the walk — the
/// cursor is built from the pre-projection document.
#[tokio::test]
async fn test_cursor_with_projection_excluding_sort_field() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..14)
        .map(|i| json!({"score": i % 4, "name": format!("item-{i}")}))
        .collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    let body = json!({
        "sort": [{"score": "desc"}],
        "fields": ["name"],
        "limit": 4
    });
    let walked = cursor_walk(&client, &base_url, "items", body).await;
    assert_eq!(walked.len(), 14);
    // Projected docs must not contain the sort field, but the order must
    // match the unprojected reference.
    for d in &walked {
        assert!(d.get("score").is_none());
        assert!(d.get("name").is_some());
    }
    let reference = reference_ids(
        &client,
        &base_url,
        "items",
        json!({"sort": [{"score": "desc"}]}),
    )
    .await;
    assert_eq!(ids_of(&walked), reference);
}

// ─── Cursor fast paths (index seek, _id seek, exact probes) ──────────────────

async fn start_test_server_with_engine(engine: &str) -> (String, TempDir) {
    use wardsondb::engine::storage::MemoryConfig;

    let tmp = TempDir::new().unwrap();
    let storage = Storage::open_with_config(tmp.path(), engine, MemoryConfig::default()).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut config = test_config(&tmp, port);
    config.storage_engine = engine.to_string();

    let state = Arc::new(AppState {
        storage,
        config,
        started_at: Instant::now(),
        metrics: Arc::new(Metrics::new()),
        api_keys: vec![],
    });

    let app = build_router(state);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    let base_url = format!("http://{addr}");

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (base_url, tmp)
}

/// Seed a collection with a compound index [event_type, received_at] and 15
/// "fw" docs (with duplicated timestamps so ties span pages) + 5 "dns" docs.
async fn seed_index_sorted_collection(client: &Client, base_url: &str) {
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_time", "fields": ["event_type", "received_at"]}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..20)
        .map(|i| {
            json!({
                "event_type": if i < 15 { "fw" } else { "dns" },
                // i/3 → duplicate timestamps: tie runs of 3 cross page bounds
                "received_at": format!("2026-04-{:02}", (i / 3) + 1),
                "n": i
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();
}

/// B-3: cursor walks stay on the index_sorted fast path page after page, in
/// both directions, with tie runs crossing page boundaries.
#[tokio::test]
async fn test_cursor_walk_index_sorted_both_directions() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_index_sorted_collection(&client, &base_url).await;

    for dir in ["asc", "desc"] {
        let body = json!({
            "filter": {"event_type": "fw"},
            "sort": [{"received_at": dir}],
            "limit": 4
        });

        // Every page must be served by index_sorted, including cursor resumes.
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut req = body.clone();
            if let Some(c) = &cursor {
                req["cursor"] = json!(c);
            }
            let resp = client
                .post(format!("{base_url}/events/query"))
                .json(&req)
                .send()
                .await
                .unwrap();
            let resp_body: Value = resp.json().await.unwrap();
            assert_eq!(
                resp_body["meta"]["scan_strategy"], "index_sorted",
                "direction {dir}: cursor pages must stay on the seek path"
            );
            all.extend(resp_body["data"].as_array().unwrap().iter().cloned());
            match resp_body["meta"]["next_cursor"].as_str() {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }

        assert_eq!(all.len(), 15, "direction {dir}");
        assert_eq!(
            ids_of(&all),
            reference_ids(&client, &base_url, "events", body).await,
            "direction {dir}: pages must equal the one-shot result"
        );
    }
}

/// B-14: index_sorted has_more is exact — when matches == offset + limit the
/// probe finds nothing and has_more/next_cursor are absent. (Previously it
/// reported has_more: true the moment the limit filled.)
#[tokio::test]
async fn test_index_sorted_has_more_exact_boundary() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_index_sorted_collection(&client, &base_url).await;

    // 15 fw docs exactly.
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "fw"},
            "sort": [{"received_at": "asc"}],
            "limit": 15
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["scan_strategy"], "index_sorted");
    assert_eq!(body["data"].as_array().unwrap().len(), 15);
    assert!(
        body["meta"]["has_more"].is_null(),
        "matches == limit must not report has_more"
    );
    assert!(body["meta"]["next_cursor"].is_null());

    // offset + limit == matches → same.
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "fw"},
            "sort": [{"received_at": "asc"}],
            "offset": 10,
            "limit": 5
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["scan_strategy"], "index_sorted");
    assert!(body["meta"]["has_more"].is_null());

    // One doc really is left → has_more present.
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "fw"},
            "sort": [{"received_at": "asc"}],
            "limit": 14
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["has_more"], true);
    assert!(body["meta"]["next_cursor"].is_string());
}

/// An index with extra trailing fields can't resume safely: exact has_more
/// but NO next_cursor; covering the tail with the sort re-enables cursors.
#[tokio::test]
async fn test_index_sorted_extras_no_cursor() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_time_sev", "fields": ["event_type", "received_at", "severity"]}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..10)
        .map(|i| {
            json!({
                "event_type": "fw",
                "received_at": format!("2026-04-{:02}", i + 1),
                "severity": if i % 2 == 0 { "high" } else { "low" }
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Sort covers only [received_at]: index tail has extras → no cursor.
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "fw"},
            "sort": [{"received_at": "asc"}],
            "limit": 4
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["scan_strategy"], "index_sorted");
    assert_eq!(body["meta"]["has_more"], true);
    assert!(
        body["meta"]["next_cursor"].is_null(),
        "extras-tail plans must not emit cursors"
    );

    // Extending the sort to cover the tail re-enables cursor emission.
    let body_full = json!({
        "filter": {"event_type": "fw"},
        "sort": [{"received_at": "asc"}, {"severity": "asc"}],
        "limit": 4
    });
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&body_full)
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["scan_strategy"], "index_sorted");
    assert!(body["meta"]["next_cursor"].is_string());

    let walked = cursor_walk(&client, &base_url, "events", body_full.clone()).await;
    assert_eq!(walked.len(), 10);
    assert_eq!(
        ids_of(&walked),
        reference_ids(&client, &base_url, "events", body_full).await
    );
}

/// B-7: a plain no-sort query bootstraps a cursor from the full scan's _id
/// order and the seek path finishes the walk without re-materializing.
#[tokio::test]
async fn test_cursor_no_sort_id_walk() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..23)
        .map(|i| json!({"n": i, "keep": i % 2 == 0}))
        .collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Unfiltered whole-collection walk, no sort at all.
    let walked = cursor_walk(&client, &base_url, "items", json!({"limit": 5})).await;
    assert_eq!(walked.len(), 23);
    let ids = ids_of(&walked);
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "no-sort walk must stream in _id order");

    // Filtered no-sort walk exercises the seek path's residual filtering.
    let walked = cursor_walk(
        &client,
        &base_url,
        "items",
        json!({"filter": {"keep": true}, "limit": 4}),
    )
    .await;
    assert_eq!(walked.len(), 12);
    for d in &walked {
        assert_eq!(d["keep"], true);
    }
}

/// B-15: the descending index seek on the fjall backend (range_iterator_rev)
/// — full walk equivalence end-to-end over HTTP.
#[tokio::test]
async fn test_fjall_cursor_walk_index_sorted_desc() {
    let (base_url, _tmp) = start_test_server_with_engine("fjall").await;
    let client = Client::new();
    seed_index_sorted_collection(&client, &base_url).await;

    let body = json!({
        "filter": {"event_type": "fw"},
        "sort": [{"received_at": "desc"}],
        "limit": 4
    });

    let probe = client
        .post(format!("{base_url}/events/query"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let probe_body: Value = probe.json().await.unwrap();
    assert_eq!(probe_body["meta"]["scan_strategy"], "index_sorted");

    let walked = cursor_walk(&client, &base_url, "events", body.clone()).await;
    assert_eq!(walked.len(), 15);
    assert_eq!(
        ids_of(&walked),
        reference_ids(&client, &base_url, "events", body).await
    );

    // And the no-sort seek path on fjall too.
    let walked = cursor_walk(&client, &base_url, "events", json!({"limit": 7})).await;
    assert_eq!(walked.len(), 20);
}

/// B-16: --max-query-limit clamps each cursor page; the walk still completes.
#[tokio::test]
async fn test_cursor_limit_clamp_per_page() {
    let (base_url, _tmp) = start_test_server_with_max_query_limit(3).await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..10).map(|i| json!({"score": i})).collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Ask for 50 per page; the cap forces pages of 3.
    let mut pages = 0;
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut req = json!({"sort": [{"score": "asc"}], "limit": 50});
        if let Some(c) = &cursor {
            req["cursor"] = json!(c);
        }
        let resp = client
            .post(format!("{base_url}/items/query"))
            .json(&req)
            .send()
            .await
            .unwrap();
        let body: Value = resp.json().await.unwrap();
        let docs = body["data"].as_array().unwrap();
        assert!(docs.len() <= 3, "page must respect the configured cap");
        all.extend(docs.iter().cloned());
        pages += 1;
        assert!(pages < 20, "walk must terminate");
        match body["meta"]["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(all.len(), 10);
    assert_eq!(pages, 4); // 3+3+3+1
    let scores: Vec<i64> = all.iter().map(|d| d["score"].as_i64().unwrap()).collect();
    assert_eq!(scores, (0..10).collect::<Vec<i64>>());
}

/// Regression: index-only aggregation assumed 36-byte (UUID) doc ids and
/// silently skipped index entries for custom _ids of any other length,
/// undercounting groups. The separator is the LAST 0x00 in the key — value
/// encodings may themselves contain 0x00 bytes (e.g. embedded NULs).
#[tokio::test]
async fn test_index_only_aggregate_custom_ids() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_kind", "field": "kind"}))
        .send()
        .await
        .unwrap();

    // Mix of id lengths: 1 byte, 13 bytes, exactly 36 (UUID-length custom),
    // 100 bytes, and auto-generated UUIDv7s. Plus one value containing an
    // embedded NUL to pin the last-0x00 separator handling.
    let long_id = "L".repeat(100);
    let uuid_len_id = "x".repeat(36);
    let docs = json!({
        "documents": [
            {"_id": "a", "kind": "alpha"},
            {"_id": "medium-id-123", "kind": "alpha"},
            {"kind": "alpha"},                       // auto UUID
            {"_id": uuid_len_id, "kind": "beta"},
            {"_id": long_id, "kind": "beta"},
            {"kind": "beta"},                        // auto UUID
            {"kind": "beta"},                        // auto UUID
            {"_id": "z", "kind": "nul\u{0000}led"},
        ]
    });
    let resp = client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "bulk insert failed: {}",
        resp.text().await.unwrap_or_default()
    );

    let resp = client
        .post(format!("{base_url}/events/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$group": {"_id": "kind", "count": {"$count": {}}}},
                {"$sort": {"count": "desc"}}
            ]
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["meta"]["scan_strategy"], "index_only_aggregate");
    assert_eq!(body["meta"]["docs_scanned"], 0);

    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 3, "three distinct kinds: {data:?}");
    assert_eq!(data[0]["_id"], "beta");
    assert_eq!(data[0]["count"], 4);
    assert_eq!(data[1]["_id"], "alpha");
    assert_eq!(data[1]["count"], 3);
    assert_eq!(data[2]["_id"], "nul\u{0000}led");
    assert_eq!(data[2]["count"], 1);
}

/// Unfiltered count_only must come from DocCounters (O(1)), not a full
/// collection scan-and-parse, and must stay exact across every mutation path
/// that changes doc counts.
#[tokio::test]
async fn test_count_only_unfiltered_uses_doc_counter() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();

    let count_meta = |filter: Option<Value>| {
        let client = client.clone();
        let base_url = base_url.clone();
        async move {
            let mut req = json!({"count_only": true});
            if let Some(f) = filter {
                req["filter"] = f;
            }
            let resp = client
                .post(format!("{base_url}/items/query"))
                .json(&req)
                .send()
                .await
                .unwrap();
            resp.json::<Value>().await.unwrap()
        }
    };

    // Empty collection.
    let body = count_meta(None).await;
    assert_eq!(body["data"]["count"], 0);
    assert_eq!(body["meta"]["scan_strategy"], "doc_counter");
    assert_eq!(body["meta"]["docs_scanned"], 0);

    // Bulk insert 7 + single insert 1.
    let docs: Vec<Value> = (0..7).map(|i| json!({"n": i, "kind": "bulk"})).collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();
    let resp = client
        .post(format!("{base_url}/items/docs"))
        .json(&json!({"n": 100, "kind": "single"}))
        .send()
        .await
        .unwrap();
    let created: Value = resp.json().await.unwrap();
    let single_id = created["data"]["_id"].as_str().unwrap().to_string();

    let body = count_meta(None).await;
    assert_eq!(body["data"]["count"], 8);
    assert_eq!(body["meta"]["scan_strategy"], "doc_counter");

    // Delete one by id.
    client
        .delete(format!("{base_url}/items/docs/{single_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(count_meta(None).await["data"]["count"], 7);

    // Delete two by query.
    client
        .post(format!("{base_url}/items/docs/_delete_by_query"))
        .json(&json!({"filter": {"n": {"$gte": 5}}}))
        .send()
        .await
        .unwrap();
    let body = count_meta(None).await;
    assert_eq!(body["data"]["count"], 5);
    assert_eq!(body["meta"]["docs_scanned"], 0);

    // Filtered counts still take the scan/index paths and stay exact.
    let body = count_meta(Some(json!({"kind": "bulk"}))).await;
    assert_eq!(body["data"]["count"], 5);
    assert_ne!(body["meta"]["scan_strategy"], "doc_counter");

    // Missing collection still 404s.
    let resp = client
        .post(format!("{base_url}/nope/query"))
        .json(&json!({"count_only": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// The request body limit is configurable (--max-body-mb, default 64 MiB) —
/// axum's 2 MB default silently capped bulk inserts below the 16 MB
/// single-document limit. Oversized bodies get 413 DOCUMENT_TOO_LARGE.
#[tokio::test]
async fn test_request_body_limit() {
    // Default server: a ~3 MB bulk insert (over axum's old 2 MB default)
    // must succeed.
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "big"}))
        .send()
        .await
        .unwrap();

    let filler = "x".repeat(30_000);
    let docs: Vec<Value> = (0..100).map(|i| json!({"n": i, "pad": filler})).collect();
    let resp = client
        .post(format!("{base_url}/big/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "3MB bulk must pass the default limit");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"]["inserted"], 100);

    // Low-limit server: a 2 MB body against a 1 MiB cap → 413.
    let tmp = TempDir::new().unwrap();
    let storage = Storage::open(tmp.path()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut config = test_config(&tmp, port);
    config.max_body_mb = 1;
    let state = Arc::new(AppState {
        storage,
        config,
        started_at: Instant::now(),
        metrics: Arc::new(Metrics::new()),
        api_keys: vec![],
    });
    let app = build_router(state);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let small_base = format!("http://{addr}");

    client
        .post(format!("{small_base}/_collections"))
        .json(&json!({"name": "big"}))
        .send()
        .await
        .unwrap();
    let filler = "y".repeat(2_000_000);
    let resp = client
        .post(format!("{small_base}/big/docs"))
        .json(&json!({"pad": filler}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "DOCUMENT_TOO_LARGE");
}

/// A doc inserted immediately after collection creation must be counted:
/// the counter is seeded before the create commits (and increments upsert
/// on miss), so no write can land in an unseeded window and vanish from
/// the authoritative count_only path.
#[tokio::test]
async fn test_count_after_create_insert_immediately() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "fresh"}))
        .send()
        .await
        .unwrap();

    client
        .post(format!("{base_url}/fresh/docs"))
        .json(&json!({"kind": "first"}))
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{base_url}/fresh/query"))
        .json(&json!({"count_only": true}))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());
    assert_eq!(body["data"]["count"], 1);
    assert_eq!(body["meta"]["scan_strategy"], "doc_counter");
}

/// An offset near u64::MAX must not overflow the IndexSorted page probe
/// (offset + limit + 1 previously wrapped in release / panicked in debug,
/// silently truncating the backend read) — it must behave like any offset
/// past the end of the match set: 200, empty page, no more results.
#[tokio::test]
async fn test_giant_offset_no_overflow() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..10)
        .map(|i| {
            json!({
                "event_type": "firewall",
                "received_at": format!("2026-07-09T00:00:{i:02}Z")
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_time", "fields": ["event_type", "received_at"]}))
        .send()
        .await
        .unwrap();

    // Sanity: this query shape plans index_sorted — the site that overflowed.
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "firewall"},
            "sort": [{"received_at": "desc"}],
            "limit": 5
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["meta"]["scan_strategy"], "index_sorted");

    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": "firewall"},
            "sort": [{"received_at": "desc"}],
            "limit": 5,
            "offset": 18446744073709551615u64
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap());
    assert_eq!(body["data"].as_array().unwrap().len(), 0);
    assert_ne!(body["meta"]["has_more"], json!(true));
}

/// Every count_only fast path must self-report its scan strategy (T8), and
/// the $in count path must not double-count duplicate values — it sums
/// per-value index counts, unlike the doc-returning path which dedups ids.
#[tokio::test]
async fn test_count_only_scan_strategy_labels() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();

    // 100 docs: event_type cycles 4 values (indexed), seq 0..99 (indexed),
    // shard cycles 3 values (never indexed — exercises the full-scan count).
    let types = ["firewall", "dns", "dhcp", "ids"];
    let docs: Vec<Value> = (0..100)
        .map(|i| json!({"event_type": types[i % 4], "seq": i, "shard": i % 3}))
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();
    for (name, field) in [("idx_event_type", "event_type"), ("idx_seq", "seq")] {
        client
            .post(format!("{base_url}/events/indexes"))
            .json(&json!({"name": name, "field": field}))
            .send()
            .await
            .unwrap();
    }

    let count = |filter: Value| {
        let client = client.clone();
        let url = format!("{base_url}/events/query");
        async move {
            let resp = client
                .post(url)
                .json(&json!({"filter": filter, "count_only": true}))
                .send()
                .await
                .unwrap();
            let body: Value = resp.json().await.unwrap();
            assert!(body["ok"].as_bool().unwrap());
            (
                body["data"]["count"].as_u64().unwrap(),
                body["meta"]["scan_strategy"].as_str().unwrap().to_string(),
            )
        }
    };

    // $in with a duplicate value: 25 firewall + 25 dns, NOT 50 + 25.
    let (n, strategy) =
        count(json!({"event_type": {"$in": ["firewall", "firewall", "dns"]}})).await;
    assert_eq!(n, 50);
    assert_eq!(strategy, "index_in");

    // The doc-returning $in path must also yield no duplicates for
    // duplicate values (ids dedup at the value level).
    let resp = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"event_type": {"$in": ["firewall", "firewall", "dns"]}},
            "limit": 100
        }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 50);
    let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(unique.len(), 50, "duplicate docs in $in page");

    let (n, strategy) = count(json!({"seq": {"$gte": 50, "$lt": 75}})).await;
    assert_eq!(n, 25);
    assert_eq!(strategy, "index_range");

    // Unindexed field → full scan count.
    let expected_shard1 = (0..100).filter(|i| i % 3 == 1).count() as u64;
    let (n, strategy) = count(json!({"shard": 1})).await;
    assert_eq!(n, expected_shard1);
    assert_eq!(strategy, "full_scan");

    // Indexed eq + unindexed residual → materialized count, still labeled.
    let expected_fw_shard0 = (0..100).filter(|i| i % 4 == 0 && i % 3 == 0).count() as u64;
    let (n, strategy) = count(json!({"event_type": "firewall", "shard": 0})).await;
    assert_eq!(n, expected_fw_shard0);
    assert_eq!(strategy, "index_eq");
}

/// $regex behavior must be identical with parse-time compilation: match set,
/// $not composition, non-string fields never match, non-string pattern
/// operands are accepted-but-never-match, invalid/oversize patterns 400.
#[tokio::test]
async fn test_regex_semantics_preserved() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "notes"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/notes/docs/_bulk"))
        .json(&json!({"documents": [
            {"tag": "alpha-1", "n": 1},
            {"tag": "alpha-2", "n": 2},
            {"tag": "beta-1", "n": 3},
            {"tag": 42, "n": 4},          // non-string field value
            {"n": 5},                      // field missing
        ]}))
        .send()
        .await
        .unwrap();

    let query = |filter: Value| {
        let client = client.clone();
        let url = format!("{base_url}/notes/query");
        async move {
            let resp = client
                .post(url)
                .json(&json!({"filter": filter, "sort": [{"n": "asc"}]}))
                .send()
                .await
                .unwrap();
            let status = resp.status();
            let body: Value = resp.json().await.unwrap();
            (status, body)
        }
    };

    // Plain match set.
    let (status, body) = query(json!({"tag": {"$regex": "^alpha-[0-9]$"}})).await;
    assert_eq!(status, 200);
    let ns: Vec<i64> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["n"].as_i64().unwrap())
        .collect();
    assert_eq!(ns, [1, 2]);

    // $not composition inverts, including non-string and missing fields.
    let (status, body) = query(json!({"$not": {"tag": {"$regex": "^alpha-"}}})).await;
    assert_eq!(status, 200);
    let ns: Vec<i64> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["n"].as_i64().unwrap())
        .collect();
    assert_eq!(ns, [3, 4, 5]);

    // Non-string pattern operand: accepted, matches nothing.
    let (status, body) = query(json!({"tag": {"$regex": 123}})).await;
    assert_eq!(status, 200);
    assert_eq!(body["data"].as_array().unwrap().len(), 0);

    // Invalid pattern still rejected at parse time.
    let (status, body) = query(json!({"tag": {"$regex": "[unclosed"}})).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "INVALID_QUERY");

    // Oversize pattern still rejected.
    let big = "a".repeat(2000);
    let (status, body) = query(json!({"tag": {"$regex": big}})).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "INVALID_QUERY");
}

/// Restart seeding must stay EXACT — DocCounters is authoritative for
/// count_only — and now counts keys without materializing values. Covers
/// both engines and mixes custom ids with UUIDv7 ids (partially discharges
/// DT-17's restart half).
#[test]
fn test_doc_count_reseed_on_restart() {
    use wardsondb::engine::storage::MemoryConfig;

    for engine in ["rocksdb", "fjall"] {
        let tmp = TempDir::new().unwrap();
        {
            let storage =
                Storage::open_with_config(tmp.path(), engine, MemoryConfig::default()).unwrap();
            storage.create_collection("events").unwrap();
            for i in 0..25 {
                storage
                    .insert_document("events", json!({"seq": i}))
                    .unwrap();
            }
            for i in 0..5 {
                storage
                    .insert_document(
                        "events",
                        json!({"_id": format!("custom-{i}"), "seq": 100 + i}),
                    )
                    .unwrap();
            }
            assert_eq!(storage.doc_counts.get("events"), 30, "{engine}: live count");
        } // storage dropped — engine closed

        let storage =
            Storage::open_with_config(tmp.path(), engine, MemoryConfig::default()).unwrap();
        assert_eq!(
            storage.doc_counts.get("events"),
            30,
            "{engine}: reseeded count after restart must be exact"
        );
    }
}

/// Range-count boundary math: the backend count_range starts at the
/// successor of the exclusive-lower exact prefix — it must agree with the
/// materializing (non-count) path on every bound shape, including empty and
/// inverted windows.
#[tokio::test]
async fn test_count_range_exclusive_bounds() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "nums"}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..20).map(|i| json!({"seq": i})).collect();
    client
        .post(format!("{base_url}/nums/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/nums/indexes"))
        .json(&json!({"name": "idx_seq", "field": "seq"}))
        .send()
        .await
        .unwrap();

    let cases = [
        (json!({"seq": {"$gt": 5}}), 14u64),
        (json!({"seq": {"$gte": 5}}), 15),
        (json!({"seq": {"$lt": 5}}), 5),
        (json!({"seq": {"$lte": 5}}), 6),
        (json!({"seq": {"$gt": 5, "$lte": 10}}), 5),
        (json!({"seq": {"$gte": 5, "$lt": 10}}), 5),
        (json!({"seq": {"$gt": 4, "$lt": 5}}), 0),
        (json!({"seq": {"$gte": 10, "$lt": 5}}), 0), // inverted → guarded zero
    ];

    for (filter, expected) in cases {
        let resp = client
            .post(format!("{base_url}/nums/query"))
            .json(&json!({"filter": filter.clone(), "count_only": true}))
            .send()
            .await
            .unwrap();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(
            body["data"]["count"].as_u64().unwrap(),
            expected,
            "count_only for {filter}"
        );

        let resp = client
            .post(format!("{base_url}/nums/query"))
            .json(&json!({"filter": filter.clone(), "limit": 100}))
            .send()
            .await
            .unwrap();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(
            body["meta"]["total_count"].as_u64().unwrap(),
            expected,
            "non-count total_count for {filter}"
        );
        assert_eq!(
            body["data"].as_array().unwrap().len() as u64,
            expected,
            "returned docs for {filter}"
        );
    }
}

async fn setup_window_collection(base_url: &str, client: &Client) {
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();
    // 30 docs, event_type cycles 3 values → 10 "firewall" docs in insertion
    // (= UUIDv7 id = index within-value) order, seq marks identity.
    let types = ["firewall", "dns", "dhcp"];
    let docs: Vec<Value> = (0..30)
        .map(|i| {
            json!({
                "event_type": types[i % 3],
                "seq": i,
                "received_at": format!("2026-07-09T00:00:{i:02}Z"),
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_event_type", "field": "event_type"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_time", "fields": ["event_type", "received_at"]}))
        .send()
        .await
        .unwrap();
}

fn seqs(body: &Value) -> Vec<i64> {
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["seq"].as_i64().unwrap())
        .collect()
}

/// The windowed fast path must return byte-identical pages to the
/// materializing path (forced via an always-true residual, which keeps the
/// same candidate order but disables the window). docs_scanned proves which
/// path served each side.
#[tokio::test]
async fn test_index_eq_window_page_equivalence() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    setup_window_collection(&base_url, &client).await;

    for (offset, limit) in [(0u64, 3u64), (3, 3), (2, 5), (9, 5)] {
        let fast: Value = client
            .post(format!("{base_url}/events/query"))
            .json(&json!({
                "filter": {"event_type": "firewall"},
                "limit": limit, "offset": offset
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let slow: Value = client
            .post(format!("{base_url}/events/query"))
            .json(&json!({
                "filter": {"event_type": "firewall", "_id": {"$exists": true}},
                "limit": limit, "offset": offset
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(
            fast["data"], slow["data"],
            "page mismatch at offset {offset} limit {limit}"
        );
        assert_eq!(fast["meta"]["total_count"], 10);
        assert_eq!(fast["meta"]["index_used"], "idx_event_type");
        // Proof the fast path engaged: it loads only the window…
        let page_len = fast["data"].as_array().unwrap().len() as u64;
        assert_eq!(fast["meta"]["docs_scanned"].as_u64().unwrap(), page_len);
        // …while the residual-forced path STREAMS: it hydrates candidates
        // only until the page + probe row filled (offset matches skipped +
        // limit+1 kept), and reports an exact total only when the stream ran
        // to exhaustion — omitted exactly when has_more is true.
        let streamed = (offset + limit + 1).min(10);
        assert_eq!(
            slow["meta"]["docs_scanned"].as_u64().unwrap(),
            streamed,
            "residual side hydrates to the probe row at offset {offset} limit {limit}"
        );
        if offset + limit < 10 {
            assert!(
                slow["meta"]["total_count"].is_null(),
                "early-exited residual page must omit total_count"
            );
            assert_eq!(slow["meta"]["has_more"], true);
        } else {
            assert_eq!(slow["meta"]["total_count"], 10);
            assert_ne!(slow["meta"]["has_more"], true);
        }
    }
}

/// Contract §2 pin: growing-offset tiling with a constant filter must cover
/// every doc exactly once, matching a one-shot fetch.
#[tokio::test]
async fn test_index_eq_window_tiling() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    setup_window_collection(&base_url, &client).await;

    let one_shot: Value = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({"filter": {"event_type": "firewall"}, "limit": 100}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let expected = seqs(&one_shot);
    assert_eq!(expected.len(), 10);

    let mut tiled: Vec<i64> = Vec::new();
    let mut offset = 0u64;
    loop {
        let page: Value = client
            .post(format!("{base_url}/events/query"))
            .json(&json!({
                "filter": {"event_type": "firewall"},
                "limit": 3, "offset": offset
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        tiled.extend(seqs(&page));
        if page["meta"]["has_more"] != json!(true) {
            break;
        }
        offset += 3;
    }
    assert_eq!(tiled, expected, "tiled pages must equal the one-shot fetch");
}

/// Window framing edges: has_more flips exactly at the boundary; windows
/// straddling or past the end truncate/empty with exact total_count.
#[tokio::test]
async fn test_index_eq_window_boundaries() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    setup_window_collection(&base_url, &client).await;

    // (offset, limit, expected_len, expected_has_more) over 10 matches
    for (offset, limit, len, more) in [
        (6u64, 3u64, 3usize, true), // end=9 < 10
        (7, 3, 3, false),           // end=10 == total
        (9, 3, 1, false),           // straddles the end
        (10, 3, 0, false),          // exactly past the end
        (100, 3, 0, false),         // far past the end
    ] {
        let body: Value = client
            .post(format!("{base_url}/events/query"))
            .json(&json!({
                "filter": {"event_type": "firewall"},
                "limit": limit, "offset": offset
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            body["data"].as_array().unwrap().len(),
            len,
            "len at offset {offset}"
        );
        assert_eq!(
            body["meta"]["has_more"] == json!(true),
            more,
            "has_more at offset {offset}"
        );
        assert_eq!(body["meta"]["total_count"], 10, "total at offset {offset}");
    }
}

/// The compound-range window must match its residual-forced page and keep
/// the compound_range strategy label.
#[tokio::test]
async fn test_compound_range_window_page() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    setup_window_collection(&base_url, &client).await;

    // firewall docs are seq 0,3,…,27; received_at >= :15 → seqs 15..27 step 3 (5 docs)
    let range_filter = json!({
        "event_type": "firewall",
        "received_at": {"$gte": "2026-07-09T00:00:15Z"}
    });
    let mut residual_filter = range_filter.clone();
    residual_filter["_id"] = json!({"$exists": true});

    for (offset, limit) in [(0u64, 2u64), (1, 2), (3, 5)] {
        let fast: Value = client
            .post(format!("{base_url}/events/query"))
            .json(&json!({"filter": range_filter, "limit": limit, "offset": offset}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let slow: Value = client
            .post(format!("{base_url}/events/query"))
            .json(&json!({"filter": residual_filter, "limit": limit, "offset": offset}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(fast["meta"]["scan_strategy"], "compound_range");
        assert_eq!(slow["meta"]["scan_strategy"], "compound_range");
        assert_eq!(
            fast["data"], slow["data"],
            "page mismatch at offset {offset} limit {limit}"
        );
        assert_eq!(fast["meta"]["total_count"], 5);
        let page_len = fast["data"].as_array().unwrap().len() as u64;
        assert_eq!(fast["meta"]["docs_scanned"].as_u64().unwrap(), page_len);
    }
}

/// Bitmap window: pages must match the residual-forced bitmap path and tile
/// without skips or duplicates in ascending position (insertion) order.
#[tokio::test]
async fn test_bitmap_window_page() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..12)
        .map(|i| json!({"category": if i % 2 == 0 { "a" } else { "b" }, "seq": i}))
        .collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    let one_shot: Value = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"filter": {"category": "a"}, "limit": 100}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one_shot["meta"]["scan_strategy"], "bitmap");
    let expected = seqs(&one_shot);
    assert_eq!(expected, [0, 2, 4, 6, 8, 10]);

    // Window equivalence vs the residual-forced path.
    for (offset, limit) in [(0u64, 2u64), (2, 2), (4, 4)] {
        let fast: Value = client
            .post(format!("{base_url}/items/query"))
            .json(&json!({"filter": {"category": "a"}, "limit": limit, "offset": offset}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let slow: Value = client
            .post(format!("{base_url}/items/query"))
            .json(&json!({
                "filter": {"category": "a", "_id": {"$exists": true}},
                "limit": limit, "offset": offset
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(fast["meta"]["scan_strategy"], "bitmap");
        assert_eq!(slow["meta"]["scan_strategy"], "bitmap");
        assert_eq!(fast["data"], slow["data"], "bitmap page at offset {offset}");
        assert_eq!(fast["meta"]["total_count"], 6);
        let page_len = fast["data"].as_array().unwrap().len() as u64;
        assert_eq!(fast["meta"]["docs_scanned"].as_u64().unwrap(), page_len);
    }

    // Tiling.
    let mut tiled: Vec<i64> = Vec::new();
    let mut offset = 0u64;
    loop {
        let page: Value = client
            .post(format!("{base_url}/items/query"))
            .json(&json!({"filter": {"category": "a"}, "limit": 2, "offset": offset}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        tiled.extend(seqs(&page));
        if page["meta"]["has_more"] != json!(true) {
            break;
        }
        offset += 2;
    }
    assert_eq!(tiled, expected);
}

// ─── Cross-type value ordering (T3/R2) ────────────────────────────────────────

/// One doc per corner of the six encoding buckets (null < false < true <
/// number < string < array/object; 0x05 values order by serialized JSON
/// text). Custom `_id`s are assigned so `_id` order differs from encoding
/// order: a comparator that collapses cross-type pairs to the `_id` tiebreak
/// cannot accidentally produce the expected order.
const MIXED_ASC_IDS: [&str; 13] = [
    "m06", // null
    "m05", // false
    "m04", // true
    "m08", // -3.5
    "m10", // 7
    "m03", // 42
    "m11", // ""
    "m09", // "Zebra"  ('Z' < 'a')
    "m02", // "apple"
    "m07", // [1,2]    ("[1,2]" < "[]")
    "m12", // []
    "m01", // {"b":1}  ("{\"b\":1}" < "{}"; all arrays < all objects)
    "m13", // {}
];

async fn seed_mixed_type_fixture(client: &Client, base_url: &str, collection: &str) {
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": collection}))
        .send()
        .await
        .unwrap();
    let docs = json!({
        "documents": [
            {"_id": "m01", "val": {"b": 1}},
            {"_id": "m02", "val": "apple"},
            {"_id": "m03", "val": 42},
            {"_id": "m04", "val": true},
            {"_id": "m05", "val": false},
            {"_id": "m06", "val": null},
            {"_id": "m07", "val": [1, 2]},
            {"_id": "m08", "val": -3.5},
            {"_id": "m09", "val": "Zebra"},
            {"_id": "m10", "val": 7},
            {"_id": "m11", "val": ""},
            {"_id": "m12", "val": []},
            {"_id": "m13", "val": {}},
        ]
    });
    let resp = client
        .post(format!("{base_url}/{collection}/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
}

/// T3/R2: sorting a field holding every JSON type returns 200 (pre-fix the
/// intransitive comparator could panic Rust's total-order check → 500) and
/// orders by the index encoding's cross-type order, both directions.
#[tokio::test]
async fn test_mixed_type_sort_all_buckets() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_mixed_type_fixture(&client, &base_url, "mixed").await;

    let resp = client
        .post(format!("{base_url}/mixed/query"))
        .json(&json!({"sort": [{"val": "asc"}], "limit": 100}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(ids_of(body["data"].as_array().unwrap()), MIXED_ASC_IDS);

    let resp = client
        .post(format!("{base_url}/mixed/query"))
        .json(&json!({"sort": [{"val": "desc"}], "limit": 100}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let mut desc_expected = MIXED_ASC_IDS;
    desc_expected.reverse();
    assert_eq!(ids_of(body["data"].as_array().unwrap()), desc_expected);
}

/// T3/R2: cursor walks over a mixed-type sort field lose no documents. A
/// non-total comparator breaks `partition_point` monotonicity, silently
/// skipping docs between pages (and long mixed runs can panic the sort).
#[tokio::test]
async fn test_mixed_type_cursor_walk_matches_reference() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "mixedwalk"}))
        .send()
        .await
        .unwrap();
    // Values anti-correlate with `_id`s (59 - i): a comparator that collapses
    // cross-type pairs onto the `_id` tiebreak produces an order that
    // conflicts with the within-type order, which is exactly what breaks
    // `partition_point` monotonicity on cursor resume.
    let docs: Vec<Value> = (0..60)
        .map(|i| {
            let val = match i % 6 {
                0 => json!(59 - i),
                1 => json!(format!("s{:02}", 59 - i)),
                2 => json!(i % 4 == 2),
                3 => json!(null),
                4 => json!([59 - i]),
                _ => json!({"k": 59 - i}),
            };
            json!({"_id": format!("d{i:02}"), "val": val})
        })
        .collect();
    let resp = client
        .post(format!("{base_url}/mixedwalk/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    for direction in ["asc", "desc"] {
        let body = json!({"sort": [{"val": direction}], "limit": 7});
        let walked = cursor_walk(&client, &base_url, "mixedwalk", body.clone()).await;
        let reference = reference_ids(&client, &base_url, "mixedwalk", body).await;
        assert_eq!(walked.len(), 60, "no docs dropped ({direction})");
        assert_eq!(ids_of(&walked), reference, "walk == one-shot ({direction})");
    }
}

/// T3/R2: the in-memory comparator and the index encoding agree — the same
/// mixed-type query returns byte-identical order as a full scan and as an
/// `index_sorted` walk over a covering compound index.
#[tokio::test]
async fn test_mixed_type_index_sorted_matches_in_memory() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "mixedidx"}))
        .send()
        .await
        .unwrap();
    // Every doc carries both fields (explicit null, never missing: docs
    // missing an indexed field are absent from the index — T6, out of scope).
    let docs = json!({
        "documents": [
            {"_id": "i01", "group": "g", "val": {"o": 2}},
            {"_id": "i02", "group": "g", "val": "mango"},
            {"_id": "i03", "group": "g", "val": 99},
            {"_id": "i04", "group": "g", "val": true},
            {"_id": "i05", "group": "g", "val": null},
            {"_id": "i06", "group": "g", "val": [5]},
            {"_id": "i07", "group": "g", "val": false},
            {"_id": "i08", "group": "g", "val": -1},
            {"_id": "i09", "group": "g", "val": "Apple"},
            {"_id": "i10", "group": "g", "val": {"a": 1}},
            {"_id": "i11", "group": "g", "val": 7},
            {"_id": "i12", "group": "g", "val": 7},
        ]
    });
    let resp = client
        .post(format!("{base_url}/mixedidx/docs/_bulk"))
        .json(&json!(docs))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let query = |dir: &str| json!({"filter": {"group": "g"}, "sort": [{"val": dir}], "limit": 50});

    // Reference order first: full scan + in-memory sort (no index exists yet).
    let mut in_memory = std::collections::HashMap::new();
    for dir in ["asc", "desc"] {
        let resp = client
            .post(format!("{base_url}/mixedidx/query"))
            .json(&query(dir))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        in_memory.insert(dir, ids_of(body["data"].as_array().unwrap()));
    }

    let resp = client
        .post(format!("{base_url}/mixedidx/indexes"))
        .json(&json!({"name": "idx_group_val", "fields": ["group", "val"]}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    for dir in ["asc", "desc"] {
        let resp = client
            .post(format!("{base_url}/mixedidx/query"))
            .json(&query(dir))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(
            body["meta"]["scan_strategy"], "index_sorted",
            "compound index must serve the sorted query ({dir})"
        );
        assert_eq!(
            ids_of(body["data"].as_array().unwrap()),
            in_memory[dir],
            "index order == in-memory order ({dir})"
        );
    }
}

/// T3: `$min`/`$max` on a mixed-type group return the encoding-order extremes
/// (pre-fix: cross-type pairs compared Equal, so whichever value was scanned
/// first stuck as both min and max). The strings-only group pins the
/// unaffected same-type behavior alongside.
#[tokio::test]
async fn test_min_max_mixed_and_single_type() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "acc"}))
        .send()
        .await
        .unwrap();
    // "x" first: the pre-fix first-seen-wins accumulator would report
    // min == max == "x", never null / {"z":1}.
    let docs = json!({
        "documents": [
            {"g": "mixed", "v": "x"},
            {"g": "mixed", "v": 9},
            {"g": "mixed", "v": false},
            {"g": "mixed", "v": null},
            {"g": "mixed", "v": [1]},
            {"g": "mixed", "v": {"z": 1}},
            {"g": "pure", "v": "zeta"},
            {"g": "pure", "v": "alpha"},
            {"g": "pure", "v": "beta"},
        ]
    });
    client
        .post(format!("{base_url}/acc/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{base_url}/acc/aggregate"))
        .json(&json!({
            "pipeline": [
                {"$group": {"_id": "g", "min_v": {"$min": "v"}, "max_v": {"$max": "v"}}},
                {"$sort": {"_id": "asc"}}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);

    let mixed = &data[0];
    assert_eq!(mixed["_id"], "mixed");
    assert!(mixed.as_object().unwrap().contains_key("min_v"));
    assert_eq!(mixed["min_v"], Value::Null, "encoding minimum is null");
    assert_eq!(
        mixed["max_v"],
        json!({"z": 1}),
        "encoding maximum is the object"
    );

    let pure = &data[1];
    assert_eq!(pure["_id"], "pure");
    assert_eq!(pure["min_v"], "alpha");
    assert_eq!(pure["max_v"], "zeta");
}

/// T3: `$collect` over mixed types returns 200 (its finalize sorts collected
/// values — pre-fix that sort could panic) with values in encoding order
/// (pre-fix: effectively HashSet iteration order).
#[tokio::test]
async fn test_collect_mixed_types_sorted_encoding_order() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "coll5"}))
        .send()
        .await
        .unwrap();
    let docs = json!({
        "documents": [
            {"v": {"m": 1}},
            {"v": "a"},
            {"v": -2},
            {"v": true},
            {"v": null},
            {"v": [3]},
            {"v": false},
            {"v": 10},
        ]
    });
    client
        .post(format!("{base_url}/coll5/docs/_bulk"))
        .json(&docs)
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("{base_url}/coll5/aggregate"))
        .json(&json!({
            "pipeline": [{"$group": {"_id": null, "vals": {"$collect": "v"}}}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(
        data[0]["vals"],
        json!([null, false, true, -2, 10, "a", [3], {"m": 1}])
    );
}

/// T3/R2: the aggregate `$sort` stage uses the same fixed comparator.
#[tokio::test]
async fn test_aggregate_sort_mixed_types() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_mixed_type_fixture(&client, &base_url, "aggmixed").await;

    for (dir, expected) in [
        ("asc", MIXED_ASC_IDS),
        ("desc", {
            let mut rev = MIXED_ASC_IDS;
            rev.reverse();
            rev
        }),
    ] {
        let resp = client
            .post(format!("{base_url}/aggmixed/aggregate"))
            .json(&json!({"pipeline": [{"$sort": [{"val": dir}]}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(
            ids_of(body["data"].as_array().unwrap()),
            expected,
            "aggregate $sort {dir}"
        );
    }
}

// ─── Type-bracketed index range scans (R10/S3-10) ─────────────────────────────

/// Same docs into an indexed collection and an unindexed twin: range results
/// must be identical. Docs carry the field explicitly (null, never missing —
/// missing-field index exclusion is T6, out of scope here).
async fn seed_range_twins(client: &Client, base_url: &str) {
    for coll in ["vals_idx", "vals_plain"] {
        client
            .post(format!("{base_url}/_collections"))
            .json(&json!({"name": coll}))
            .send()
            .await
            .unwrap();
        let docs = json!({
            "documents": [
                {"_id": "v01", "val": 1},
                {"_id": "v02", "val": 5},
                {"_id": "v03", "val": 9},
                {"_id": "v04", "val": 9},
                {"_id": "v05", "val": "a"},
                {"_id": "v06", "val": "m"},
                {"_id": "v07", "val": true},
                {"_id": "v08", "val": false},
                {"_id": "v09", "val": null},
                {"_id": "v10", "val": [1]},
                {"_id": "v11", "val": {"k": 1}},
                {"_id": "v12", "val": 5},
            ]
        });
        let resp = client
            .post(format!("{base_url}/{coll}/docs/_bulk"))
            .json(&docs)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }
    let resp = client
        .post(format!("{base_url}/vals_idx/indexes"))
        .json(&json!({"name": "idx_val", "field": "val"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
}

/// The sweep every range test below shares: operator × operand-type cases
/// with their expected matching ids under type-bracketed semantics.
fn range_bracket_cases() -> Vec<(Value, Vec<&'static str>)> {
    vec![
        (json!({"$gt": 5}), vec!["v03", "v04"]),
        (json!({"$gte": 5}), vec!["v02", "v03", "v04", "v12"]),
        (json!({"$lt": 5}), vec!["v01"]),
        (json!({"$lte": 5}), vec!["v01", "v02", "v12"]),
        (json!({"$gt": "c"}), vec!["v06"]),
        (json!({"$lte": "a"}), vec!["v05"]),
        (json!({"$gt": false}), vec!["v07"]),
        (json!({"$gte": false}), vec!["v07", "v08"]),
        (json!({"$lt": true}), vec!["v08"]),
        (json!({"$lte": true}), vec!["v07", "v08"]),
        (json!({"$gt": null}), vec![]),
        (json!({"$gte": null}), vec![]),
        (json!({"$lt": [2]}), vec![]),
        (json!({"$gte": {"k": 1}}), vec![]),
    ]
}

/// R10: indexed range queries return exactly what the in-memory filter
/// matches — no cross-type leakage past open bounds (pre-fix an indexed
/// `$gt: 5` returned strings/arrays/objects; `$gt: null` returned every
/// non-null doc), and null/array/object operands match nothing on any path.
#[tokio::test]
async fn test_index_range_mixed_types_matches_full_scan() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_range_twins(&client, &base_url).await;

    for (op, expected) in range_bracket_cases() {
        let filter = json!({"val": op});
        let mut results = Vec::new();
        for coll in ["vals_idx", "vals_plain"] {
            let resp = client
                .post(format!("{base_url}/{coll}/query"))
                .json(&json!({"filter": filter, "limit": 1000}))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let body: Value = resp.json().await.unwrap();
            if coll == "vals_idx" {
                assert_eq!(
                    body["meta"]["index_used"], "idx_val",
                    "index must serve {filter}"
                );
            }
            let mut ids = ids_of(body["data"].as_array().unwrap());
            ids.sort();
            results.push(ids);
        }
        assert_eq!(results[0], results[1], "indexed == full scan for {filter}");
        assert_eq!(results[0], expected, "expected match set for {filter}");
    }
}

/// R10: `count_only` takes the keys-only `count_range` path — counts must
/// bracket identically.
#[tokio::test]
async fn test_count_range_mixed_types_matches_full_scan() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_range_twins(&client, &base_url).await;

    for (op, expected) in range_bracket_cases() {
        let filter = json!({"val": op});
        let mut counts = Vec::new();
        for (coll, strategy) in [("vals_idx", "index_range"), ("vals_plain", "full_scan")] {
            let resp = client
                .post(format!("{base_url}/{coll}/query"))
                .json(&json!({"filter": filter, "count_only": true}))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let body: Value = resp.json().await.unwrap();
            assert_eq!(
                body["meta"]["scan_strategy"], strategy,
                "count strategy for {filter} on {coll}"
            );
            counts.push(body["meta"]["total_count"].as_u64().unwrap());
        }
        assert_eq!(
            counts[0], counts[1],
            "indexed == full scan count for {filter}"
        );
        assert_eq!(counts[0], expected.len() as u64, "count for {filter}");
    }
}

/// R10: bounds from two different type buckets can never both hold for one
/// value — empty everywhere, without scanning.
#[tokio::test]
async fn test_index_range_cross_type_bounds_empty() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_range_twins(&client, &base_url).await;

    let filter = json!({"val": {"$gt": 5, "$lt": "z"}});
    for coll in ["vals_idx", "vals_plain"] {
        let resp = client
            .post(format!("{base_url}/{coll}/query"))
            .json(&json!({"filter": filter, "limit": 1000}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(
            body["data"].as_array().unwrap().len(),
            0,
            "cross-bucket bounds match nothing on {coll}"
        );

        let resp = client
            .post(format!("{base_url}/{coll}/query"))
            .json(&json!({"filter": filter, "count_only": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["meta"]["total_count"], 0, "count on {coll}");
    }
}

/// R10: the CompoundRange path (equality prefix + range suffix) brackets its
/// range field the same way — cross-type entries inside the equality group
/// stay excluded, and other groups never leak in.
#[tokio::test]
async fn test_compound_range_mixed_types_matches_full_scan() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    for coll in ["cvals_idx", "cvals_plain"] {
        client
            .post(format!("{base_url}/_collections"))
            .json(&json!({"name": coll}))
            .send()
            .await
            .unwrap();
        let docs = json!({
            "documents": [
                {"_id": "c01", "grp": "a", "val": 1},
                {"_id": "c02", "grp": "a", "val": 5},
                {"_id": "c03", "grp": "a", "val": 9},
                {"_id": "c04", "grp": "a", "val": "m"},
                {"_id": "c05", "grp": "a", "val": true},
                {"_id": "c06", "grp": "a", "val": null},
                {"_id": "c07", "grp": "a", "val": [1]},
                {"_id": "c08", "grp": "b", "val": 5},
            ]
        });
        let resp = client
            .post(format!("{base_url}/{coll}/docs/_bulk"))
            .json(&docs)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }
    let resp = client
        .post(format!("{base_url}/cvals_idx/indexes"))
        .json(&json!({"name": "idx_grp_val", "fields": ["grp", "val"]}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let cases: Vec<(Value, Vec<&str>)> = vec![
        (json!({"$gt": 5}), vec!["c03"]),
        (json!({"$lt": 5}), vec!["c01"]),
        (json!({"$gte": "c"}), vec!["c04"]),
        (json!({"$gt": null}), vec![]),
        (json!({"$gt": 5, "$lte": "z"}), vec![]),
    ];
    for (op, expected) in cases {
        let filter = json!({"grp": "a", "val": op});
        for count_only in [false, true] {
            let mut per_coll = Vec::new();
            for coll in ["cvals_idx", "cvals_plain"] {
                let body_json = if count_only {
                    json!({"filter": filter, "count_only": true})
                } else {
                    json!({"filter": filter, "limit": 1000})
                };
                let resp = client
                    .post(format!("{base_url}/{coll}/query"))
                    .json(&body_json)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(resp.status(), 200);
                let body: Value = resp.json().await.unwrap();
                if coll == "cvals_idx" {
                    assert_eq!(
                        body["meta"]["scan_strategy"], "compound_range",
                        "compound index must serve {filter} (count_only={count_only})"
                    );
                }
                if count_only {
                    per_coll.push(vec![format!(
                        "count:{}",
                        body["meta"]["total_count"].as_u64().unwrap()
                    )]);
                } else {
                    let mut ids = ids_of(body["data"].as_array().unwrap());
                    ids.sort();
                    per_coll.push(ids);
                }
            }
            assert_eq!(
                per_coll[0], per_coll[1],
                "indexed == full scan for {filter} (count_only={count_only})"
            );
            if !count_only {
                assert_eq!(per_coll[0], expected, "expected match set for {filter}");
            } else {
                assert_eq!(
                    per_coll[0][0],
                    format!("count:{}", expected.len()),
                    "expected count for {filter}"
                );
            }
        }
    }
}

/// DT-9/R3: long-but-legal sort values roundtrip through a full cursor walk;
/// oversize values omit `next_cursor` at emission instead of handing the
/// client a token the server itself rejects on replay (pre-fix: the emitted
/// cursor 400'd, making page 2 unreachable).
#[tokio::test]
async fn test_cursor_size_guard_long_sort_values() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    // ~2 KiB sort values: cursors stay under MAX_CURSOR_LEN and a paged walk
    // must concatenate to the one-shot reference.
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "longsort"}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..8)
        .map(|i| json!({"_id": format!("L{i}"), "val": format!("{}{i:02}", "x".repeat(2000))}))
        .collect();
    let resp = client
        .post(format!("{base_url}/longsort/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let body = json!({"sort": [{"val": "asc"}], "limit": 3});
    let walked = cursor_walk(&client, &base_url, "longsort", body.clone()).await;
    assert_eq!(walked.len(), 8, "no docs dropped on long-value walk");
    assert_eq!(
        ids_of(&walked),
        reference_ids(&client, &base_url, "longsort", body).await
    );

    // ~5 KiB sort values: the page still succeeds with exact has_more, but
    // next_cursor is omitted (the token would exceed the decode cap).
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "hugesort"}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..6)
        .map(|i| json!({"_id": format!("H{i}"), "val": format!("{}{i:02}", "y".repeat(5000))}))
        .collect();
    let resp = client
        .post(format!("{base_url}/hugesort/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let resp = client
        .post(format!("{base_url}/hugesort/query"))
        .json(&json!({"sort": [{"val": "asc"}], "limit": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(body["meta"]["has_more"], true, "has_more stays exact");
    assert!(
        body["meta"]["next_cursor"].is_null(),
        "oversize boundary value must omit next_cursor"
    );
}

/// DT-13/S3-7: sort values containing the index key encoding's separator
/// bytes — NUL (the doc-id separator) and 0x01 (the compound field
/// separator) — resume correctly through `index_sorted` cursor seeks. The
/// seek key builder (`index_cursor_key`) must stay byte-identical to
/// `make_compound_index_key`, or a walk skips/repeats around these values.
#[tokio::test]
async fn test_cursor_walk_index_sorted_separator_bytes() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "seps"}))
        .send()
        .await
        .unwrap();
    // Byte order of the values: "a" < "a\0b" < "a\0c" < "a\u{1}b" < "ab"
    // < "b" < "b\0" — adjacent pairs differ exactly around the separator
    // bytes a wrongly-built seek key would collide with.
    let vals = [
        "a",
        "a\u{0000}b",
        "a\u{0000}c",
        "a\u{0001}b",
        "ab",
        "b",
        "b\u{0000}",
    ];
    let docs: Vec<Value> = vals
        .iter()
        .enumerate()
        .map(|(i, v)| json!({"_id": format!("s{i}"), "grp": "g", "val": v}))
        .collect();
    let resp = client
        .post(format!("{base_url}/seps/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let resp = client
        .post(format!("{base_url}/seps/indexes"))
        .json(&json!({"name": "idx_grp_val", "fields": ["grp", "val"]}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    for dir in ["asc", "desc"] {
        let body = json!({
            "filter": {"grp": "g"},
            "sort": [{"val": dir}],
            "limit": 2
        });
        // Inline walk so every page can assert it stayed on the seek path.
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut req = body.clone();
            if let Some(c) = &cursor {
                req["cursor"] = json!(c);
            }
            let resp = client
                .post(format!("{base_url}/seps/query"))
                .json(&req)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let resp_body: Value = resp.json().await.unwrap();
            assert_eq!(
                resp_body["meta"]["scan_strategy"], "index_sorted",
                "direction {dir}: cursor pages must stay on the seek path"
            );
            all.extend(resp_body["data"].as_array().unwrap().iter().cloned());
            match resp_body["meta"]["next_cursor"].as_str() {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }
        assert_eq!(all.len(), vals.len(), "direction {dir}: no skips, no dups");
        assert_eq!(
            ids_of(&all),
            reference_ids(&client, &base_url, "seps", body).await,
            "direction {dir}: pages must equal the one-shot result"
        );
    }
}

// ─── $or index-union planning (H-P3.1) ────────────────────────────────────────

/// Twin collections: `orvals_idx` carries single-field indexes on `t` and
/// `n`; `orvals_plain` carries none, so it full-scans — the reference the
/// union must match byte-for-byte. `u` stays unindexed everywhere.
async fn seed_or_union_twins(client: &Client, base_url: &str) {
    for coll in ["orvals_idx", "orvals_plain"] {
        client
            .post(format!("{base_url}/_collections"))
            .json(&json!({"name": coll}))
            .send()
            .await
            .unwrap();
        let docs = json!({
            "documents": [
                {"_id": "o01", "t": "a", "n": 1,  "u": "x"},
                {"_id": "o02", "t": "a", "n": 5,  "u": "x"},
                {"_id": "o03", "t": "b", "n": 5,  "u": "x"},
                {"_id": "o04", "t": "b", "n": 9,  "u": "x"},
                {"_id": "o05", "t": "c", "n": 2,  "u": "x"},
                {"_id": "o06", "t": "c", "n": 7,  "u": "x"},
                {"_id": "o07", "t": "d", "n": 3,  "u": "y"},
                {"_id": "o08", "t": "d", "n": 8,  "u": "y"},
                {"_id": "o09", "t": "e", "n": 4,  "u": "y"},
                {"_id": "o10", "t": "e", "n": 6,  "u": "y"},
                {"_id": "o11", "t": "a", "n": 10, "u": "y"},
                {"_id": "o12", "t": "b", "n": 0,  "u": "y"},
            ]
        });
        let resp = client
            .post(format!("{base_url}/{coll}/docs/_bulk"))
            .json(&docs)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }
    for (name, field) in [("idx_t", "t"), ("idx_n", "n")] {
        let resp = client
            .post(format!("{base_url}/orvals_idx/indexes"))
            .json(&json!({"name": name, "field": field}))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }
}

/// H-P3.1: a fully-indexable `$or` plans the per-arm union and returns
/// byte-identical pages to the full scan it replaces — verified by tiling
/// both twins across offset windows for eq/eq, eq/range (with an
/// arm-overlapping doc), and in/range shapes.
#[tokio::test]
async fn test_or_union_matches_full_scan() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_or_union_twins(&client, &base_url).await;

    let shapes = [
        json!({"$or": [{"t": "a"}, {"t": "b"}]}),
        json!({"$or": [{"t": "a"}, {"n": {"$gte": 8}}]}), // o11 matches both arms
        json!({"$or": [{"t": {"$in": ["c", "e"]}}, {"n": {"$lt": 2}}]}),
    ];
    for filter in &shapes {
        // One-shot equality + union metadata.
        let idx: Value = client
            .post(format!("{base_url}/orvals_idx/query"))
            .json(&json!({"filter": filter, "limit": 100}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let plain: Value = client
            .post(format!("{base_url}/orvals_plain/query"))
            .json(&json!({"filter": filter, "limit": 100}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            ids_of(idx["data"].as_array().unwrap()),
            ids_of(plain["data"].as_array().unwrap()),
            "one-shot ids and order must match for {filter}"
        );
        assert_eq!(
            idx["meta"]["scan_strategy"], "or_union",
            "union must serve {filter}"
        );
        assert!(
            idx["meta"]["index_used"].as_str().unwrap().contains("idx_"),
            "index_used names the arm indexes for {filter}"
        );
        // The union reads only per-arm candidates, never the collection.
        let matches = plain["data"].as_array().unwrap().len() as u64;
        assert!(
            idx["meta"]["docs_scanned"].as_u64().unwrap() <= matches,
            "union must not scan beyond its candidates for {filter}"
        );

        // Bare-page tiling: identical windows on both twins.
        let mut offset = 0u64;
        loop {
            let idx_page: Value = client
                .post(format!("{base_url}/orvals_idx/query"))
                .json(&json!({"filter": filter, "limit": 3, "offset": offset}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let plain_page: Value = client
                .post(format!("{base_url}/orvals_plain/query"))
                .json(&json!({"filter": filter, "limit": 3, "offset": offset}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(
                ids_of(idx_page["data"].as_array().unwrap()),
                ids_of(plain_page["data"].as_array().unwrap()),
                "tiling window at offset {offset} for {filter}"
            );
            if idx_page["meta"]["has_more"] != json!(true) {
                break;
            }
            offset += 3;
        }
    }
}

/// H-P3.1: `count_only` over exact arms counts the deduped union with zero
/// document loads; a residual (And) arm still counts correctly through the
/// materialized path.
#[tokio::test]
async fn test_or_union_count_only() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_or_union_twins(&client, &base_url).await;

    // Exact arms — o11 matches both, so a naive per-arm count sum would
    // report 6; the deduped union must say 5.
    let filter = json!({"$or": [{"t": "a"}, {"n": {"$gte": 8}}]});
    let idx: Value = client
        .post(format!("{base_url}/orvals_idx/query"))
        .json(&json!({"filter": filter, "count_only": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(idx["meta"]["scan_strategy"], "or_union");
    assert_eq!(idx["meta"]["total_count"], 5);
    assert_eq!(
        idx["meta"]["docs_scanned"], 0,
        "exact union counts keys only"
    );

    let plain: Value = client
        .post(format!("{base_url}/orvals_plain/query"))
        .json(&json!({"filter": filter, "count_only": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(plain["meta"]["total_count"], 5);
    assert_eq!(plain["meta"]["scan_strategy"], "full_scan");

    // Residual arm: {t:"a", u:"x"} — u is unindexed, so the arm
    // over-approximates and the union post-filters with the original $or.
    let filter = json!({"$or": [{"t": "a", "u": "x"}, {"n": 9}]});
    let idx: Value = client
        .post(format!("{base_url}/orvals_idx/query"))
        .json(&json!({"filter": filter, "count_only": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // o01, o02 (t=a,u=x), o04 (n=9); o11 (t=a,u=y) must be filtered out.
    assert_eq!(idx["meta"]["scan_strategy"], "or_union");
    assert_eq!(idx["meta"]["total_count"], 3);
    assert!(
        idx["meta"]["docs_scanned"].as_u64().unwrap() > 0,
        "residual arms load candidates to filter"
    );
}

/// H-P3.1: an over-approximating And arm's stray candidates are removed by
/// the original-$or post-filter — doc results match the full scan exactly.
#[tokio::test]
async fn test_or_union_residual_and_arm() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_or_union_twins(&client, &base_url).await;

    let filter = json!({"$or": [{"t": "a", "u": "x"}, {"n": 9}]});
    let idx: Value = client
        .post(format!("{base_url}/orvals_idx/query"))
        .json(&json!({"filter": filter, "limit": 100}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plain: Value = client
        .post(format!("{base_url}/orvals_plain/query"))
        .json(&json!({"filter": filter, "limit": 100}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(idx["meta"]["scan_strategy"], "or_union");
    assert_eq!(
        ids_of(idx["data"].as_array().unwrap()),
        ids_of(plain["data"].as_array().unwrap()),
        "union results must match the full scan"
    );
    assert_eq!(
        ids_of(idx["data"].as_array().unwrap()),
        vec!["o01", "o02", "o04"],
        "over-approximated candidate o11 must be post-filtered out"
    );
}

/// H-P3.1: the union only engages when EVERY arm is index-servable — one
/// unindexed or nested-$or arm keeps today's full-scan behavior.
#[tokio::test]
async fn test_or_union_requires_all_arms_indexed() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_or_union_twins(&client, &base_url).await;

    for filter in [
        json!({"$or": [{"t": "a"}, {"u": "x"}]}), // u unindexed
        json!({"$or": [{"$or": [{"t": "a"}]}, {"n": 5}]}), // nested $or arm
    ] {
        let resp: Value = client
            .post(format!("{base_url}/orvals_idx/query"))
            .json(&json!({"filter": filter, "count_only": true}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            resp["meta"]["scan_strategy"], "full_scan",
            "{filter} must fall back to a full scan"
        );
        // And the fallback still answers correctly.
        let plain: Value = client
            .post(format!("{base_url}/orvals_plain/query"))
            .json(&json!({"filter": filter, "count_only": true}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp["meta"]["total_count"], plain["meta"]["total_count"]);
    }
}

/// H-P3.1: sorted, cursor-paginated `$or` rides the materializing machinery —
/// a paged walk equals the one-shot result and the full-scan twin.
#[tokio::test]
async fn test_or_union_with_sort_and_cursor_walk() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_or_union_twins(&client, &base_url).await;

    let body = json!({
        "filter": {"$or": [{"t": "a"}, {"t": "b"}]},
        "sort": [{"n": "desc"}],
        "limit": 2
    });
    let walked = cursor_walk(&client, &base_url, "orvals_idx", body.clone()).await;
    assert_eq!(walked.len(), 6);
    assert_eq!(
        ids_of(&walked),
        reference_ids(&client, &base_url, "orvals_idx", body.clone()).await,
        "walk == one-shot on the indexed twin"
    );
    assert_eq!(
        ids_of(&walked),
        reference_ids(&client, &base_url, "orvals_plain", body).await,
        "walk == full-scan reference"
    );
}

/// H-P3.1 × bitmap: a fully-covered `$or` stays on the bitmap path (higher
/// priority); partial bitmap coverage — which bails per S3-1 — now upgrades
/// to the index union instead of a full scan when the arms are indexed.
#[tokio::test]
async fn test_or_union_bitmap_priority_and_partial_upgrade() {
    let (base_url, _tmp) = start_test_server_with_bitmap("cat").await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..12)
        .map(|i| {
            json!({
                "_id": format!("b{i:02}"),
                "cat": if i % 3 == 0 { "x" } else { "y" },
                "num": i
            })
        })
        .collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();
    for (name, field) in [("idx_cat", "cat"), ("idx_num", "num")] {
        client
            .post(format!("{base_url}/items/indexes"))
            .json(&json!({"name": name, "field": field}))
            .send()
            .await
            .unwrap();
    }

    // Fully bitmap-covered $or: bitmap outranks the union.
    let full: Value = client
        .post(format!("{base_url}/items/query"))
        .json(&json!({"filter": {"$or": [{"cat": "x"}, {"cat": "y"}]}, "count_only": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(full["meta"]["scan_strategy"], "bitmap");
    assert_eq!(full["meta"]["total_count"], 12);

    // Partially covered (num has no bitmap column): S3-1 bails the bitmap —
    // the union now serves it instead of a full scan.
    let partial: Value = client
        .post(format!("{base_url}/items/query"))
        .json(
            &json!({"filter": {"$or": [{"cat": "x"}, {"num": {"$gte": 10}}]}, "count_only": true}),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(partial["meta"]["scan_strategy"], "or_union");
    // cat=x → b00,b03,b06,b09; num>=10 → b10,b11 (no overlap) = 6.
    assert_eq!(partial["meta"]["total_count"], 6);
    assert_eq!(partial["meta"]["docs_scanned"], 0);
}

// ─── Backend-parity slice: fjall coverage (DT-1, DT-19) ───────────────────────
//
// The per-commit suite defaults to rocksdb; these run the cursor matrix and
// the windowed-page machinery on fjall so BOTH engines stay guarded while
// H-P2 rewrites BackendIterator. Same fixtures/assertions as their rocksdb
// twins — only the engine differs.

/// Engine-parameterized twin of `start_test_server_with_bitmap`.
async fn start_test_server_with_engine_and_bitmap(
    engine: &str,
    bitmap_fields: &str,
) -> (String, TempDir) {
    use wardsondb::engine::storage::MemoryConfig;

    let tmp = TempDir::new().unwrap();
    let storage = Storage::open_with_config(tmp.path(), engine, MemoryConfig::default()).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut config = test_config(&tmp, port);
    config.storage_engine = engine.to_string();
    config.bitmap_fields = bitmap_fields.to_string();

    if !bitmap_fields.is_empty() {
        let fields: Vec<String> = bitmap_fields
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        storage.scan_accelerator.configure_fields(fields);
        storage.scan_accelerator.set_ready(true);
    }

    let state = Arc::new(AppState {
        storage,
        config,
        started_at: Instant::now(),
        metrics: Arc::new(Metrics::new()),
        api_keys: vec![],
    });

    let app = build_router(state);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    let base_url = format!("http://{addr}");

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (base_url, tmp)
}

/// DT-1: the ASCENDING index-seek walk on fjall (the desc twin is B-15) —
/// forward `range_iterator` seek + limit+1 probe end-to-end over HTTP.
#[tokio::test]
async fn test_fjall_cursor_walk_index_sorted_asc() {
    let (base_url, _tmp) = start_test_server_with_engine("fjall").await;
    let client = Client::new();
    seed_index_sorted_collection(&client, &base_url).await;

    let body = json!({
        "filter": {"event_type": "fw"},
        "sort": [{"received_at": "asc"}],
        "limit": 4
    });

    let probe = client
        .post(format!("{base_url}/events/query"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let probe_body: Value = probe.json().await.unwrap();
    assert_eq!(probe_body["meta"]["scan_strategy"], "index_sorted");

    let walked = cursor_walk(&client, &base_url, "events", body.clone()).await;
    assert_eq!(walked.len(), 15);
    assert_eq!(
        ids_of(&walked),
        reference_ids(&client, &base_url, "events", body).await
    );
}

/// DT-1: the materializing cursor path on fjall — full scan + in-memory sort
/// with duplicate sort values and docs missing the sort field.
#[tokio::test]
async fn test_fjall_cursor_walk_materializing() {
    let (base_url, _tmp) = start_test_server_with_engine("fjall").await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "items"}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..25)
        .map(|i| {
            if i % 5 == 4 {
                json!({"n": i}) // missing sort field
            } else {
                json!({"score": i % 4, "n": i}) // duplicate-heavy sort values
            }
        })
        .collect();
    client
        .post(format!("{base_url}/items/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    let body = json!({"sort": [{"score": "asc"}], "limit": 4});
    let walked = cursor_walk(&client, &base_url, "items", body.clone()).await;
    assert_eq!(walked.len(), 25);
    assert_eq!(
        ids_of(&walked),
        reference_ids(&client, &base_url, "items", body).await
    );
}

/// DT-1: the bitmap cursor walk on fjall (accelerator positions resolve
/// against fjall-backed docs; materializing layer sorts + paginates).
#[tokio::test]
async fn test_fjall_cursor_walk_bitmap() {
    let (base_url, _tmp) = start_test_server_with_engine_and_bitmap("fjall", "event_type").await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..30)
        .map(|i| {
            json!({
                "event_type": if i % 3 == 0 { "dns" } else { "firewall" },
                "value": (i * 7) % 10,
                "n": i
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    let body = json!({
        "filter": {"event_type": "firewall"},
        "sort": [{"value": "asc"}],
        "limit": 4
    });
    let probe = client
        .post(format!("{base_url}/events/query"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let probe_body: Value = probe.json().await.unwrap();
    assert_eq!(probe_body["meta"]["scan_strategy"], "bitmap");

    let walked = cursor_walk(&client, &base_url, "events", body.clone()).await;
    assert_eq!(walked.len(), 20);
    assert_eq!(
        ids_of(&walked),
        reference_ids(&client, &base_url, "events", body).await
    );
}

/// DT-1: the compound-range cursor walk on fjall (eq prefix + range suffix
/// through the shared bounds builder, sorted in memory by an uncovered field).
#[tokio::test]
async fn test_fjall_cursor_walk_compound_range() {
    let (base_url, _tmp) = start_test_server_with_engine("fjall").await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "events"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/events/indexes"))
        .json(&json!({"name": "idx_type_ts", "fields": ["event_type", "ts"]}))
        .send()
        .await
        .unwrap();

    let docs: Vec<Value> = (0..24)
        .map(|i| {
            json!({
                "event_type": if i % 2 == 0 { "fw" } else { "dns" },
                "ts": i,
                "other": (i * 5) % 7,
                "n": i
            })
        })
        .collect();
    client
        .post(format!("{base_url}/events/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    let body = json!({
        "filter": {"event_type": "fw", "ts": {"$gte": 4, "$lte": 18}},
        "sort": [{"other": "asc"}],
        "limit": 3
    });

    let probe = client
        .post(format!("{base_url}/events/query"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let probe_body: Value = probe.json().await.unwrap();
    assert_eq!(probe_body["meta"]["scan_strategy"], "compound_range");

    let walked = cursor_walk(&client, &base_url, "events", body.clone()).await;
    assert_eq!(walked.len(), 8); // ts in {4,6,8,10,12,14,16,18}
    assert_eq!(
        ids_of(&walked),
        reference_ids(&client, &base_url, "events", body).await
    );
}

/// DT-19: the M2 windowed-page machinery on fjall — window-vs-residual
/// equivalence with the docs_scanned == page-size proof, growing-offset
/// tiling == one-shot, and the or_union window on the same collection.
#[tokio::test]
async fn test_fjall_window_tiling() {
    let (base_url, _tmp) = start_test_server_with_engine("fjall").await;
    let client = Client::new();
    setup_window_collection(&base_url, &client).await;

    // Window vs residual-forced page equivalence (the M2 proof on fjall).
    for (offset, limit) in [(0u64, 3u64), (2, 5), (9, 5)] {
        let fast: Value = client
            .post(format!("{base_url}/events/query"))
            .json(&json!({
                "filter": {"event_type": "firewall"},
                "limit": limit, "offset": offset
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let slow: Value = client
            .post(format!("{base_url}/events/query"))
            .json(&json!({
                "filter": {"event_type": "firewall", "_id": {"$exists": true}},
                "limit": limit, "offset": offset
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            fast["data"], slow["data"],
            "page mismatch at offset {offset} limit {limit}"
        );
        assert_eq!(fast["meta"]["total_count"], 10);
        let page_len = fast["data"].as_array().unwrap().len() as u64;
        assert_eq!(
            fast["meta"]["docs_scanned"].as_u64().unwrap(),
            page_len,
            "window must load only the page"
        );
        // Residual side streams: hydrates to the probe row, total exact
        // only on exhaustion (same pins as the rocksdb twin).
        assert_eq!(
            slow["meta"]["docs_scanned"].as_u64().unwrap(),
            (offset + limit + 1).min(10)
        );
        if offset + limit < 10 {
            assert!(slow["meta"]["total_count"].is_null());
        } else {
            assert_eq!(slow["meta"]["total_count"], 10);
        }
    }

    // Growing-offset tiling == one-shot, for the index window and the
    // or_union window (both ride load_id_window).
    for filter in [
        json!({"event_type": "firewall"}),
        json!({"$or": [{"event_type": "firewall"}, {"event_type": "dns"}]}),
    ] {
        let one_shot: Value = client
            .post(format!("{base_url}/events/query"))
            .json(&json!({"filter": filter, "limit": 100}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let expected = seqs(&one_shot);

        let mut tiled: Vec<i64> = Vec::new();
        let mut offset = 0u64;
        loop {
            let page: Value = client
                .post(format!("{base_url}/events/query"))
                .json(&json!({"filter": filter, "limit": 3, "offset": offset}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            tiled.extend(seqs(&page));
            if page["meta"]["has_more"] != json!(true) {
                break;
            }
            offset += 3;
        }
        assert_eq!(tiled, expected, "tiling for {filter}");
    }

    // The $or filter above must actually ride the union on fjall.
    let or_probe: Value = client
        .post(format!("{base_url}/events/query"))
        .json(&json!({
            "filter": {"$or": [{"event_type": "firewall"}, {"event_type": "dns"}]},
            "count_only": true
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(or_probe["meta"]["scan_strategy"], "or_union");
    assert_eq!(or_probe["meta"]["total_count"], 20);
}

// ── H-P2: streamed FullScan pages ────────────────────────────────────────
//
// The full scan no longer materializes the collection. Unsorted filtered
// pages early-exit at the limit+1 probe — total_count is omitted exactly
// when has_more is true and exact when the scan ran out. Unfiltered pages
// skip offset entries without parsing and take total_count from DocCounters.
// Sorted pages keep only the offset+limit+1 smallest rows (top-K) while
// still seeing every match, so their totals stay exact.

async fn seed_full_scan_stream(client: &Client, base_url: &str) {
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "fs_stream"}))
        .send()
        .await
        .unwrap();
    for i in 0..30 {
        let resp = client
            .post(format!("{base_url}/fs_stream/docs"))
            .json(&json!({
                "_id": format!("d{i:02}"),
                "grp": i % 3,
                "s": i % 4,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
    }
}

async fn fs_query(client: &Client, base_url: &str, body: Value) -> Value {
    let resp = client
        .post(format!("{base_url}/fs_stream/query"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

#[tokio::test]
async fn test_full_scan_streamed_filtered_pages() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_full_scan_stream(&client, &base_url).await;

    // grp == 0 matches d00, d03, ..., d27 — 10 of 30 docs. Page 1's probe
    // row (the 4th match, d09) lands at the 10th parsed doc.
    let body = fs_query(
        &client,
        &base_url,
        json!({"filter": {"grp": 0}, "limit": 3}),
    )
    .await;
    assert_eq!(
        ids_of(body["data"].as_array().unwrap()),
        ["d00", "d03", "d06"]
    );
    assert_eq!(body["meta"]["has_more"], true);
    assert!(
        body["meta"]["total_count"].is_null(),
        "early-exited page must omit total_count"
    );
    assert_eq!(body["meta"]["docs_scanned"], 10);

    // Mid page: skips 3 matches, keeps 3 + probe — parsed through d18.
    let body = fs_query(
        &client,
        &base_url,
        json!({"filter": {"grp": 0}, "limit": 3, "offset": 3}),
    )
    .await;
    assert_eq!(
        ids_of(body["data"].as_array().unwrap()),
        ["d09", "d12", "d15"]
    );
    assert_eq!(body["meta"]["has_more"], true);
    assert!(body["meta"]["total_count"].is_null());
    assert_eq!(body["meta"]["docs_scanned"], 19);

    // Final page: the scan runs out, so the exact total returns.
    let body = fs_query(
        &client,
        &base_url,
        json!({"filter": {"grp": 0}, "limit": 3, "offset": 9}),
    )
    .await;
    assert_eq!(ids_of(body["data"].as_array().unwrap()), ["d27"]);
    assert_ne!(
        body["meta"]["has_more"], true,
        "final page must not report has_more"
    );
    assert_eq!(body["meta"]["total_count"], 10);
    assert_eq!(body["meta"]["docs_scanned"], 30);

    // count_only is untouched: exact count, whole collection evaluated.
    let body = fs_query(
        &client,
        &base_url,
        json!({"filter": {"grp": 0}, "count_only": true}),
    )
    .await;
    assert_eq!(body["meta"]["total_count"], 10);
    assert_eq!(body["meta"]["docs_scanned"], 30);
    assert_eq!(body["meta"]["scan_strategy"], "full_scan");

    // Offset tiling with a constant filter covers every match exactly once.
    let expected = reference_ids(
        &client,
        &base_url,
        "fs_stream",
        json!({"filter": {"grp": 0}}),
    )
    .await;
    let mut tiled = Vec::new();
    for offset in (0..12).step_by(3) {
        let body = fs_query(
            &client,
            &base_url,
            json!({"filter": {"grp": 0}, "limit": 3, "offset": offset}),
        )
        .await;
        tiled.extend(ids_of(body["data"].as_array().unwrap()));
    }
    assert_eq!(tiled, expected);
}

#[tokio::test]
async fn test_full_scan_unfiltered_offset_page_skips_unparsed() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_full_scan_stream(&client, &base_url).await;

    // Mid-collection window: offset entries skipped without parsing —
    // docs_scanned counts only page + probe; the total comes from DocCounters.
    let body = fs_query(&client, &base_url, json!({"limit": 5, "offset": 10})).await;
    assert_eq!(
        ids_of(body["data"].as_array().unwrap()),
        ["d10", "d11", "d12", "d13", "d14"]
    );
    assert_eq!(body["meta"]["total_count"], 30);
    assert_eq!(body["meta"]["docs_scanned"], 6);
    assert_eq!(body["meta"]["has_more"], true);

    // Final window: exhaustion, no probe row beyond the end.
    let body = fs_query(&client, &base_url, json!({"limit": 5, "offset": 25})).await;
    assert_eq!(
        ids_of(body["data"].as_array().unwrap()),
        ["d25", "d26", "d27", "d28", "d29"]
    );
    assert_eq!(body["meta"]["total_count"], 30);
    assert_eq!(body["meta"]["docs_scanned"], 5);
    assert_ne!(
        body["meta"]["has_more"], true,
        "final page must not report has_more"
    );
}

#[tokio::test]
async fn test_full_scan_sorted_topk_pages_match_reference() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_full_scan_stream(&client, &base_url).await;

    // s == i % 4 gives four big tie runs; ties order by _id in the last
    // sort field's direction (asc here).
    let body = fs_query(
        &client,
        &base_url,
        json!({"sort": [{"s": "asc"}], "limit": 7}),
    )
    .await;
    assert_eq!(
        ids_of(body["data"].as_array().unwrap()),
        ["d00", "d04", "d08", "d12", "d16", "d20", "d24"]
    );
    assert_eq!(body["meta"]["total_count"], 30, "top-K keeps exact totals");
    assert_eq!(body["meta"]["has_more"], true);
    assert!(
        body["meta"]["next_cursor"].is_string(),
        "sorted pages still emit cursors"
    );

    // Descending flips the tiebreak with the direction.
    let body = fs_query(
        &client,
        &base_url,
        json!({"sort": [{"s": "desc"}], "limit": 3}),
    )
    .await;
    assert_eq!(
        ids_of(body["data"].as_array().unwrap()),
        ["d27", "d23", "d19"]
    );

    // Offset tiling over the sorted order equals the one-shot.
    let expected = reference_ids(
        &client,
        &base_url,
        "fs_stream",
        json!({"sort": [{"s": "asc"}]}),
    )
    .await;
    let mut tiled = Vec::new();
    for offset in (0..35).step_by(7) {
        let body = fs_query(
            &client,
            &base_url,
            json!({"sort": [{"s": "asc"}], "limit": 7, "offset": offset}),
        )
        .await;
        tiled.extend(ids_of(body["data"].as_array().unwrap()));
    }
    assert_eq!(tiled, expected);

    // Filtered + sorted: every match is still seen, totals stay exact.
    let body = fs_query(
        &client,
        &base_url,
        json!({"filter": {"grp": 0}, "sort": [{"s": "desc"}], "limit": 4}),
    )
    .await;
    assert_eq!(body["meta"]["total_count"], 10);
    assert_eq!(body["meta"]["docs_scanned"], 30);
    assert_eq!(body["meta"]["has_more"], true);
}

#[tokio::test]
async fn test_fjall_full_scan_streamed_pages() {
    let (base_url, _tmp) = start_test_server_with_engine("fjall").await;
    let client = Client::new();
    seed_full_scan_stream(&client, &base_url).await;

    let body = fs_query(
        &client,
        &base_url,
        json!({"filter": {"grp": 0}, "limit": 3}),
    )
    .await;
    assert_eq!(
        ids_of(body["data"].as_array().unwrap()),
        ["d00", "d03", "d06"]
    );
    assert!(body["meta"]["total_count"].is_null());
    assert_eq!(body["meta"]["docs_scanned"], 10);

    let body = fs_query(
        &client,
        &base_url,
        json!({"filter": {"grp": 0}, "limit": 3, "offset": 9}),
    )
    .await;
    assert_eq!(ids_of(body["data"].as_array().unwrap()), ["d27"]);
    assert_eq!(body["meta"]["total_count"], 10);

    let expected = reference_ids(
        &client,
        &base_url,
        "fs_stream",
        json!({"filter": {"grp": 0}}),
    )
    .await;
    let mut tiled = Vec::new();
    for offset in (0..12).step_by(3) {
        let body = fs_query(
            &client,
            &base_url,
            json!({"filter": {"grp": 0}, "limit": 3, "offset": offset}),
        )
        .await;
        tiled.extend(ids_of(body["data"].as_array().unwrap()));
    }
    assert_eq!(tiled, expected);

    // Sorted top-K on fjall.
    let body = fs_query(
        &client,
        &base_url,
        json!({"sort": [{"s": "asc"}], "limit": 7}),
    )
    .await;
    assert_eq!(
        ids_of(body["data"].as_array().unwrap()),
        ["d00", "d04", "d08", "d12", "d16", "d20", "d24"]
    );
    assert_eq!(body["meta"]["total_count"], 30);
}

/// Mutation match phases stream (matches-only resident) while the write
/// stays one atomic batch: after delete_by_query, the docs AND their index
/// entries are gone together — an indexed count over the deleted value
/// finds zero entries without scanning.
#[tokio::test]
async fn test_delete_by_query_survivors_and_index_atomicity() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();

    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "dbq_stream"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/dbq_stream/indexes"))
        .json(&json!({"name": "idx_kind", "fields": ["kind"]}))
        .send()
        .await
        .unwrap();
    let docs: Vec<Value> = (0..30)
        .map(|i| json!({"kind": if i < 20 { "a" } else { "b" }, "n": i}))
        .collect();
    client
        .post(format!("{base_url}/dbq_stream/docs/_bulk"))
        .json(&json!({"documents": docs}))
        .send()
        .await
        .unwrap();

    // Match-heavy delete: 20 of 30 docs match.
    let resp: Value = client
        .post(format!("{base_url}/dbq_stream/docs/_delete_by_query"))
        .json(&json!({"filter": {"kind": "a"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["data"]["deleted"], 20);

    // The index agrees instantly: zero entries for the deleted value,
    // counted keys-only (docs_scanned 0) — doc and index removal committed
    // as one batch.
    let count: Value = client
        .post(format!("{base_url}/dbq_stream/query"))
        .json(&json!({"filter": {"kind": "a"}, "count_only": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(count["meta"]["total_count"], 0);
    assert_eq!(count["meta"]["scan_strategy"], "index_eq");
    assert_eq!(count["meta"]["docs_scanned"], 0);

    // Survivors intact.
    let survivors: Value = client
        .post(format!("{base_url}/dbq_stream/query"))
        .json(&json!({"filter": {"kind": "b"}, "count_only": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(survivors["meta"]["total_count"], 10);

    // Match-light delete: nothing matches, nothing changes.
    let resp: Value = client
        .post(format!("{base_url}/dbq_stream/docs/_delete_by_query"))
        .json(&json!({"filter": {"kind": "zzz"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["data"]["deleted"], 0);

    // Delete the rest; the unfiltered O(1) count sees an empty collection.
    let resp: Value = client
        .post(format!("{base_url}/dbq_stream/docs/_delete_by_query"))
        .json(&json!({"filter": {"kind": "b"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["data"]["deleted"], 10);
    let count: Value = client
        .post(format!("{base_url}/dbq_stream/query"))
        .json(&json!({"count_only": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(count["meta"]["total_count"], 0);
    assert_eq!(count["meta"]["scan_strategy"], "doc_counter");
}

/// Existence-cache + partition-cache lifecycle (H-P1.3): create → visible,
/// drop → immediately 404 on reads AND writes (the cache entry leaves before
/// the drop commits), recreate → fresh counter over the reused partition.
#[tokio::test]
async fn test_collection_drop_recreate_cycle_both_engines() {
    for engine in ["rocksdb", "fjall"] {
        let (base_url, _tmp) = start_test_server_with_engine(engine).await;
        let client = Client::new();

        client
            .post(format!("{base_url}/_collections"))
            .json(&json!({"name": "cyc"}))
            .send()
            .await
            .unwrap();
        let resp = client
            .post(format!("{base_url}/cyc/docs"))
            .json(&json!({"v": 1}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "{engine}: insert into fresh collection");

        let resp = client
            .delete(format!("{base_url}/cyc"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{engine}: drop");

        let resp = client
            .post(format!("{base_url}/cyc/query"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{engine}: query after drop");
        let resp = client
            .post(format!("{base_url}/cyc/docs"))
            .json(&json!({"v": 2}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{engine}: insert after drop");

        // Recreate: counter starts fresh, reads and writes work again.
        client
            .post(format!("{base_url}/_collections"))
            .json(&json!({"name": "cyc"}))
            .send()
            .await
            .unwrap();
        let count: Value = client
            .post(format!("{base_url}/cyc/query"))
            .json(&json!({"count_only": true}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(count["meta"]["total_count"], 0, "{engine}: fresh counter");

        let resp = client
            .post(format!("{base_url}/cyc/docs"))
            .json(&json!({"v": 3}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "{engine}: insert after recreate");
        let count: Value = client
            .post(format!("{base_url}/cyc/query"))
            .json(&json!({"count_only": true}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            count["meta"]["total_count"], 1,
            "{engine}: count after recreate"
        );
    }
}

// ── Queue #5: test-debt sweep (DT-2/3/4/5/10/12/17/18) ──────────────────

/// DT-5: custom `_id`s mixed with server UUIDs inside cursor walks — sorted
/// (dup values force `_id` tiebreaks across id "styles") and no-sort walks
/// both concatenate to the one-shot reference.
#[tokio::test]
async fn test_cursor_walks_with_mixed_custom_ids() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "walk_ids"}))
        .send()
        .await
        .unwrap();
    for i in 0..12 {
        let mut doc = json!({"s": i % 3, "n": i});
        if i % 2 == 0 {
            doc["_id"] = json!(format!("cust-{i:02}"));
        }
        let resp = client
            .post(format!("{base_url}/walk_ids/docs"))
            .json(&doc)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
    }

    let reference = reference_ids(
        &client,
        &base_url,
        "walk_ids",
        json!({"sort": [{"s": "asc"}]}),
    )
    .await;
    let walked = cursor_walk(
        &client,
        &base_url,
        "walk_ids",
        json!({"sort": [{"s": "asc"}], "limit": 4}),
    )
    .await;
    assert_eq!(ids_of(&walked), reference, "sorted walk with mixed ids");

    let reference = reference_ids(&client, &base_url, "walk_ids", json!({})).await;
    let walked = cursor_walk(&client, &base_url, "walk_ids", json!({"limit": 5})).await;
    assert_eq!(ids_of(&walked), reference, "no-sort id walk with mixed ids");
}

/// DT-12: projection that strips the sort field, riding the index_sorted
/// SEEK path across cursor resumes — the cursor is built pre-projection, so
/// every page must stay on the fast path AND return only projected fields.
#[tokio::test]
async fn test_index_sorted_cursor_walk_with_projection() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    seed_index_sorted_collection(&client, &base_url).await;

    let unprojected = reference_ids(
        &client,
        &base_url,
        "events",
        json!({"filter": {"event_type": "fw"}, "sort": [{"received_at": "asc"}]}),
    )
    .await;

    let mut collected: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..20 {
        let mut req = json!({
            "filter": {"event_type": "fw"},
            "sort": [{"received_at": "asc"}],
            "limit": 4,
            "fields": ["event_type"]
        });
        if let Some(c) = &cursor {
            req["cursor"] = json!(c);
        }
        let body: Value = client
            .post(format!("{base_url}/events/query"))
            .json(&req)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            body["meta"]["scan_strategy"], "index_sorted",
            "every page (incl. resumes) stays on the seek path"
        );
        for doc in body["data"].as_array().unwrap() {
            let keys: Vec<&str> = doc
                .as_object()
                .unwrap()
                .keys()
                .map(|k| k.as_str())
                .collect();
            assert_eq!(
                keys.len(),
                2,
                "projected doc has only _id + event_type: {doc}"
            );
            assert!(doc.get("received_at").is_none(), "sort field stripped");
            collected.push(doc["_id"].as_str().unwrap().to_string());
        }
        match body["meta"]["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(
        collected, unprojected,
        "projection must not disturb the walk"
    );
}

/// DT-2: inserts landing mid-walk. Strictly-after semantics: a doc inserted
/// BEHIND the cursor position never appears; one inserted AHEAD appears
/// exactly once.
#[tokio::test]
async fn test_cursor_walk_insert_mid_pagination() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "ins_walk"}))
        .send()
        .await
        .unwrap();
    for i in 1..=10 {
        client
            .post(format!("{base_url}/ins_walk/docs"))
            .json(&json!({"_id": format!("d{i:02}"), "s": i * 10}))
            .send()
            .await
            .unwrap();
    }

    let page1: Value = client
        .post(format!("{base_url}/ins_walk/query"))
        .json(&json!({"sort": [{"s": "asc"}], "limit": 3}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut seen: Vec<i64> = page1["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["s"].as_i64().unwrap())
        .collect();
    assert_eq!(seen, [10, 20, 30]);
    let mut cursor = page1["meta"]["next_cursor"].as_str().unwrap().to_string();

    // Insert behind the cursor (s=15) and ahead of it (s=55).
    for (id, s) in [("x-behind", 15), ("x-ahead", 55)] {
        client
            .post(format!("{base_url}/ins_walk/docs"))
            .json(&json!({"_id": id, "s": s}))
            .send()
            .await
            .unwrap();
    }

    let mut remaining_ids: Vec<String> = Vec::new();
    for _ in 0..10 {
        let body: Value = client
            .post(format!("{base_url}/ins_walk/query"))
            .json(&json!({"sort": [{"s": "asc"}], "limit": 3, "cursor": cursor}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        for doc in body["data"].as_array().unwrap() {
            seen.push(doc["s"].as_i64().unwrap());
            remaining_ids.push(doc["_id"].as_str().unwrap().to_string());
        }
        match body["meta"]["next_cursor"].as_str() {
            Some(c) => cursor = c.to_string(),
            None => break,
        }
    }
    assert_eq!(
        seen,
        [10, 20, 30, 40, 50, 55, 60, 70, 80, 90, 100],
        "behind-insert invisible, ahead-insert exactly once"
    );
    assert!(!remaining_ids.contains(&"x-behind".to_string()));
    assert_eq!(
        remaining_ids.iter().filter(|id| *id == "x-ahead").count(),
        1
    );
}

/// DT-3: updates moving docs across the cursor boundary. Documented
/// semantics (API.md): moved-behind may be skipped, moved-ahead may
/// re-surface — but never duplicated within the remaining walk.
#[tokio::test]
async fn test_cursor_walk_update_across_boundary() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "upd_walk"}))
        .send()
        .await
        .unwrap();
    for i in 1..=10 {
        client
            .post(format!("{base_url}/upd_walk/docs"))
            .json(&json!({"_id": format!("d{i:02}"), "s": i * 10}))
            .send()
            .await
            .unwrap();
    }

    let page1: Value = client
        .post(format!("{base_url}/upd_walk/query"))
        .json(&json!({"sort": [{"s": "asc"}], "limit": 3}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        ids_of(page1["data"].as_array().unwrap()),
        ["d01", "d02", "d03"]
    );
    let mut cursor = page1["meta"]["next_cursor"].as_str().unwrap().to_string();

    // d05 (s=50, ahead) moves BEHIND the cursor; d02 (s=20, already
    // returned) moves AHEAD to s=75.
    client
        .patch(format!("{base_url}/upd_walk/docs/d05"))
        .json(&json!({"s": 5}))
        .send()
        .await
        .unwrap();
    client
        .patch(format!("{base_url}/upd_walk/docs/d02"))
        .json(&json!({"s": 75}))
        .send()
        .await
        .unwrap();

    let mut remaining: Vec<String> = Vec::new();
    for _ in 0..10 {
        let body: Value = client
            .post(format!("{base_url}/upd_walk/query"))
            .json(&json!({"sort": [{"s": "asc"}], "limit": 3, "cursor": cursor}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        remaining.extend(ids_of(body["data"].as_array().unwrap()));
        match body["meta"]["next_cursor"].as_str() {
            Some(c) => cursor = c.to_string(),
            None => break,
        }
    }
    assert_eq!(
        remaining,
        ["d04", "d06", "d07", "d02", "d08", "d09", "d10"],
        "strictly-after order over the NEW values"
    );
    assert!(
        !remaining.contains(&"d05".to_string()),
        "moved-behind doc is skipped (documented)"
    );
    assert_eq!(
        remaining.iter().filter(|id| *id == "d02").count(),
        1,
        "moved-ahead doc re-surfaces exactly once (documented)"
    );
}

/// DT-4: a cursor carrying a Missing sort value must NOT route to the index
/// seek even when a covering compound index exists (created mid-walk here —
/// the only way such a cursor can meet such an index, since index_sorted
/// pages never contain missing-field docs).
#[tokio::test]
async fn test_missing_sort_cursor_avoids_index_seek() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "miss_walk"}))
        .send()
        .await
        .unwrap();
    // d01 lacks `score` entirely; Missing sorts before every present value.
    client
        .post(format!("{base_url}/miss_walk/docs"))
        .json(&json!({"_id": "d01", "event_type": "x"}))
        .send()
        .await
        .unwrap();
    for i in 2..=5 {
        client
            .post(format!("{base_url}/miss_walk/docs"))
            .json(&json!({"_id": format!("d{i:02}"), "event_type": "x", "score": i}))
            .send()
            .await
            .unwrap();
    }

    let page1: Value = client
        .post(format!("{base_url}/miss_walk/query"))
        .json(&json!({"filter": {"event_type": "x"}, "sort": [{"score": "asc"}], "limit": 1}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ids_of(page1["data"].as_array().unwrap()), ["d01"]);
    let mut cursor = page1["meta"]["next_cursor"]
        .as_str()
        .expect("boundary doc with Missing sort value still yields a cursor")
        .to_string();

    // NOW create a compound index covering filter + sort: the planner must
    // still refuse the seek for this cursor (its position isn't an index key).
    client
        .post(format!("{base_url}/miss_walk/indexes"))
        .json(&json!({"name": "idx_type_score", "fields": ["event_type", "score"]}))
        .send()
        .await
        .unwrap();

    let mut walked = vec!["d01".to_string()];
    let mut missing_cursor_resume = true;
    for _ in 0..10 {
        let body: Value = client
            .post(format!("{base_url}/miss_walk/query"))
            .json(&json!({
                "filter": {"event_type": "x"},
                "sort": [{"score": "asc"}],
                "limit": 1,
                "cursor": cursor
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if missing_cursor_resume {
            // Only THIS resume carries the Missing value; once the boundary
            // doc has the field, later cursors may legitimately re-enter the
            // index_sorted seek (and do — pinned below).
            assert_ne!(
                body["meta"]["scan_strategy"], "index_sorted",
                "Missing-valued cursor must fall back off the seek path"
            );
            missing_cursor_resume = false;
        } else {
            assert_eq!(
                body["meta"]["scan_strategy"], "index_sorted",
                "Present-valued cursors re-enter the seek path"
            );
        }
        walked.extend(ids_of(body["data"].as_array().unwrap()));
        match body["meta"]["next_cursor"].as_str() {
            Some(c) => cursor = c.to_string(),
            None => break,
        }
    }
    assert_eq!(
        walked,
        ["d01", "d02", "d03", "d04", "d05"],
        "no loss, no dups"
    );
}

/// DT-17 remainder: concurrent bulk inserts racing delete_by_query must
/// leave DocCounters exactly agreeing with a full evaluation.
#[tokio::test]
async fn test_doc_counter_concurrent_bulk_and_delete_by_query() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "ctr_race"}))
        .send()
        .await
        .unwrap();

    let mut handles = Vec::new();
    for t in 0..3u64 {
        let client = client.clone();
        let base = base_url.clone();
        handles.push(tokio::spawn(async move {
            for round in 0..5 {
                let docs: Vec<Value> = (0..40)
                    .map(|i| json!({"grp": t, "round": round, "i": i}))
                    .collect();
                client
                    .post(format!("{base}/ctr_race/docs/_bulk"))
                    .json(&json!({"documents": docs}))
                    .send()
                    .await
                    .unwrap();
            }
        }));
    }
    for t in 0..2u64 {
        let client = client.clone();
        let base = base_url.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..4 {
                client
                    .post(format!("{base}/ctr_race/docs/_delete_by_query"))
                    .json(&json!({"filter": {"grp": t}}))
                    .send()
                    .await
                    .unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let counter: Value = client
        .post(format!("{base_url}/ctr_race/query"))
        .json(&json!({"count_only": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(counter["meta"]["scan_strategy"], "doc_counter");
    let full: Value = client
        .post(format!("{base_url}/ctr_race/query"))
        .json(&json!({"filter": {"_id": {"$exists": true}}, "count_only": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(full["meta"]["scan_strategy"], "full_scan");
    assert_eq!(
        counter["meta"]["total_count"], full["meta"]["total_count"],
        "authoritative counter must equal full evaluation after racing mutations"
    );
}

/// DT-18: the DEFAULT 64 MiB body ceiling itself (only the 1 MiB-cap server
/// was tested): a ~68 MiB request is rejected with 413.
#[tokio::test]
async fn test_default_body_limit_ceiling() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "big"}))
        .send()
        .await
        .unwrap();

    let pad = "a".repeat(68 * 1024 * 1024);
    let resp = client
        .post(format!("{base_url}/big/docs"))
        .json(&json!({"pad": pad}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413, "default 64 MiB ceiling enforced");
}

/// DT-10: seeded randomized walk equivalence. Random mixed-type values
/// (with missing fields and heavy duplicates), random page sizes, several
/// sort specs — cursor walks AND offset tiling must both concatenate to the
/// one-shot reference. Fixed seeds keep CI deterministic.
#[tokio::test]
async fn test_randomized_walk_and_tiling_equivalence() {
    use rand::{Rng, SeedableRng, rngs::StdRng};

    for (engine, seeds) in [("rocksdb", vec![7u64, 42, 1337]), ("fjall", vec![42u64])] {
        for seed in seeds {
            let (base_url, _tmp) = start_test_server_with_engine(engine).await;
            let client = Client::new();
            let mut rng = StdRng::seed_from_u64(seed);
            client
                .post(format!("{base_url}/_collections"))
                .json(&json!({"name": "rand_walk"}))
                .send()
                .await
                .unwrap();

            let strings = ["alpha", "beta", "", "zz"];
            for i in 0..80 {
                let mut doc = json!({"_id": format!("d{i:03}"), "w": rng.gen_range(0..4)});
                match rng.gen_range(0..8) {
                    0 => doc["v"] = json!(null),
                    1 => doc["v"] = json!(rng.gen_bool(0.5)),
                    2 | 3 => doc["v"] = json!(rng.gen_range(0..5)),
                    4 => doc["v"] = json!(rng.gen_range(-3.0..3.0)),
                    5 | 6 => doc["v"] = json!(strings[rng.gen_range(0..strings.len())]),
                    _ => {} // v missing entirely
                }
                client
                    .post(format!("{base_url}/rand_walk/docs"))
                    .json(&doc)
                    .send()
                    .await
                    .unwrap();
            }

            let specs: Vec<Option<Value>> = vec![
                Some(json!([{"v": "asc"}])),
                Some(json!([{"v": "desc"}])),
                Some(json!([{"w": "asc"}, {"v": "desc"}])),
                None, // no-sort _id walk
            ];
            for spec in &specs {
                let mut base_body = json!({});
                if let Some(s) = spec {
                    base_body["sort"] = s.clone();
                }
                let reference =
                    reference_ids(&client, &base_url, "rand_walk", base_body.clone()).await;
                assert_eq!(reference.len(), 80);

                let limit = rng.gen_range(1..=7);
                let mut walk_body = base_body.clone();
                walk_body["limit"] = json!(limit);
                let walked = cursor_walk(&client, &base_url, "rand_walk", walk_body).await;
                assert_eq!(
                    ids_of(&walked),
                    reference,
                    "cursor walk: engine {engine} seed {seed} spec {spec:?} limit {limit}"
                );

                let mut tiled: Vec<String> = Vec::new();
                let mut offset = 0u64;
                loop {
                    let mut page_body = base_body.clone();
                    page_body["limit"] = json!(limit);
                    page_body["offset"] = json!(offset);
                    let page: Value = client
                        .post(format!("{base_url}/rand_walk/query"))
                        .json(&page_body)
                        .send()
                        .await
                        .unwrap()
                        .json()
                        .await
                        .unwrap();
                    let ids = ids_of(page["data"].as_array().unwrap());
                    let n = ids.len();
                    tiled.extend(ids);
                    if n < limit as usize {
                        break;
                    }
                    offset += limit;
                }
                assert_eq!(
                    tiled, reference,
                    "offset tiling: engine {engine} seed {seed} spec {spec:?} limit {limit}"
                );
            }
        }
    }
}

/// DT-22 (bundled with S2-1..5): deletes punch holes in the bitmap position
/// map — the windowed bitmap path and the cursor walk must skip holes
/// without consuming the window: pages contain exactly the survivors, and
/// tiling still equals the one-shot.
#[tokio::test]
async fn test_bitmap_window_walk_with_holes() {
    let (base_url, _tmp) = start_test_server_with_bitmap("kind").await;
    let client = Client::new();
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "holey"}))
        .send()
        .await
        .unwrap();
    for i in 0..20 {
        client
            .post(format!("{base_url}/holey/docs"))
            .json(&json!({"_id": format!("d{i:02}"), "kind": if i % 2 == 0 { "a" } else { "b" }, "i": i}))
            .send()
            .await
            .unwrap();
    }

    // The bitmap path must actually serve this filter.
    let probe: Value = client
        .post(format!("{base_url}/holey/query"))
        .json(&json!({"filter": {"kind": "a"}, "count_only": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["meta"]["scan_strategy"], "bitmap");
    assert_eq!(probe["meta"]["total_count"], 10);

    // Punch holes: delete 4 of the kind=a docs (positions stay allocated,
    // slots go None) and 2 of kind=b for good measure.
    for id in ["d00", "d08", "d12", "d16", "d03", "d11"] {
        let resp = client
            .delete(format!("{base_url}/holey/docs/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "delete {id}");
    }

    // One-shot over the survivors, in insertion-position order.
    let one_shot: Value = client
        .post(format!("{base_url}/holey/query"))
        .json(&json!({"filter": {"kind": "a"}, "limit": 100}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let survivors = ids_of(one_shot["data"].as_array().unwrap());
    assert_eq!(survivors, ["d02", "d04", "d06", "d10", "d14", "d18"]);
    assert_eq!(one_shot["meta"]["total_count"], 6);

    // Windowed tiling across the holes equals the one-shot — resolve_window
    // must skip hole positions without consuming the window.
    let mut tiled: Vec<String> = Vec::new();
    for offset in (0..9).step_by(3) {
        let page: Value = client
            .post(format!("{base_url}/holey/query"))
            .json(&json!({"filter": {"kind": "a"}, "limit": 3, "offset": offset}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(page["meta"]["scan_strategy"], "bitmap");
        tiled.extend(ids_of(page["data"].as_array().unwrap()));
    }
    assert_eq!(tiled, survivors, "tiling over holes == one-shot");

    // Cursor walk over the bitmap path (deterministic _id re-sort) with
    // MORE holes punched mid-walk: already-returned deletions must not
    // disturb later pages; a deleted-ahead doc must not appear.
    let page1: Value = client
        .post(format!("{base_url}/holey/query"))
        .json(&json!({"filter": {"kind": "a"}, "sort": [{"_id": "asc"}], "limit": 2}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ids_of(page1["data"].as_array().unwrap()), ["d02", "d04"]);
    let mut cursor = page1["meta"]["next_cursor"].as_str().unwrap().to_string();
    // Delete one already-returned (d02) and one ahead (d14).
    for id in ["d02", "d14"] {
        client
            .delete(format!("{base_url}/holey/docs/{id}"))
            .send()
            .await
            .unwrap();
    }
    let mut walked: Vec<String> = vec!["d02".into(), "d04".into()];
    for _ in 0..10 {
        let body: Value = client
            .post(format!("{base_url}/holey/query"))
            .json(&json!({"filter": {"kind": "a"}, "sort": [{"_id": "asc"}], "limit": 2, "cursor": cursor}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        walked.extend(ids_of(body["data"].as_array().unwrap()));
        match body["meta"]["next_cursor"].as_str() {
            Some(c) => cursor = c.to_string(),
            None => break,
        }
    }
    assert_eq!(
        walked,
        ["d02", "d04", "d06", "d10", "d18"],
        "holes skipped; deleted-ahead doc absent; no dups"
    );
}

// ── F1: multi-collection bitmap scoping ─────────────────────────────────────
// The accelerator's position space and value bitmaps are GLOBAL across
// collections; every answer must be scoped to the queried collection's
// membership. Pre-fix, fully-covered count_only returned global counts
// (more docs than the collection held), bare pages ghosted past the
// collection's real matches (0-doc pages with has_more=true), bitmap
// aggregates counted every collection, and drop_collection wiped ALL
// acceleration until restart. Found live by the pre-merge SIEM rig
// (PRE-MERGE-LIVE-TESTING.md § F1); every bitmap test above runs a single
// collection, which is exactly why CI never saw it.

/// events_a: 6 red + 4 blue; events_b: 3 red + 5 green. "red" spans both
/// (global 9 — the pre-fix wrong answer), "blue"/"green" are
/// single-collection (the aggregate-omission check).
async fn setup_two_collection_bitmap_data(base_url: &str, client: &Client) {
    for name in ["events_a", "events_b"] {
        client
            .post(format!("{base_url}/_collections"))
            .json(&json!({"name": name}))
            .send()
            .await
            .unwrap();
    }
    let a_docs: Vec<Value> = (0..10)
        .map(|i| json!({"category": if i < 6 { "red" } else { "blue" }, "n": i}))
        .collect();
    let b_docs: Vec<Value> = (0..8)
        .map(|i| json!({"category": if i < 3 { "red" } else { "green" }, "n": i}))
        .collect();
    for (coll, docs) in [("events_a", a_docs), ("events_b", b_docs)] {
        client
            .post(format!("{base_url}/{coll}/docs/_bulk"))
            .json(&json!({"documents": docs}))
            .send()
            .await
            .unwrap();
    }
}

async fn count_only_with_strategy(
    client: &Client,
    base_url: &str,
    coll: &str,
    filter: Value,
) -> (u64, String) {
    let body: Value = client
        .post(format!("{base_url}/{coll}/query"))
        .json(&json!({"filter": filter, "count_only": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["ok"], true, "count_only failed: {body}");
    (
        body["meta"]["total_count"].as_u64().unwrap(),
        body["meta"]["scan_strategy"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    )
}

/// F1a: fully-covered bitmap counts are per-collection, and membership
/// follows delete_by_query (the SIEM's rollup-churn shape).
#[tokio::test]
async fn test_bitmap_multi_collection_scoped_counts() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();
    setup_two_collection_bitmap_data(&base_url, &client).await;

    let (a_red, strat) =
        count_only_with_strategy(&client, &base_url, "events_a", json!({"category": "red"})).await;
    assert_eq!(strat, "bitmap");
    assert_eq!(a_red, 6, "events_a red — not the global 9");
    let (b_red, _) =
        count_only_with_strategy(&client, &base_url, "events_b", json!({"category": "red"})).await;
    assert_eq!(b_red, 3);

    // Ground-truth twin (the live sweep's method): $exists forces doc
    // evaluation, which is inherently collection-scoped.
    let (twin, _) = count_only_with_strategy(
        &client,
        &base_url,
        "events_a",
        json!({"$and": [{"category": "red"}, {"_id": {"$exists": true}}]}),
    )
    .await;
    assert_eq!(twin, a_red);

    // Membership maintenance through delete_by_query.
    client
        .post(format!("{base_url}/events_b/docs/_delete_by_query"))
        .json(&json!({"filter": {"category": "red"}}))
        .send()
        .await
        .unwrap();
    let (b_after, _) =
        count_only_with_strategy(&client, &base_url, "events_b", json!({"category": "red"})).await;
    assert_eq!(b_after, 0);
    let (a_after, _) =
        count_only_with_strategy(&client, &base_url, "events_a", json!({"category": "red"})).await;
    assert_eq!(a_after, 6, "events_a unaffected by events_b churn");
}

/// F1b: bare-page windows report the collection's total and never ghost —
/// pre-fix, offsets past the collection's matches (but inside the global
/// count) returned 0-doc pages with has_more=true.
#[tokio::test]
async fn test_bitmap_bare_pages_have_no_ghost_tail() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();
    setup_two_collection_bitmap_data(&base_url, &client).await;

    let body: Value = client
        .post(format!("{base_url}/events_a/query"))
        .json(&json!({"filter": {"category": "red"}, "limit": 4, "offset": 4}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["meta"]["scan_strategy"], "bitmap");
    let docs = body["data"].as_array().unwrap();
    assert_eq!(docs.len(), 2, "6 red docs, offset 4 → final 2");
    for doc in docs {
        assert_eq!(doc["category"], "red");
    }
    assert_eq!(body["meta"]["total_count"], 6, "not the global 9");
    // has_more is skip-serialized when false — absent is the pass state.
    assert_ne!(body["meta"]["has_more"], true);

    // The pre-fix ghost zone: offset in (collection matches, global matches].
    let body: Value = client
        .post(format!("{base_url}/events_a/query"))
        .json(&json!({"filter": {"category": "red"}, "limit": 4, "offset": 7}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 0);
    assert_ne!(
        body["meta"]["has_more"], true,
        "no ghost pages past the collection's matches"
    );
}

/// F1c: bitmap_aggregate group counts are per-collection; values with no
/// documents in the queried collection are absent from the result.
#[tokio::test]
async fn test_bitmap_aggregate_scoped_groups() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();
    setup_two_collection_bitmap_data(&base_url, &client).await;

    let body: Value = client
        .post(format!("{base_url}/events_a/aggregate"))
        .json(&json!({"pipeline": [{"$group": {"_id": "category", "n": {"$count": {}}}}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["meta"]["scan_strategy"], "bitmap_aggregate");
    let groups: std::collections::HashMap<String, u64> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["_id"].as_str().unwrap().to_string(),
                g["n"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        groups.len(),
        2,
        "green (events_b-only) must be absent: {groups:?}"
    );
    assert_eq!(groups["red"], 6, "not the global 9");
    assert_eq!(groups["blue"], 4);
}

/// F1d: dropping one collection is surgical — every other collection stays
/// bitmap-accelerated (pre-fix the drop cleared the whole accelerator and
/// nothing re-armed it until restart), new writes keep being tracked, and
/// /_stats reflects the removal.
#[tokio::test]
async fn test_drop_collection_keeps_others_accelerated() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();
    setup_two_collection_bitmap_data(&base_url, &client).await;

    let resp = client
        .delete(format!("{base_url}/events_b"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let (a_red, strat) =
        count_only_with_strategy(&client, &base_url, "events_a", json!({"category": "red"})).await;
    assert_eq!(strat, "bitmap", "acceleration survives an unrelated drop");
    assert_eq!(a_red, 6);

    // Still tracking new writes post-drop.
    client
        .post(format!("{base_url}/events_a/docs"))
        .json(&json!({"category": "red"}))
        .send()
        .await
        .unwrap();
    let (after, strat) =
        count_only_with_strategy(&client, &base_url, "events_a", json!({"category": "red"})).await;
    assert_eq!(strat, "bitmap");
    assert_eq!(after, 7);

    let stats: Value = client
        .get(format!("{base_url}/_stats"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let accel = &stats["data"]["scan_accelerator"];
    assert_eq!(accel["ready"], true);
    let by_coll = accel["positions_by_collection"].as_array().unwrap();
    assert_eq!(by_coll.len(), 1, "only events_a remains: {by_coll:?}");
    assert_eq!(by_coll[0]["collection"], "events_a");
    assert_eq!(by_coll[0]["positions"], 11);
}

/// F1 fjall twin: scoping is engine-independent, pinned cheaply.
#[tokio::test]
async fn test_fjall_bitmap_multi_collection_scoped_counts() {
    let (base_url, _tmp) = start_test_server_with_engine_and_bitmap("fjall", "category").await;
    let client = Client::new();
    setup_two_collection_bitmap_data(&base_url, &client).await;

    let (a_red, strat) =
        count_only_with_strategy(&client, &base_url, "events_a", json!({"category": "red"})).await;
    assert_eq!((a_red, strat.as_str()), (6, "bitmap"));
    let (b_red, _) =
        count_only_with_strategy(&client, &base_url, "events_b", json!({"category": "red"})).await;
    assert_eq!(b_red, 3);
}

/// F2: eq/in/range on a field whose ONLY index is compound must not be
/// served from that index — compound indexes exclude documents missing any
/// component field, so the old leading-field fallback silently dropped them
/// (with several matching compounds, WHICH one served was per-process
/// HashMap order; with exactly one, the undercount is deterministic — this
/// test fails on the pre-fix planner with 2 instead of 3). Also pins the
/// unblocked remedy: a real single-field index is creatable (the fallback
/// used to misdetect it as a duplicate) and takes over, complete.
#[tokio::test]
async fn test_compound_leading_field_not_served_from_partial_index() {
    let (base_url, _tmp) = start_test_server().await;
    let client = Client::new();
    client
        .post(format!("{base_url}/_collections"))
        .json(&json!({"name": "logs"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base_url}/logs/indexes"))
        .json(&json!({"name": "idx_kind_action", "fields": ["kind", "action"]}))
        .send()
        .await
        .unwrap();
    for doc in [
        json!({"kind": "sys", "action": "a"}),
        json!({"kind": "sys", "action": "b"}),
        json!({"kind": "sys"}), // no action — absent from the compound index
        json!({"kind": "net", "action": "a"}),
    ] {
        client
            .post(format!("{base_url}/logs/docs"))
            .json(&doc)
            .send()
            .await
            .unwrap();
    }

    let count = |filter: Value, count_only: bool| {
        let client = client.clone();
        let base_url = base_url.clone();
        async move {
            let body: Value = client
                .post(format!("{base_url}/logs/query"))
                .json(&json!({"filter": filter, "count_only": count_only}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(body["ok"], true, "query failed: {body}");
            (
                body["meta"]["total_count"].as_u64(),
                body["meta"]["scan_strategy"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                body["data"].as_array().map(|d| d.len()).unwrap_or(0),
            )
        }
    };

    // eq count: all 3 sys docs, including the action-less one.
    let (total, strat, _) = count(json!({"kind": "sys"}), true).await;
    assert_eq!(
        total,
        Some(3),
        "must include the doc the compound index skipped"
    );
    assert_ne!(
        strat, "index_eq",
        "no single-field index exists — no index_eq"
    );

    // eq doc query: same 3 docs.
    let (_, strat, n) = count(json!({"kind": "sys"}), false).await;
    assert_eq!(n, 3);
    assert_ne!(strat, "index_eq");

    // $in and bounded range on the leading field: same rule.
    let (total, strat, _) = count(json!({"kind": {"$in": ["sys", "net"]}}), true).await;
    assert_eq!(total, Some(4));
    assert_ne!(strat, "index_in");
    let (total, strat, _) = count(json!({"kind": {"$gte": "sys", "$lte": "sys"}}), true).await;
    assert_eq!(total, Some(3));
    assert_ne!(strat, "index_range");

    // The remedy is unblocked: a single-field index on the same leading
    // field is NOT a duplicate of the compound one...
    let resp = client
        .post(format!("{base_url}/logs/indexes"))
        .json(&json!({"name": "idx_kind", "field": "kind"}))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "single-field index creation must not collide with the compound: {:?}",
        resp.text().await
    );

    // ...and once it exists (complete — every doc has kind), index_eq
    // serves the full answer.
    let (total, strat, _) = count(json!({"kind": "sys"}), true).await;
    assert_eq!((total, strat.as_str()), (Some(3), "index_eq"));
}

/// F3: the COLLECTION_NOT_FOUND contract must not depend on the query plan.
/// The existence gate lived only in execute_full_scan, so bitmap- and
/// index-fast-path-planned queries/aggregates/distincts on a missing (e.g.
/// freshly dropped) collection returned 200-with-empty instead of 404 —
/// observed live right after the rig's rotation drop.
#[tokio::test]
async fn test_missing_collection_404_regardless_of_plan() {
    let (base_url, _tmp) = start_test_server_with_bitmap("category").await;
    let client = Client::new();
    // One real collection so the accelerator has live columns; the queried
    // collection never exists.
    setup_two_collection_bitmap_data(&base_url, &client).await;

    // Bitmap-planned count (pre-fix: 200, count 0, strategy "bitmap").
    let resp = client
        .post(format!("{base_url}/never_created/query"))
        .json(&json!({"filter": {"category": "red"}, "count_only": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "COLLECTION_NOT_FOUND");

    // Bitmap-planned doc page.
    let resp = client
        .post(format!("{base_url}/never_created/query"))
        .json(&json!({"filter": {"category": "red"}, "limit": 5}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Bitmap aggregate fast path.
    let resp = client
        .post(format!("{base_url}/never_created/aggregate"))
        .json(&json!({"pipeline": [{"$group": {"_id": "category", "n": {"$count": {}}}}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Distinct.
    let resp = client
        .post(format!("{base_url}/never_created/distinct"))
        .json(&json!({"field": "category"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Dropped-collection variant: exists, then dropped, then queried.
    client
        .delete(format!("{base_url}/events_b"))
        .send()
        .await
        .unwrap();
    let resp = client
        .post(format!("{base_url}/events_b/query"))
        .json(&json!({"filter": {"category": "red"}, "count_only": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        404,
        "dropped collection must 404 on bitmap plans"
    );

    // The unfiltered doc_counter count keeps its 404 too.
    let resp = client
        .post(format!("{base_url}/events_b/query"))
        .json(&json!({"count_only": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
