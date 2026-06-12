use std::{
   path::{Path, PathBuf},
   time::{Duration, Instant},
};

use analysis::{ScopeAnalyzer, extract_scope_candidates};
use api::{
   AnalysisContext, fallback_summary, generate_analysis_with_map_reduce, generate_fast_commit,
   generate_summary_from_analysis, summary_from_holistic_analysis,
};
use arboard::Clipboard;
use clap::{CommandFactory, Parser};
use compose::run_compose_mode;
use config::CommitConfig;
use diff::{
   classify_diff_whitespace, smart_truncate_diff, strip_whitespace_only_files,
   truncate_diff_by_lines,
};
use error::{CommitGenError, Result};
use git::{
   commit_snapshot_tree, ensure_git_repo, get_common_scopes, get_git_diff, get_git_numstat,
   get_git_stat, get_recent_commits, git_command, git_commit, git_push,
   init_git_command_settings, write_real_index_tree,
};
use llm_git::{style, tokens::create_token_counter, *};
use normalization::{format_commit_message, post_process_commit_message};
use types::{Args, CommitSummary, CommitType, ConventionalCommit, Mode, resolve_model_name};
use validation::{check_type_scope_consistency, validate_commit_message};

/// Print status messages to stderr in pipe mode, stdout otherwise.
macro_rules! status {
   ($($arg:tt)*) => {
      if llm_git::style::pipe_mode() {
         eprintln!($($arg)*);
      } else {
         println!($($arg)*);
      }
   };
}

/// Save debug output to the specified directory
#[tracing::instrument(target = "lgit", name = "io.save_debug_output", skip_all, fields(path = %dir.join(filename).display()))]
fn save_debug_output(dir: &Path, filename: &str, content: &str) -> Result<()> {
   std::fs::create_dir_all(dir)?;
   let path = dir.join(filename);
   std::fs::write(&path, content)?;
   Ok(())
}

/// Generate a shell completion script for `shell` and write it to stdout.
///
/// Uses the real installed binary name (`lgit`) rather than the crate name so
/// the emitted script wires up against the command users actually invoke.
fn print_completions(shell: clap_complete::Shell) {
   let mut cmd = Args::command();
   clap_complete::generate(shell, &mut cmd, "lgit", &mut std::io::stdout());
}

#[derive(Debug, Clone, serde::Serialize)]
struct TimingPhase {
   phase:       String,
   duration_ms: f64,
   share_pct:   f64,
}

#[derive(Debug, serde::Serialize)]
struct TimingReport {
   total_ms: f64,
   phases:   Vec<TimingPhase>,
}

fn round_ms(duration: Duration) -> f64 {
   (duration.as_secs_f64() * 1000.0 * 10.0).round() / 10.0
}

fn record_timing(phases: &mut Option<Vec<TimingPhase>>, phase: &str, duration: Duration) {
   let duration_ms = round_ms(duration);
   if profile::enabled() {
      tracing::info!(
         target: profile::TARGET,
         event = "timing_recorded",
         section = phase,
         elapsed_ms = duration.as_secs_f64() * 1000.0,
         elapsed_us = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
      );
   }

   let Some(phases) = phases.as_mut() else {
      return;
   };

   phases.push(TimingPhase { phase: phase.to_string(), duration_ms, share_pct: 0.0 });
}

fn timings_enabled(args: &Args) -> bool {
   args.debug_output.is_some()
      || args.trace_output.is_some()
      || std::env::var_os("LLM_GIT_TRACE_FILE").is_some_and(|path| !path.is_empty())
      || std::env::var("LLM_GIT_TRACE").is_ok()
}

fn trace_output_path(args: &Args) -> Option<PathBuf> {
   args.trace_output.clone().or_else(|| {
      std::env::var_os("LLM_GIT_TRACE_FILE")
         .and_then(|path| (!path.is_empty()).then(|| PathBuf::from(path)))
   })
}

fn finalize_timings(mut phases: Vec<TimingPhase>, total: Duration) -> TimingReport {
   let total_ms = round_ms(total);
   for phase in &mut phases {
      phase.share_pct = if total_ms > 0.0 {
         ((phase.duration_ms / total_ms) * 1000.0).round() / 10.0
      } else {
         0.0
      };
   }

   TimingReport { total_ms, phases }
}

fn emit_timing_report(args: &Args, report: &TimingReport) -> Result<()> {
   if let Some(debug_dir) = &args.debug_output {
      let report_json = serde_json::to_string_pretty(report).map_err(CommitGenError::from)?;
      save_debug_output(debug_dir, "timings.json", &report_json)?;
   }

   if profile::enabled() {
      tracing::info!(
         target: profile::TARGET,
         event = "timing_report_finished",
         total_ms = report.total_ms,
         phase_count = report.phases.len(),
      );
   }

   if std::env::var("LLM_GIT_TRACE").is_ok() {
      eprintln!("[TIMING] total={:.1}ms", report.total_ms);
      for phase in &report.phases {
         eprintln!(
            "[TIMING] {:>28} {:>8.1}ms {:>5.1}%",
            phase.phase, phase.duration_ms, phase.share_pct
         );
      }
   }

   Ok(())
}

/// Run test mode for fixture-based testing
#[tracing::instrument(target = "lgit", name = "test.run_test_mode", skip_all)]
async fn run_test_mode(args: &Args, config: &CommitConfig) -> Result<()> {
   use llm_git::testing::{self, TestRunner, TestSummary};

   let fixtures_dir = args
      .fixtures_dir
      .clone()
      .unwrap_or_else(testing::fixtures_dir);

   // Handle --test-list
   if args.test_list {
      let fixtures = testing::fixture::discover_fixtures(&fixtures_dir)?;
      if fixtures.is_empty() {
         println!("No fixtures found in {}", fixtures_dir.display());
      } else {
         println!("Available fixtures ({}):", fixtures.len());
         for name in fixtures {
            println!("  {name}");
         }
      }
      return Ok(());
   }

   // Handle --test-add
   if let Some(commit_hash) = &args.test_add {
      let name = args.test_name.as_ref().ok_or_else(|| {
         CommitGenError::Other("--test-name is required when using --test-add".to_string())
      })?;

      return add_fixture(&fixtures_dir, commit_hash, name, &args.dir, config);
   }

   // Handle --test-update
   if args.test_update {
      let runner =
         TestRunner::new(&fixtures_dir, config.clone()).with_filter(args.test_filter.clone());

      println!("Updating golden files...");
      let updated = runner.update_all().await?;
      println!("Updated {} fixtures:", updated.len());
      for name in &updated {
         println!("  ✓ {name}");
      }
      return Ok(());
   }

   // Default: run tests
   let runner =
      TestRunner::new(&fixtures_dir, config.clone()).with_filter(args.test_filter.clone());

   println!("Running fixture tests from {}...\n", fixtures_dir.display());

   let results = runner.run_all().await?;

   if results.is_empty() {
      println!("No fixtures found.");
      return Ok(());
   }

   // Print results
   for result in &results {
      if let Some(err) = &result.error {
         println!("✗ {} - ERROR: {}", result.name, err);
      } else if let Some(cmp) = &result.comparison {
         println!("{} {} - {}", if cmp.passed { "✓" } else { "✗" }, result.name, cmp.summary);
      } else {
         println!("? {} - no golden file", result.name);
      }
   }

   // Print summary
   let summary = TestSummary::from_results(&results);
   println!("\n─────────────────────────────────────");
   println!(
      "Total: {} | Passed: {} | Failed: {} | No golden: {} | Errors: {}",
      summary.total, summary.passed, summary.failed, summary.no_golden, summary.errors
   );

   // Generate HTML report if requested
   if let Some(report_path) = &args.test_report {
      // Load fixtures for comparison display
      let fixture_names = testing::fixture::discover_fixtures(&fixtures_dir)?;
      let mut fixtures = Vec::new();
      for name in &fixture_names {
         if let Some(pattern) = &args.test_filter
            && !name.contains(pattern)
         {
            continue;
         }
         if let Ok(f) = testing::Fixture::load(&fixtures_dir, name) {
            fixtures.push(f);
         }
      }

      testing::generate_html_report(&results, &fixtures, report_path)?;
      println!("\nHTML report generated: {}", report_path.display());
   }

   if !summary.all_passed() {
      return Err(CommitGenError::Other("Some tests failed".to_string()));
   }

   Ok(())
}

