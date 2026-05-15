//! Content-addressed cache for parsed LLM responses.
//!
//! Every successful one-shot LLM call writes its parsed payload here keyed on
//! the canonical request material (operation, model, prompts, schema,
//! temperature, …). Subsequent calls with byte-identical inputs short-circuit
//! the network round-trip and replay the parsed value, which is the cheapest
//! possible recovery when the caller (eg. `lgit --compose`) is rerun after a
//! transient failure or unrelated edit.
//!
//! Backed by `SQLite` for atomic upserts and TTL-based eviction. The cache is
//! best-effort: any failure to read/write is logged and skipped — never fatal.

use std::{
   path::{Path, PathBuf},
   sync::{Arc, OnceLock},
   time::{Duration, SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
   config::CommitConfig,
   error::{CommitGenError, Result},
};

/// Bumped whenever the on-disk row format or hashing scheme changes. Existing
/// rows with a different schema version are treated as misses.
const SCHEMA_VERSION: i32 = 1;

/// Approximate inverse probability of running a TTL prune on each successful
/// `put` call. Keeps the cache bounded without scheduling background work.
const PRUNE_DIVISOR: u64 = 64;

/// Holds the process-wide cache. Initialized from runtime config in `main`,
/// hence `OnceLock` rather than `LazyLock` (the value depends on user config
/// loaded at startup, not on a static initializer).
static GLOBAL: OnceLock<Option<Arc<LlmCache>>> = OnceLock::new();

/// Initialize the global LLM response cache from `config`. Idempotent: only
/// the first call wins.
pub fn init(config: &CommitConfig) {
   let _ = GLOBAL.set(build_from_config(config));
}

/// Get the active cache handle, if any. Cheap clone of an `Arc`.
pub fn global() -> Option<Arc<LlmCache>> {
   GLOBAL.get().and_then(Option::clone)
}

fn build_from_config(config: &CommitConfig) -> Option<Arc<LlmCache>> {
   if !config.cache_enabled {
      return None;
   }
   let dir = resolve_cache_dir(config)?;
   let path = dir.join("responses.sqlite");
   let ttl = Duration::from_secs(u64::from(config.cache_ttl_days).saturating_mul(86_400));
   match LlmCache::open(&path, ttl) {
      Ok(cache) => Some(Arc::new(cache)),
      Err(err) => {
         crate::style::warn(&format!(
            "LLM response cache disabled (failed to open {}): {err}",
            path.display()
         ));
         None
      },
   }
}

fn resolve_cache_dir(config: &CommitConfig) -> Option<PathBuf> {
   if let Some(dir) = config.cache_dir.as_deref()
      && !dir.is_empty()
   {
      return Some(PathBuf::from(dir));
   }
   if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
      && !xdg.is_empty()
   {
      return Some(PathBuf::from(xdg).join("llm-git"));
   }
   if let Ok(home) = std::env::var("HOME") {
      return Some(PathBuf::from(home).join(".cache").join("llm-git"));
   }
   if let Ok(home) = std::env::var("USERPROFILE") {
      return Some(PathBuf::from(home).join(".cache").join("llm-git"));
   }
   None
}

/// SQLite-backed cache of LLM responses. Cheap to clone via `Arc`.
pub struct LlmCache {
   conn:     Mutex<Connection>,
   ttl_secs: u64,
}

impl LlmCache {
   /// Open (or create) the cache at `path` with the given TTL. A TTL of zero
   /// disables expiration.
   pub fn open(path: &Path, ttl: Duration) -> Result<Self> {
      if let Some(parent) = path.parent() {
         std::fs::create_dir_all(parent).map_err(|err| {
            CommitGenError::Other(format!("create cache dir {}: {err}", parent.display()))
         })?;
      }
      let conn = Connection::open(path)
         .map_err(|err| CommitGenError::Other(format!("open llm cache db: {err}")))?;
      conn
         .pragma_update(None, "journal_mode", "WAL")
         .map_err(|err| CommitGenError::Other(format!("pragma WAL: {err}")))?;
      conn
         .pragma_update(None, "synchronous", "NORMAL")
         .map_err(|err| CommitGenError::Other(format!("pragma synchronous: {err}")))?;
      conn
         .execute_batch(
            "CREATE TABLE IF NOT EXISTS responses (
                key            TEXT    PRIMARY KEY,
                schema_version INTEGER NOT NULL,
                model          TEXT    NOT NULL,
                operation      TEXT    NOT NULL,
                response       TEXT    NOT NULL,
                created_at     INTEGER NOT NULL,
                accessed_at    INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_responses_created_at
                ON responses(created_at);",
         )
         .map_err(|err| CommitGenError::Other(format!("create cache schema: {err}")))?;
      Ok(Self { conn: Mutex::new(conn), ttl_secs: ttl.as_secs() })
   }

   /// Look up the stored payload string for `key`. Returns `None` on miss,
   /// expired entry, or any underlying error (cache failures are silent).
   pub fn get(&self, key: &str) -> Option<String> {
      let conn = self.conn.lock();
      let now = now_unix();
      let row: Option<(String, i64)> = conn
         .query_row(
            "SELECT response, created_at FROM responses
             WHERE key = ?1 AND schema_version = ?2",
            params![key, SCHEMA_VERSION],
            |row| Ok((row.get(0)?, row.get(1)?)),
         )
         .optional()
         .ok()
         .flatten();
      let (response, created_at) = row?;
      if self.ttl_secs > 0 {
         let cutoff = now.saturating_sub(self.ttl_secs);
         if (created_at as u64) < cutoff {
            let _ = conn.execute("DELETE FROM responses WHERE key = ?1", params![key]);
            return None;
         }
      }
      let _ = conn
         .execute("UPDATE responses SET accessed_at = ?1 WHERE key = ?2", params![now as i64, key]);
      Some(response)
   }

   /// Insert (or replace) a cached payload. Failures are silently swallowed —
   /// the cache must never break the actual operation.
   pub fn put(&self, key: &str, model: &str, operation: &str, response: &str) {
      let conn = self.conn.lock();
      let now = now_unix();
      let _ = conn.execute(
         "INSERT OR REPLACE INTO responses
          (key, schema_version, model, operation, response, created_at, accessed_at)
          VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
         params![key, SCHEMA_VERSION, model, operation, response, now as i64],
      );
      if self.ttl_secs > 0 && now.is_multiple_of(PRUNE_DIVISOR) {
         let cutoff = now.saturating_sub(self.ttl_secs);
         let _ =
            conn.execute("DELETE FROM responses WHERE created_at < ?1", params![cutoff as i64]);
      }
   }
}

fn now_unix() -> u64 {
   SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map(|d| d.as_secs())
      .unwrap_or(0)
}

/// Material that uniquely identifies a one-shot LLM call. Hashed into the
/// cache key.
pub struct CacheMaterial<'a> {
   pub operation:        &'a str,
   pub model:            &'a str,
   pub tool_name:        &'a str,
   pub tool_description: &'a str,
   pub system_prompt:    &'a str,
   pub user_prompt:      &'a str,
   pub schema:           &'a serde_json::Value,
   pub temperature:      f32,
   pub max_tokens:       u32,
   pub api_mode:         &'a str,
}

