//! File-backed tracing for profiling CLI execution.
//!
//! Profiling uses tracing spans as the primary timing primitive. The subscriber
//! emits JSONL span lifecycle events; span close events include elapsed
//! busy/idle time, so nested functions and sections can be profiled without
//! bespoke timers.

use std::{
   fs::OpenOptions,
   path::{Path, PathBuf},
};

use tracing::Level;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
   filter::{LevelFilter, Targets},
   fmt::format::FmtSpan,
   layer::SubscriberExt,
};

use crate::{CommitGenError, Result};

/// Tracing target used by every profiling event.
pub const TARGET: &str = "lgit";

/// Owns the background tracing worker. Dropping it flushes the trace file.
pub struct TraceGuard {
   _guard: WorkerGuard,
   path:   PathBuf,
}

impl TraceGuard {
   pub fn path(&self) -> &Path {
      &self.path
   }
}

/// Initialize JSONL tracing to `path`.
///
/// The caller must keep the returned guard alive until shutdown so buffered
/// events are flushed to disk.
pub fn init_file_tracing(path: &Path) -> Result<TraceGuard> {
   if let Some(parent) = path
      .parent()
      .filter(|parent| !parent.as_os_str().is_empty())
   {
      std::fs::create_dir_all(parent)?;
   }

   let file = OpenOptions::new().create(true).append(true).open(path)?;
   let (writer, guard) = tracing_appender::non_blocking(file);

   let filter = Targets::new().with_target(TARGET, LevelFilter::TRACE);
   let layer = tracing_subscriber::fmt::layer()
      .json()
      .with_ansi(false)
      .with_current_span(true)
      .with_span_list(true)
      .with_target(true)
      .with_level(true)
      .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
      .with_writer(writer);

   let subscriber = tracing_subscriber::registry().with(filter).with(layer);
   tracing::subscriber::set_global_default(subscriber).map_err(|error| {
      CommitGenError::Other(format!("Failed to initialize profiling trace subscriber: {error}"))
   })?;

   tracing::info!(
      target: TARGET,
      event = "trace_started",
      path = %path.display(),
      pid = std::process::id(),
   );

   Ok(TraceGuard { _guard: guard, path: path.to_path_buf() })
}

#[inline]
pub fn enabled() -> bool {
   tracing::enabled!(target: TARGET, Level::INFO)
}

/// Build a profiling span for a named logical section.
///
/// Callers enter the returned span with `.entered()` for synchronous sections
/// or use `Future::instrument(span)` for async blocks. Function-level profiling
/// should prefer `#[tracing::instrument]`.
#[inline]
pub fn section(section: &'static str) -> tracing::Span {
   tracing::info_span!(target: TARGET, "profile.section", section)
}