/// Add a new fixture from a commit
#[tracing::instrument(target = "lgit", name = "test.add_fixture", skip_all, fields(commit = commit_hash, name, repo_dir))]
fn add_fixture(
   fixtures_dir: &Path,
   commit_hash: &str,
   name: &str,
   repo_dir: &str,
   config: &CommitConfig,
) -> Result<()> {
   use llm_git::testing::{
      Fixture, FixtureContext, FixtureEntry, FixtureInput, FixtureMeta, Manifest,
   };

   println!("Creating fixture '{name}' from commit {commit_hash}...");

   // Get diff and stat
   let diff = git::get_git_diff(&Mode::Commit, Some(commit_hash), repo_dir, config)?;
   let stat = git::get_git_stat(&Mode::Commit, Some(commit_hash), repo_dir, config)?;

   // Get scope candidates
   let (scope_candidates, _) =
      analysis::extract_scope_candidates(&Mode::Commit, Some(commit_hash), repo_dir, config)?;

   // Get context from current repo state
   let (recent_commits_str, common_scopes_str) = match git::get_recent_commits(repo_dir, 20) {
      Ok(commits) if !commits.is_empty() => {
         let style_patterns = git::extract_style_patterns(&commits);
         let style_str = style_patterns.map(|p| p.format_for_prompt());

         let scopes = git::get_common_scopes(repo_dir, 100)
            .ok()
            .filter(|s| !s.is_empty())
            .map(|scopes| {
               scopes
                  .iter()
                  .take(10)
                  .map(|(scope, count)| format!("{scope} ({count})"))
                  .collect::<Vec<_>>()
                  .join(", ")
            });

         (style_str, scopes)
      },
      _ => (None, None),
   };

   let repo_meta = llm_git::repo::RepoMetadata::detect(std::path::Path::new(repo_dir));
   let project_context_str = repo_meta.format_for_prompt();

   // Build fixture
   let fixture = Fixture {
      name:   name.to_string(),
      meta:   FixtureMeta {
         source_repo:   repo_dir.to_string(),
         source_commit: commit_hash.to_string(),
         description:   format!("Fixture from commit {commit_hash}"),
         captured_at:   chrono::Utc::now().to_rfc3339(),
         tags:          vec![],
      },
      input:  FixtureInput {
         diff,
         stat,
         scope_candidates,
         context: FixtureContext {
            recent_commits:  recent_commits_str,
            common_scopes:   common_scopes_str,
            project_context: project_context_str,
            user_context:    None,
         },
      },
      golden: None,
   };

   // Save fixture
   std::fs::create_dir_all(fixtures_dir)?;
   fixture.save(fixtures_dir)?;

   // Update manifest
   let mut manifest = Manifest::load(fixtures_dir)?;
   manifest.add(name.to_string(), FixtureEntry {
      description: format!("From commit {commit_hash}"),
      tags:        vec![],
   });
   manifest.save(fixtures_dir)?;

   println!("✓ Created fixture at {}/{}", fixtures_dir.display(), name);
   println!("  Run with --test-update to generate golden files");

   Ok(())
}

/// Apply CLI overrides to config
fn apply_cli_overrides(config: &mut CommitConfig, args: &Args) {
   if let Some(model) = &args.model {
      let resolved = resolve_model_name(model);
      config.analysis_model.clone_from(&resolved);
      config.summary_model = resolved;
   }
   if let Some(temp) = args.temperature {
      if (0.0..=1.0).contains(&temp) {
         config.temperature = temp;
      } else {
         eprintln!(
            "Warning: Temperature {} out of range [0.0, 1.0], using default {}",
            temp, config.temperature
         );
      }
   }
   if args.exclude_old_message {
      config.exclude_old_message = true;
   }
}

/// Load config from args or default
#[tracing::instrument(target = "lgit", name = "config.load", skip_all)]
fn load_config_from_args(args: &Args) -> Result<CommitConfig> {
   if let Some(config_path) = &args.config {
      CommitConfig::from_file(config_path)
   } else {
      CommitConfig::load()
   }
}

/// Build footers from CLI args
fn build_footers(args: &Args) -> Vec<String> {
   let mut footers = Vec::new();

   // Add issue refs from CLI (standard format: "Token #number")
   for issue in &args.fixes {
      footers.push(format!("Fixes #{}", issue.trim_start_matches('#')));
   }
   for issue in &args.closes {
      footers.push(format!("Closes #{}", issue.trim_start_matches('#')));
   }
   for issue in &args.resolves {
      footers.push(format!("Resolves #{}", issue.trim_start_matches('#')));
   }
   for issue in &args.refs {
      footers.push(format!("Refs #{}", issue.trim_start_matches('#')));
   }

   // Issue refs are now inlined in body items, so we don't add them as separate
   // footers The analysis.issue_refs field is kept for backward compatibility
   // but not used

   // Add breaking change footer if requested
   if args.breaking {
      footers.push("BREAKING CHANGE: This commit introduces breaking changes".to_string());
   }

   footers
}

