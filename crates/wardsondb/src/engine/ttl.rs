use serde::{Deserialize, Serialize};
use tracing::info;

use crate::engine::backend::StorageBackend;
use crate::error::AppError;
use crate::query::filter::parse_filter;

use super::storage::Storage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtlConfig {
    pub retention_days: u64,
    pub field: String,
    pub enabled: bool,
}

impl Storage {
    pub fn set_ttl(
        &self,
        collection: &str,
        retention_days: u64,
        field: &str,
    ) -> Result<TtlConfig, AppError> {
        self.check_not_poisoned()?;
        self.ensure_collection_exists(collection)?;

        let config = TtlConfig {
            retention_days,
            field: field.to_string(),
            enabled: true,
        };

        let meta_key = format!("ttl:{collection}");
        let bytes = serde_json::to_vec(&config)?;
        let mut batch = self.write_batch();
        batch.insert(&self.meta, meta_key.as_bytes(), &bytes)?;
        self.commit_batch(batch)?;

        Ok(config)
    }

    pub fn get_ttl(&self, collection: &str) -> Result<Option<TtlConfig>, AppError> {
        self.ensure_collection_exists(collection)?;
        let meta_key = format!("ttl:{collection}");
        match self.engine.get(&self.meta, meta_key.as_bytes())? {
            Some(bytes) => {
                let config: TtlConfig = serde_json::from_slice(&bytes)?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    pub fn delete_ttl(&self, collection: &str) -> Result<(), AppError> {
        self.check_not_poisoned()?;
        self.ensure_collection_exists(collection)?;
        let meta_key = format!("ttl:{collection}");
        let mut batch = self.write_batch();
        batch.remove(&self.meta, meta_key.as_bytes())?;
        self.commit_batch(batch)?;
        Ok(())
    }

    pub fn get_all_ttl_configs(&self) -> Result<Vec<(String, TtlConfig)>, AppError> {
        let mut results = Vec::new();
        let mut item_err: Option<AppError> = None;
        self.engine
            .scan_prefix(&self.meta, b"ttl:", &mut |key, value| {
                let key_str = match std::str::from_utf8(key) {
                    Ok(s) => s,
                    Err(e) => {
                        item_err = Some(AppError::Internal(format!("Invalid key: {e}")));
                        return std::ops::ControlFlow::Break(());
                    }
                };
                let collection = key_str.strip_prefix("ttl:").unwrap_or(key_str).to_string();
                match serde_json::from_slice::<TtlConfig>(value) {
                    Ok(config) => {
                        results.push((collection, config));
                        std::ops::ControlFlow::Continue(())
                    }
                    Err(e) => {
                        item_err = Some(e.into());
                        std::ops::ControlFlow::Break(())
                    }
                }
            })?;
        if let Some(e) = item_err {
            return Err(e);
        }
        Ok(results)
    }

    pub fn run_ttl_cleanup(&self, collection: &str, config: &TtlConfig) -> Result<u64, AppError> {
        if !config.enabled {
            return Ok(0);
        }

        let cutoff = chrono::Utc::now() - chrono::Duration::days(config.retention_days as i64);
        let cutoff_str = cutoff.to_rfc3339();

        let filter_json = serde_json::json!({
            &config.field: {"$lt": cutoff_str}
        });
        let filter = parse_filter(&filter_json)?;

        let deleted = self.delete_by_query(collection, &filter)?;
        if deleted > 0 {
            info!(
                collection = collection,
                deleted = deleted,
                field = %config.field,
                retention_days = config.retention_days,
                "TTL cleanup completed"
            );
        }
        Ok(deleted)
    }
}
