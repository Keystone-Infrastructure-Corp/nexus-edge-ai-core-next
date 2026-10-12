//! Store

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    #[serde(default = "default_sqlite_url")]
    pub url: String,
    #[serde(default)]
    pub seed_from_config: bool,
    /// If true, attach a DuckDB analytics view via `ATTACH ... AS analytics`.
    #[serde(default)]
    pub duckdb_attach: bool,
    #[serde(default = "default_duckdb_path")]
    pub duckdb_path: PathBuf,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            url: default_sqlite_url(),
            seed_from_config: true,
            duckdb_attach: false,
            duckdb_path: default_duckdb_path(),
        }
    }
}

fn default_sqlite_url() -> String {
    "sqlite:///var/lib/nexus/nexus.db?mode=rwc".to_string()
}

fn default_duckdb_path() -> PathBuf {
    PathBuf::from("/var/lib/nexus/analytics.duckdb")
}