/// Detect when a changeset differs only in whitespace and, if so, build a
/// ready-to-commit `style: reformatted …` message without calling the model.
///
/// Returns `Ok(None)` when at least one file has a substantive change, so the
/// caller continues with the normal generation pipeline. Compose mode is
/// handled separately and never reaches here.
#[tracing::instrument(target = "lgit", name = "standard.detect_reformat", skip_all, fields(mode = ?args.mode, dir = %args.dir))]
fn detect_reformat_shortcut(
   args: &Args,
   config: &CommitConfig,
) -> Result<Option<ConventionalCommit>> {
   let diff = get_git_diff(&args.mode, args.target.as_deref(), &args.dir, config)?;
   let report = classify_diff_whitespace(&diff);
   if !report.all_whitespace() {
      return Ok(None);
   }
   Ok(Some(build_reformat_commit(&report.whitespace_only_files, args, config)?))
}

/// Build a `style: reformatted …` commit for a whitespace-only changeset.
fn build_reformat_commit(
   files: &[String],
   args: &Args,
   config: &CommitConfig,
) -> Result<ConventionalCommit> {
   let summary_text = match files {
      [single] => {
         let name = single.rsplit('/').next().unwrap_or(single.as_str());
         format!("reformatted {name}")
      },
      many => format!("reformatted {} files", many.len()),
   };

   Ok(ConventionalCommit {
      commit_type: CommitType::new("style")?,
      scope:       None,
      summary:     CommitSummary::new(summary_text, config.summary_hard_limit)?,
      body:        Vec::new(),
      footers:     build_footers(args),
   })
}

fn resolve_fast_mode_model(args: &Args, config: &CommitConfig) -> String {
   if args.model.is_some() || config.legacy_model.is_some() {
      config.analysis_model.clone()
   } else {
      resolve_model_name("haiku")
   }
}

fn auto_fast_changed_lines(numstat: &str, config: &CommitConfig) -> Option<usize> {
   if config.auto_fast_threshold_lines == 0 {
      return None;
   }

   let changed_lines = ScopeAnalyzer::count_changed_lines(numstat, config);
   if changed_lines == 0 || changed_lines > config.auto_fast_threshold_lines {
      None
   } else {
      Some(changed_lines)
   }
}

/// Main generation pipeline: get diff/stat → truncate → analyze → summarize →
/// build commit
#[tracing::instrument(target = "lgit", name = "standard.run_generation", skip_all, fields(mode = ?args.mode, dir = %args.dir))]
async fn run_generation(
   config: &CommitConfig,
   args: &Args,
   token_counter: &tokens::TokenCounter,
   timings: &mut Option<Vec<TimingPhase>>,
) -> Result<ConventionalCommit> {
   let phase_start = Instant::now();
   let diff = get_git_diff(&args.mode, args.target.as_deref(), &args.dir, config)?;
   record_timing(timings, "get_git_diff", phase_start.elapsed());

   let phase_start = Instant::now();
   let stat = get_git_stat(&args.mode, args.target.as_deref(), &args.dir, config)?;
   record_timing(timings, "get_git_stat", phase_start.elapsed());

   // Save debug outputs if requested
   if let Some(debug_dir) = &args.debug_output {
      let phase_start = Instant::now();
      save_debug_output(debug_dir, "diff.patch", &diff)?;
      save_debug_output(debug_dir, "stat.txt", &stat)?;
      record_timing(timings, "write_debug_inputs", phase_start.elapsed());
   }

   // Drop whitespace-only files so the model focuses on substantive changes.
   // A changeset that is *entirely* whitespace never reaches here — it is
   // short-circuited to a reformat commit in `run_cli`.
   let phase_start = Instant::now();
   let diff = strip_whitespace_only_files(&diff).unwrap_or(diff);
   record_timing(timings, "strip_whitespace_only", phase_start.elapsed());

   status!(
      "{} {} {} {} {} {} {}",
      style::dim("›"),
      style::dim("models:"),
      style::dim("analysis"),
      style::model(&config.analysis_model),
      style::dim("summary"),
      style::model(&config.summary_model),
      style::dim(&format!("(temp: {})", config.temperature))
   );

   // Check if map-reduce should be used for large diffs
   // Map-reduce handles its own per-file processing, so we pass the original diff
   // Only apply smart truncation if map-reduce is disabled or diff is below
   // threshold
   let phase_start = Instant::now();
   let use_map_reduce = llm_git::map_reduce::should_use_map_reduce(&diff, config, token_counter);

   let diff = if use_map_reduce {
      // Map-reduce will handle the full diff with per-file analysis
      diff
   } else if diff.len() > config.max_diff_length {
      println!(
         "{}",
         style::warning(&format!(
            "Applying smart truncation (diff size: {} characters)",
            diff.len()
         ))
      );
      smart_truncate_diff(&diff, config.max_diff_length, config, token_counter)
   } else {
      diff
   };
   record_timing(timings, "prepare_diff", phase_start.elapsed());

   // Get recent commits for style consistency
   let phase_start = Instant::now();
   let (recent_commits_str, common_scopes_str) = match get_recent_commits(&args.dir, 20) {
      Ok(commits) if !commits.is_empty() => {
         // Extract structured style patterns
         let style_patterns = git::extract_style_patterns(&commits);
         let style_str = style_patterns.map(|p| p.format_for_prompt());

         let scopes = get_common_scopes(&args.dir, 100)
            .ok()
            .filter(|s| !s.is_empty())
            .map(|scopes| {
               scopes
                  .iter()
                  .take(10)
                  .map(|(scope, count)| format!("{scope} ({count})"))
                  .collect::<Vec<_>>()
                  .join(", ")
            });

         (style_str, scopes)
      },
      _ => (None, None),
   };
   record_timing(timings, "collect_recent_context", phase_start.elapsed());

   // Detect repo metadata for context
   let phase_start = Instant::now();
   let repo_meta = llm_git::repo::RepoMetadata::detect(std::path::Path::new(&args.dir));
   let project_context_str = repo_meta.format_for_prompt();
   record_timing(timings, "detect_repo_metadata", phase_start.elapsed());

   // Generate conventional commit analysis
   let phase_start = Instant::now();
   let context = if args.context.is_empty() {
      None
   } else {
      Some(args.context.join(" "))
   };
   let (scope_candidates_str, _is_wide) =
      extract_scope_candidates(&args.mode, args.target.as_deref(), &args.dir, config)?;
   record_timing(timings, "extract_scope_candidates", phase_start.elapsed());

   let phase_start = Instant::now();
   let ctx = AnalysisContext {
      user_context:    context.as_deref(),
      recent_commits:  recent_commits_str.as_deref(),
      common_scopes:   common_scopes_str.as_deref(),
      project_context: project_context_str.as_deref(),
      debug_output:    args.debug_output.as_deref(),
      debug_prefix:    None,
   };
   let analysis = style::with_spinner("Analysis phase: waiting for LLM response", async {
      generate_analysis_with_map_reduce(
         &stat,
         &diff,
         &config.analysis_model,
         &scope_candidates_str,
         &ctx,
         config,
         token_counter,
      )
      .await
   })
   .await?;
   record_timing(timings, "generate_analysis", phase_start.elapsed());

   // Save analysis debug output
   if let Some(debug_dir) = &args.debug_output {
      let phase_start = Instant::now();
      let analysis_json = serde_json::to_string_pretty(&analysis)?;
      save_debug_output(debug_dir, "analysis.json", &analysis_json)?;
      record_timing(timings, "write_debug_analysis", phase_start.elapsed());
   }

   // Log scope selection
   if let Some(scope) = &analysis.scope {
      status!("{} {} {}", style::dim("›"), style::dim("scope:"), style::scope(&scope.to_string()));
   } else {
      status!("{} {}", style::dim("›"), style::dim("scope: (none)"));
   }

   let detail_points = analysis.body_texts();
   let phase_start = Instant::now();
   let (summary, summary_phase) = match summary_from_holistic_analysis(&analysis, config) {
      Ok(Some(summary)) => (summary, "use_analysis_summary"),
      Ok(None) => {
         let summary = style::with_spinner("Summary phase: waiting for LLM response", async {
            generate_summary_from_analysis(
               &stat,
               analysis.commit_type.as_str(),
               analysis.scope.as_ref().map(|s| s.as_str()),
               &detail_points,
               context.as_deref(),
               config,
               args.debug_output.as_deref(),
               None,
            )
            .await
         })
         .await
         .unwrap_or_else(|err| {
            eprintln!(
               "{}",
               style::warning(&format!(
                  "Failed to create summary with {}: {err}",
                  config.summary_model
               ))
            );
            fallback_summary(&stat, &detail_points, analysis.commit_type.as_str(), config)
         });
         (summary, "generate_summary")
      },
      Err(err) => {
         eprintln!("{}", style::warning(&format!("Invalid analysis summary: {err}")));
         (
            fallback_summary(&stat, &detail_points, analysis.commit_type.as_str(), config),
            "use_analysis_summary",
         )
      },
   };
   record_timing(timings, summary_phase, phase_start.elapsed());

   // Save summary debug output
   if let Some(debug_dir) = &args.debug_output {
      let phase_start = Instant::now();
      let summary_json = serde_json::json!({
         "summary": summary.as_str(),
         "commit_type": analysis.commit_type.as_str(),
         "scope": analysis.scope.as_ref().map(|s| s.as_str()),
      });
      save_debug_output(debug_dir, "summary.json", &serde_json::to_string_pretty(&summary_json)?)?;
      record_timing(timings, "write_debug_summary", phase_start.elapsed());
   }

   let footers = build_footers(args);

   Ok(ConventionalCommit {
      commit_type: analysis.commit_type,
      scope: analysis.scope,
      summary,
      body: detail_points,
      footers,
   })
}