/// Compute a content-addressed cache key over `material`. Stable across runs
/// for byte-identical inputs.
pub fn compute_key(material: &CacheMaterial<'_>) -> String {
   let mut hasher = blake3::Hasher::new();
   hasher.update(b"llm-cache/v1\n");
   write_field(&mut hasher, "operation", material.operation);
   write_field(&mut hasher, "model", material.model);
   write_field(&mut hasher, "api_mode", material.api_mode);
   write_field(&mut hasher, "tool_name", material.tool_name);
   write_field(&mut hasher, "tool_description", material.tool_description);
   write_field(&mut hasher, "system", material.system_prompt);
   write_field(&mut hasher, "user", material.user_prompt);
   // serde_json::Value uses BTreeMap by default → keys serialize in stable
   // order without preserve_order, giving a canonical schema string.
   let schema_canonical = serde_json::to_string(material.schema).unwrap_or_else(|_| String::new());
   write_field(&mut hasher, "schema", &schema_canonical);
   hasher.update(b"temperature\x00");
   hasher.update(&material.temperature.to_bits().to_le_bytes());
   hasher.update(b"\nmax_tokens\x00");
   hasher.update(&material.max_tokens.to_le_bytes());
   hasher.update(b"\n");
   hasher.finalize().to_hex().to_string()
}

fn write_field(hasher: &mut blake3::Hasher, name: &str, value: &str) {
   hasher.update(name.as_bytes());
   hasher.update(b"\x00");
   hasher.update(value.as_bytes());
   hasher.update(b"\n");
}

#[cfg(test)]
mod tests {
   use std::sync::Arc;

   use serde_json::json;
   use tempfile::tempdir;

   use super::*;

   fn material<'a>() -> CacheMaterial<'a> {
      // Stable static-ish references for tests.
      static SCHEMA: std::sync::LazyLock<serde_json::Value> =
         std::sync::LazyLock::new(|| json!({"foo": "bar"}));
      CacheMaterial {
         operation:        "test",
         model:            "test-model",
         tool_name:        "tool",
         tool_description: "desc",
         system_prompt:    "system",
         user_prompt:      "user",
         schema:           &SCHEMA,
         temperature:      0.0,
         max_tokens:       100,
         api_mode:         "ChatCompletions",
      }
   }

   #[test]
   fn key_is_stable_and_collision_resistant() {
      let m = material();
      let k1 = compute_key(&m);
      let k2 = compute_key(&m);
      assert_eq!(k1, k2);

      let mut other = material();
      other.user_prompt = "different";
      assert_ne!(k1, compute_key(&other));
   }

   #[test]
   fn roundtrip_get_put() {
      let dir = tempdir().unwrap();
      let cache =
         Arc::new(LlmCache::open(&dir.path().join("c.sqlite"), Duration::from_secs(60)).unwrap());
      assert!(cache.get("k").is_none());
      cache.put("k", "model", "op", "{\"x\":1}");
      assert_eq!(cache.get("k").as_deref(), Some("{\"x\":1}"));
      cache.put("k", "model", "op", "{\"x\":2}");
      assert_eq!(cache.get("k").as_deref(), Some("{\"x\":2}"));
   }

   #[test]
   fn ttl_zero_disables_expiry() {
      let dir = tempdir().unwrap();
      let cache = LlmCache::open(&dir.path().join("c.sqlite"), Duration::from_secs(0)).unwrap();
      cache.put("k", "model", "op", "v");
      assert_eq!(cache.get("k").as_deref(), Some("v"));
   }
}