/// Post-process, validate, retry with fallback. Returns validation error if any
#[tracing::instrument(target = "lgit", name = "standard.validate_and_process", skip_all, fields(commit_type = %commit_msg.commit_type, detail_count = detail_points.len()))]
async fn validate_and_process(
   commit_msg: &mut ConventionalCommit,
   stat: &str,
   detail_points: &[String],
   user_context: Option<&str>,
   config: &CommitConfig,
) -> Option<String> {
   let mut validation_error: Option<String> = None;
   for attempt in 0..=2 {
      post_process_commit_message(commit_msg, config);

      // Check soft limit BEFORE full validation (only on first attempt)
      if attempt == 0 {
         let scope_part = commit_msg
            .scope
            .as_ref()
            .map(|s| format!("({s})"))
            .unwrap_or_default();
         let first_line_len =
            commit_msg.commit_type.len() + scope_part.len() + 2 + commit_msg.summary.len();

         if first_line_len > config.summary_soft_limit {
            eprintln!("Summary too long ({first_line_len} chars), retrying generation...");

            // Regenerate summary (call API again)
            match generate_summary_from_analysis(
               stat,
               commit_msg.commit_type.as_str(),
               commit_msg.scope.as_ref().map(|s| s.as_str()),
               detail_points,
               user_context,
               config,
               None,
               None,
            )
            .await
            {
               Ok(new_summary) => {
                  commit_msg.summary = new_summary;
                  continue; // Retry validation loop
               },
               Err(e) => {
                  eprintln!("Retry generation failed: {e}, using fallback");
                  commit_msg.summary =
                     fallback_summary(stat, detail_points, commit_msg.commit_type.as_str(), config);
                  continue;
               },
            }
         }
      }

      // Full validation
      match validate_commit_message(commit_msg, config) {
         Ok(()) => {
            validation_error = None;
            break;
         },
         Err(e) => {
            let message = e.to_string();

            // Special case: if scope is the project name, remove it and re-validate once
            if message.contains("is the project name") && commit_msg.scope.is_some() {
               eprintln!("⚠ Scope matches project name, removing scope...");
               commit_msg.scope = None;
               post_process_commit_message(commit_msg, config);

               // Re-validate with scope removed
               match validate_commit_message(commit_msg, config) {
                  Ok(()) => {
                     validation_error = None;
                     break;
                  },
                  Err(e2) => {
                     eprintln!("Validation failed after scope removal: {e2}");
                     // Fall through to normal retry logic
                  },
               }
            }

            eprintln!("Validation attempt {} failed: {message}", attempt + 1);
            validation_error = Some(message);
            if attempt < 2 {
               commit_msg.summary =
                  fallback_summary(stat, detail_points, commit_msg.commit_type.as_str(), config);
               continue;
            }
            break;
         },
      }
   }
   validation_error
}

/// Copy text to clipboard
#[tracing::instrument(target = "lgit", name = "clipboard.copy", skip_all)]
fn copy_to_clipboard(text: &str) -> Result<()> {
   let mut clipboard = Clipboard::new().map_err(CommitGenError::ClipboardError)?;
   clipboard
      .set_text(text)
      .map_err(CommitGenError::ClipboardError)?;
   Ok(())
}

/// Auto-stage all changes if nothing is staged in the working directory.
#[tracing::instrument(target = "lgit", name = "git.auto_stage_if_needed", skip_all, fields(dir))]
fn auto_stage_if_needed(dir: &str) -> Result<()> {
   let staged_check = git_command()
      .args(["diff", "--cached", "--quiet"])
      .current_dir(dir)
      .status()
      .map_err(|e| CommitGenError::git(format!("Failed to check staged changes: {e}")))?;

   // exit code 1 = changes exist, 0 = no changes
   if staged_check.success() {
      // Check if there are any unstaged changes before staging
      let unstaged_check = git_command()
         .args(["diff", "--quiet"])
         .current_dir(dir)
         .status()
         .map_err(|e| CommitGenError::git(format!("Failed to check unstaged changes: {e}")))?;

      // Check for untracked files
      let untracked_output = git_command()
         .args(["ls-files", "--others", "--exclude-standard"])
         .current_dir(dir)
         .output()
         .map_err(|e| CommitGenError::git(format!("Failed to check untracked files: {e}")))?;

      let has_untracked = !untracked_output.stdout.is_empty();

      // If no unstaged changes AND no untracked files, working directory is clean
      if unstaged_check.success() && !has_untracked {
         return Err(CommitGenError::NoChanges {
            mode: "working directory (nothing to commit)".to_string(),
         });
      }

      status!("{} {}", style::info("›"), style::dim("No staged changes; running git add -A"));
      let add_output = git_command()
         .args(["add", "-A"])
         .current_dir(dir)
         .output()
         .map_err(|e| CommitGenError::git(format!("Failed to stage changes: {e}")))?;

      if !add_output.status.success() {
         let stderr = String::from_utf8_lossy(&add_output.stderr);
         return Err(CommitGenError::git(format!("git add -A failed: {stderr}")));
      }
   }

   Ok(())
}

/// Commit staged-mode output, tolerating mid-run index drift.
///
/// When the live index still matches the analyzed snapshot, plain `git
/// commit` runs (hooks included). When something was staged while the message
/// was generated, the snapshot tree is committed directly instead: the index
/// and worktree are left untouched, so the mid-run staging stays staged for a
/// later commit.
#[tracing::instrument(target = "lgit", name = "git.commit_staged", skip_all, fields(dir = %args.dir))]
fn commit_staged(
   message: &str,
   snapshot_tree: Option<&str>,
   args: &Args,
   config: &CommitConfig,
) -> Result<()> {
   let sign = args.sign || config.gpg_sign;
   let signoff = args.signoff || config.signoff;

   if !args.dry_run
      && let Some(expected_tree) = snapshot_tree
      && write_real_index_tree(&args.dir)? != expected_tree
   {
      status!(
         "{} {}",
         style::info("›"),
         style::dim("Index changed during generation; committing the analyzed snapshot (hooks skipped)")
      );
      match commit_snapshot_tree(message, expected_tree, &args.dir, sign, signoff, args.amend)? {
         Some(hash) => {
            status!(
               "{} {}",
               style::success(style::icons::SUCCESS),
               style::success(&format!("Successfully committed snapshot as {hash:.8}"))
            );
         },
         None => {
            status!(
               "{} {}",
               style::info("›"),
               style::dim("Snapshot already committed; nothing to do")
            );
         },
      }
      return Ok(());
   }

   git_commit(message, args.dry_run, &args.dir, sign, signoff, args.skip_hooks, args.amend)
}

/// Fast mode: single API call to generate a complete commit message.
#[tracing::instrument(target = "lgit", name = "fast.run_fast_mode", skip_all, fields(mode = ?args.mode, dir = %args.dir))]
async fn run_fast_mode(args: &Args, config: &CommitConfig) -> Result<()> {
   let total_start = Instant::now();
   let mut timings = timings_enabled(args).then(Vec::new);

   // Snapshot of the index this run will analyze and commit; compared again
   // right before committing so mid-run staging aborts instead of leaking in.
   let staged_index_tree = if matches!(args.mode, Mode::Staged) && !args.dry_run {
      Some(write_real_index_tree(&args.dir)?)
   } else {
      None
   };

   // Skip changelog entirely in fast mode

   let phase_start = Instant::now();
   let diff = get_git_diff(&args.mode, args.target.as_deref(), &args.dir, config)?;
   record_timing(&mut timings, "get_git_diff", phase_start.elapsed());

   let phase_start = Instant::now();
   let stat = get_git_stat(&args.mode, args.target.as_deref(), &args.dir, config)?;
   record_timing(&mut timings, "get_git_stat", phase_start.elapsed());

   // Save debug outputs if requested
   if let Some(debug_dir) = &args.debug_output {
      let phase_start = Instant::now();
      save_debug_output(debug_dir, "diff.patch", &diff)?;
      save_debug_output(debug_dir, "stat.txt", &stat)?;
      record_timing(&mut timings, "write_debug_inputs", phase_start.elapsed());
   }

   // Drop whitespace-only files so the model focuses on substantive changes.
   let phase_start = Instant::now();
   let diff = strip_whitespace_only_files(&diff).unwrap_or(diff);
   record_timing(&mut timings, "strip_whitespace_only", phase_start.elapsed());

   // Line-budget truncation for fast mode (10k lines)
   let phase_start = Instant::now();
   let diff = truncate_diff_by_lines(&diff, 10_000, config);
   record_timing(&mut timings, "truncate_diff_by_lines", phase_start.elapsed());

   // Extract scope candidates
   let phase_start = Instant::now();
   let (scope_candidates_str, _is_wide) =
      extract_scope_candidates(&args.mode, args.target.as_deref(), &args.dir, config)?;
   record_timing(&mut timings, "extract_scope_candidates", phase_start.elapsed());

   // Fast mode stays cheap by default, but honors the legacy single-model
   // selector when it is configured.
   let model = resolve_fast_mode_model(args, config);

   let user_context = if args.context.is_empty() {
      None
   } else {
      Some(args.context.join(" "))
   };

   status!(
      "{} {} {} {}",
      style::dim("›"),
      style::dim("fast mode:"),
      style::model(&model),
      style::dim(&format!("(temp: {})", config.temperature))
   );

   status!("{} Analyzing {} changes...", style::info("›"), match args.mode {
      Mode::Staged => style::bold("staged"),
      Mode::Commit => style::bold("commit"),
      Mode::Unstaged => style::bold("unstaged"),
      Mode::Compose => unreachable!("compose mode handled separately"),
   });

   // Single API call generates the complete commit
   let phase_start = Instant::now();
   let mut commit_msg = style::with_spinner("Fast mode: waiting for LLM response", async {
      generate_fast_commit(
         &stat,
         &diff,
         &model,
         &scope_candidates_str,
         user_context.as_deref(),
         config,
         args.debug_output.as_deref(),
      )
      .await
   })
   .await?;

   // Populate footers from CLI flags
   // (--fixes/--closes/--resolves/--refs/--breaking)
   commit_msg.footers = build_footers(args);

   record_timing(&mut timings, "generate_fast_commit", phase_start.elapsed());

   // Validate and process (reuse same logic as standard mode)
   let detail_points = commit_msg.body.clone();
   let phase_start = Instant::now();
   let validation_failed =
      validate_and_process(&mut commit_msg, &stat, &detail_points, user_context.as_deref(), config)
         .await;
   record_timing(&mut timings, "validate_and_process", phase_start.elapsed());

   if let Some(err) = &validation_failed {
      eprintln!("Warning: Generated message failed validation even after retry: {err}");
      eprintln!("You may want to manually edit the message before committing.");
   }

   // Check type-scope consistency
   let phase_start = Instant::now();
   check_type_scope_consistency(&commit_msg, &stat);
   record_timing(&mut timings, "check_type_scope_consistency", phase_start.elapsed());

   // Format and display
   let phase_start = Instant::now();
   let formatted_message = format_commit_message(&commit_msg);
   record_timing(&mut timings, "format_commit_message", phase_start.elapsed());

   // Save final commit message if debug output requested
   if let Some(debug_dir) = &args.debug_output {
      let phase_start = Instant::now();
      save_debug_output(debug_dir, "final.txt", &formatted_message)?;
      let commit_json = serde_json::to_string_pretty(&commit_msg).map_err(CommitGenError::from)?;
      save_debug_output(debug_dir, "commit.json", &commit_json)?;
      record_timing(&mut timings, "write_debug_final", phase_start.elapsed());
   }

   // Display: pipe mode outputs raw message, TTY mode shows boxed format
   let phase_start = Instant::now();
   if style::pipe_mode() {
      print!("{formatted_message}");
   } else {
      println!(
         "\n{}",
         style::boxed_message("Generated Commit Message", &formatted_message, style::term_width())
      );
   }
   record_timing(&mut timings, "display_output", phase_start.elapsed());

   // Copy to clipboard if requested
   if args.copy {
      let phase_start = Instant::now();
      match copy_to_clipboard(&formatted_message) {
         Ok(()) => status!("\n{}", style::success("Copied to clipboard")),
         Err(e) => status!("\nNote: Failed to copy to clipboard: {e}"),
      }
      record_timing(&mut timings, "copy_to_clipboard", phase_start.elapsed());
   }

   // Commit for staged mode (unless dry-run)
   if matches!(args.mode, Mode::Staged) {
      if validation_failed.is_some() {
         eprintln!(
            "\n{}",
            style::warning(
               "Skipping commit due to validation failure. Use --dry-run to test or manually \
                commit."
            )
         );
         return Err(CommitGenError::ValidationError(
            "Commit message validation failed".to_string(),
         ));
      }

      status!("\n{}", style::info("Preparing to commit..."));
      let phase_start = Instant::now();
      commit_staged(&formatted_message, staged_index_tree.as_deref(), args, config)?;
      record_timing(&mut timings, "git_commit", phase_start.elapsed());

      // Auto-push if requested (only if not dry-run)
      if args.push && !args.dry_run {
         let phase_start = Instant::now();
         git_push(&args.dir)?;
         record_timing(&mut timings, "git_push", phase_start.elapsed());
      }
   }

   if let Some(timings) = timings {
      let report = finalize_timings(timings, total_start.elapsed());
      emit_timing_report(args, &report)?;
   }

   Ok(())
}
#[tokio::main]
async fn main() -> miette::Result<()> {
   let args = Args::parse();
   let trace_path = trace_output_path(&args);
   let trace_guard = trace_path
      .as_deref()
      .map(profile::init_file_tracing)
      .transpose()?;

   if let Some(active_trace) = &trace_guard {
      tracing::info!(
         target: profile::TARGET,
         event = "cli_started",
         trace_file = %active_trace.path().display(),
         mode = ?args.mode,
         fast = args.fast,
         compose = args.compose,
         rewrite = args.rewrite,
         test = args.test,
         dry_run = args.dry_run,
         dir = %args.dir,
      );
   }

   run_cli(args).await
}

#[tracing::instrument(target = "lgit", name = "main.run_cli", skip_all, fields(mode = ?args.mode, fast = args.fast, compose = args.compose, rewrite = args.rewrite, test = args.test, dry_run = args.dry_run, dir = %args.dir))]
async fn run_cli(args: Args) -> miette::Result<()> {
   // Emit a completion script and exit before any config/git work so it works
   // outside a repository and without a configured API endpoint.
   if let Some(shell) = args.completions {
      print_completions(shell);
      return Ok(());
   }

   // Load config and apply CLI overrides
   let mut config = load_config_from_args(&args)?;
   apply_cli_overrides(&mut config, &args);
   llm_git::llm_cache::init(&config);
   init_git_command_settings(&config);

   let total_start = Instant::now();
   let mut timings = timings_enabled(&args).then(Vec::new);

   // Create token counter from final config
   let phase_start = Instant::now();
   let token_counter = create_token_counter(&config);
   record_timing(&mut timings, "create_token_counter", phase_start.elapsed());

   if !args.test || args.test_add.is_some() {
      let phase_start = Instant::now();
      ensure_git_repo(&args.dir)?;
      record_timing(&mut timings, "ensure_git_repo", phase_start.elapsed());
   }

   // Route to compose mode if --compose flag is present
   if args.compose {
      return Ok(run_compose_mode(&args, &config).await?);
   }

   // Route to rewrite mode if --rewrite flag is present
   if args.rewrite {
      return Ok(rewrite::run_rewrite_mode(&args, &config).await?);
   }

   // Route to test mode if --test flag is present
   if args.test {
      return Ok(run_test_mode(&args, &config).await?);
   }

   // Auto-stage all changes if nothing staged in staged mode
   if matches!(args.mode, Mode::Staged) {
      let phase_start = Instant::now();
      auto_stage_if_needed(&args.dir)?;
      record_timing(&mut timings, "auto_stage_if_needed", phase_start.elapsed());
   }

   // Snapshot of the index the generated message will describe. Captured
   // after auto-staging (and refreshed after changelog maintenance, which
   // legitimately stages entries), then compared right before committing so
   // anything staged mid-run aborts instead of leaking into the commit.
   let mut staged_index_tree = if matches!(args.mode, Mode::Staged) && !args.dry_run {
      Some(write_real_index_tree(&args.dir)?)
   } else {
      None
   };

   // Whitespace-only changesets become a `style: reformatted …` commit with no
   // model call. Checked before fast-mode routing so small reformats are not
   // auto-switched into the LLM fast path.
   let phase_start = Instant::now();
   let reformat_commit = detect_reformat_shortcut(&args, &config)?;
   record_timing(&mut timings, "detect_reformat_shortcut", phase_start.elapsed());

   let mut commit_msg = if let Some(commit) = reformat_commit {
      status!(
         "{} {}",
         style::info("›"),
         style::dim("Detected whitespace-only changes; recording as reformat")
      );
      commit
   } else {
      // Route to fast mode if --fast flag is present
      if args.fast {
         return Ok(run_fast_mode(&args, &config).await?);
      }

      if config.auto_fast_threshold_lines > 0 {
         let phase_start = Instant::now();
         let numstat = get_git_numstat(&args.mode, args.target.as_deref(), &args.dir, &config)?;
         record_timing(&mut timings, "get_git_numstat_for_auto_fast", phase_start.elapsed());

         if let Some(changed_lines) = auto_fast_changed_lines(&numstat, &config) {
            status!(
               "{} {}",
               style::info("›"),
               style::dim(&format!(
                  "Auto-switching to fast mode ({changed_lines} changed lines <= {})",
                  config.auto_fast_threshold_lines
               ))
            );
            return Ok(run_fast_mode(&args, &config).await?);
         }
      }

      // Run changelog maintenance if not disabled (check both CLI flag and config)
      if !args.no_changelog && config.changelog_enabled {
         let phase_start = Instant::now();
         if let Err(e) = llm_git::changelog::run_changelog_flow(&args, &config).await {
            // Don't fail the commit, just warn
            eprintln!("Warning: Changelog update failed: {e}");
         }
         record_timing(&mut timings, "run_changelog_flow", phase_start.elapsed());
      }

      if staged_index_tree.is_some() {
         staged_index_tree = Some(write_real_index_tree(&args.dir)?);
      }

      status!("{} Analyzing {} changes...", style::info("›"), match args.mode {
         Mode::Staged => style::bold("staged"),
         Mode::Commit => style::bold("commit"),
         Mode::Unstaged => style::bold("unstaged"),
         Mode::Compose => unreachable!("compose mode handled separately"),
      });

      // Run generation pipeline
      run_generation(&config, &args, &token_counter, &mut timings).await?
   };

   // Get stat and detail points for validation retry
   let phase_start = Instant::now();
   let stat = get_git_stat(&args.mode, args.target.as_deref(), &args.dir, &config)?;
   record_timing(&mut timings, "get_git_stat_for_validation", phase_start.elapsed());
   let detail_points = commit_msg.body.clone();
   let context = if args.context.is_empty() {
      None
   } else {
      Some(args.context.join(" "))
   };

   // Validate and process
   let phase_start = Instant::now();
   let validation_failed =
      validate_and_process(&mut commit_msg, &stat, &detail_points, context.as_deref(), &config)
         .await;
   record_timing(&mut timings, "validate_and_process", phase_start.elapsed());

   if let Some(err) = &validation_failed {
      eprintln!("Warning: Generated message failed validation even after retry: {err}");
      eprintln!("You may want to manually edit the message before committing.");
   }

   // Check type-scope consistency
   let phase_start = Instant::now();
   check_type_scope_consistency(&commit_msg, &stat);
   record_timing(&mut timings, "check_type_scope_consistency", phase_start.elapsed());

   // Format and display
   let phase_start = Instant::now();
   let formatted_message = format_commit_message(&commit_msg);
   record_timing(&mut timings, "format_commit_message", phase_start.elapsed());

   // Save final commit message if debug output requested
   if let Some(debug_dir) = &args.debug_output {
      let phase_start = Instant::now();
      save_debug_output(debug_dir, "final.txt", &formatted_message)?;
      let commit_json = serde_json::to_string_pretty(&commit_msg).map_err(CommitGenError::from)?;
      save_debug_output(debug_dir, "commit.json", &commit_json)?;
      record_timing(&mut timings, "write_debug_final", phase_start.elapsed());
   }

   // Display: pipe mode outputs raw message, TTY mode shows boxed format
   let phase_start = Instant::now();
   if style::pipe_mode() {
      print!("{formatted_message}");
   } else {
      println!(
         "\n{}",
         style::boxed_message("Generated Commit Message", &formatted_message, style::term_width())
      );

      if std::env::var("LLM_GIT_VERBOSE").is_ok() {
         println!("\nJSON Structure:");
         println!("{}", serde_json::to_string_pretty(&commit_msg).map_err(CommitGenError::from)?);
      }
   }
   record_timing(&mut timings, "display_output", phase_start.elapsed());

   // Copy to clipboard if requested
   if args.copy {
      let phase_start = Instant::now();
      match copy_to_clipboard(&formatted_message) {
         Ok(()) => status!("\n{}", style::success("Copied to clipboard")),
         Err(e) => status!("\nNote: Failed to copy to clipboard: {e}"),
      }
      record_timing(&mut timings, "copy_to_clipboard", phase_start.elapsed());
   }

   // Auto-commit for staged mode (unless dry-run)
   // Don't commit if validation failed
   if matches!(args.mode, Mode::Staged) {
      if validation_failed.is_some() {
         eprintln!(
            "\n{}",
            style::warning(
               "Skipping commit due to validation failure. Use --dry-run to test or manually \
                commit."
            )
         );
         return Err(
            CommitGenError::ValidationError("Commit message validation failed".to_string()).into(),
         );
      }

      status!("\n{}", style::info("Preparing to commit..."));
      let phase_start = Instant::now();
      commit_staged(&formatted_message, staged_index_tree.as_deref(), &args, &config)?;
      record_timing(&mut timings, "git_commit", phase_start.elapsed());

      // Auto-push if requested (only if not dry-run)
      if args.push && !args.dry_run {
         let phase_start = Instant::now();
         git_push(&args.dir)?;
         record_timing(&mut timings, "git_push", phase_start.elapsed());
      }
   }

   if let Some(timings) = timings {
      let report = finalize_timings(timings, total_start.elapsed());
      emit_timing_report(&args, &report)?;
   }

   Ok(())
}

#[cfg(test)]
mod tests {
   use llm_git::types::ConventionalAnalysis;

   use super::*;

   // ========== CLI / completions Tests ==========

   #[test]
   fn cli_definition_is_valid() {
      // clap's own consistency checks: catches conflicting attrs, duplicate
      // flags, bad value parsers, etc. across the whole Args definition.
      Args::command().debug_assert();
   }

   fn run_test_git(dir: &std::path::Path, args: &[&str]) -> String {
      let output = git_command()
         .args(args)
         .current_dir(dir)
         .output()
         .unwrap_or_else(|err| panic!("git {args:?} failed to spawn: {err}"));
      assert!(
         output.status.success(),
         "git {:?} failed: {}",
         args,
         String::from_utf8_lossy(&output.stderr)
      );
      String::from_utf8_lossy(&output.stdout).to_string()
   }

   #[test]
   fn test_commit_staged_commits_snapshot_on_index_drift() {
      let dir = tempfile::TempDir::new().unwrap();
      let path = dir.path();
      run_test_git(path, &["init"]);
      run_test_git(path, &["config", "user.name", "Drift Test"]);
      run_test_git(path, &["config", "user.email", "drift@test.local"]);
      run_test_git(path, &["config", "commit.gpgsign", "false"]);
      std::fs::write(path.join("a.txt"), "one\n").unwrap();
      run_test_git(path, &["add", "a.txt"]);
      run_test_git(path, &["commit", "-m", "base"]);

      // Analyzed snapshot: a.txt modified and staged.
      std::fs::write(path.join("a.txt"), "two\n").unwrap();
      run_test_git(path, &["add", "a.txt"]);
      let snapshot_tree = write_real_index_tree(path.to_str().unwrap()).unwrap();

      // Mid-run drift: more staging while the message was generated.
      std::fs::write(path.join("b.txt"), "drift\n").unwrap();
      run_test_git(path, &["add", "b.txt"]);

      let args = Args { dir: path.to_str().unwrap().to_string(), ..Default::default() };
      let config = CommitConfig::default();
      commit_staged("feat: snapshot commit", Some(&snapshot_tree), &args, &config).unwrap();

      assert_eq!(run_test_git(path, &["rev-parse", "HEAD^{tree}"]).trim(), snapshot_tree);
      assert!(run_test_git(path, &["log", "-1", "--format=%s"]).contains("feat: snapshot commit"));
      assert_eq!(
         run_test_git(path, &["diff", "--cached", "--name-only"]).trim(),
         "b.txt",
         "drifted staging stays staged for the next commit"
      );
      assert_eq!(std::fs::read_to_string(path.join("a.txt")).unwrap(), "two\n");
      assert_eq!(std::fs::read_to_string(path.join("b.txt")).unwrap(), "drift\n");
   }

   #[test]
   fn trace_output_flag_enables_file_profiling() {
      let args = Args::try_parse_from(["lgit", "--trace-output", "profile.jsonl", "--dry-run"])
         .expect("trace output flag should parse");

      assert_eq!(args.trace_output.as_deref(), Some(Path::new("profile.jsonl")));
      assert_eq!(trace_output_path(&args).as_deref(), Some(Path::new("profile.jsonl")));
      assert!(timings_enabled(&args));
   }

   #[test]
   fn completions_generate_for_all_shells() {
      use clap_complete::Shell;
      for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell, Shell::Elvish] {
         let mut cmd = Args::command();
         let mut buf = Vec::new();
         clap_complete::generate(shell, &mut cmd, "lgit", &mut buf);
         let script = String::from_utf8(buf).expect("completion script must be valid UTF-8");
         assert!(!script.is_empty(), "{shell} completion script must not be empty");
         assert!(
            script.contains("lgit"),
            "{shell} completion should reference the lgit binary name"
         );
      }
   }

   fn test_analysis_with_summary(summary: Option<&str>) -> ConventionalAnalysis {
      ConventionalAnalysis {
         commit_type: types::CommitType::new("feat").unwrap(),
         scope:       Some(types::Scope::new("api").unwrap()),
         summary:     summary.map(str::to_string),
         details:     vec![],
         issue_refs:  vec![],
      }
   }

   #[test]
   fn test_summary_from_holistic_analysis_strips_prefix() {
      let config = CommitConfig::default();
      let analysis = test_analysis_with_summary(Some("feat(api): added holistic commit titles"));

      let summary = summary_from_holistic_analysis(&analysis, &config)
         .unwrap()
         .expect("summary should be present");

      assert_eq!(summary.as_str(), "added holistic commit titles");
   }

   #[test]
   fn test_summary_from_holistic_analysis_ignores_blank_summary() {
      let config = CommitConfig::default();
      let analysis = test_analysis_with_summary(Some("   "));

      assert!(
         summary_from_holistic_analysis(&analysis, &config)
            .unwrap()
            .is_none()
      );
   }

   // ========== build_footers Tests ==========

   #[test]
   fn test_build_footers_empty() {
      let args = Args::default();
      let footers = build_footers(&args);
      assert_eq!(footers, Vec::<String>::new());
   }

   #[test]
   fn test_build_footers_cli_fixes() {
      let args = Args { fixes: vec!["123".to_string(), "#456".to_string()], ..Default::default() };
      let footers = build_footers(&args);
      assert_eq!(footers, vec!["Fixes #123", "Fixes #456"]);
   }

   #[test]
   fn test_build_footers_cli_all_types() {
      let args = Args {
         fixes: vec!["1".to_string()],
         closes: vec!["2".to_string()],
         resolves: vec!["3".to_string()],
         refs: vec!["4".to_string()],
         ..Default::default()
      };

      let footers = build_footers(&args);
      assert_eq!(footers, vec!["Fixes #1", "Closes #2", "Resolves #3", "Refs #4"]);
   }

   #[test]
   fn test_build_footers_cli_only() {
      let args = Args { fixes: vec!["123".to_string()], ..Default::default() };
      let footers = build_footers(&args);
      assert_eq!(footers, vec!["Fixes #123"]);
   }

   #[test]
   fn test_build_footers_breaking_change() {
      let args = Args { breaking: true, ..Default::default() };
      let footers = build_footers(&args);
      assert_eq!(footers, vec!["BREAKING CHANGE: This commit introduces breaking changes"]);
   }

   #[test]
   fn test_build_footers_combined() {
      let args = Args {
         fixes: vec!["100".to_string()],
         refs: vec!["200".to_string()],
         breaking: true,
         ..Default::default()
      };

      let footers = build_footers(&args);
      assert_eq!(footers, vec![
         "Fixes #100",
         "Refs #200",
         "BREAKING CHANGE: This commit introduces breaking changes"
      ]);
   }

   #[test]
   fn test_resolve_fast_mode_model_defaults_to_haiku() {
      let args = Args::default();
      let config = CommitConfig::default();

      assert_eq!(resolve_fast_mode_model(&args, &config), "claude-haiku-4-5");
   }

   #[test]
   fn test_resolve_fast_mode_model_uses_legacy_selector() {
      let args = Args::default();
      let config = CommitConfig {
         analysis_model: "gpt-5.3-codex-spark".to_string(),
         legacy_model: Some("gpt-5.3-codex-spark".to_string()),
         ..CommitConfig::default()
      };

      assert_eq!(resolve_fast_mode_model(&args, &config), "gpt-5.3-codex-spark");
   }

   #[test]
   fn test_auto_fast_changed_lines_matches_small_diff() {
      let config = CommitConfig { auto_fast_threshold_lines: 200, ..CommitConfig::default() };
      let numstat = "120\t70\tsrc/main.rs\n-\t-\tlogo.png";

      assert_eq!(auto_fast_changed_lines(numstat, &config), Some(190));
   }

   #[test]
   fn test_auto_fast_changed_lines_skips_large_diff() {
      let config = CommitConfig { auto_fast_threshold_lines: 200, ..CommitConfig::default() };
      let numstat = "120\t90\tsrc/main.rs";

      assert_eq!(auto_fast_changed_lines(numstat, &config), None);
   }

   #[test]
   fn test_auto_fast_changed_lines_can_be_disabled() {
      let config = CommitConfig { auto_fast_threshold_lines: 0, ..CommitConfig::default() };
      let numstat = "10\t5\tsrc/main.rs";

      assert_eq!(auto_fast_changed_lines(numstat, &config), None);
   }
}
