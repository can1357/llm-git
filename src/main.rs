use analysis::extract_scope_candidates;
use api::{fallback_summary, generate_conventional_analysis, generate_summary_from_analysis};
use arboard::Clipboard;
use clap::Parser;
use compose::run_compose_mode;
use config::CommitConfig;
use diff::smart_truncate_diff;
use error::{CommitGenError, Result};
use git::{get_git_diff, get_git_stat, git_commit, git_push};
use llm_git::*;
use normalization::{format_commit_message, post_process_commit_message};
use types::{Args, ConventionalAnalysis, ConventionalCommit, Mode, resolve_model_name};
use validation::{check_type_scope_consistency, validate_commit_message};

/// Apply CLI overrides to config
fn apply_cli_overrides(config: &mut CommitConfig, args: &Args) {
   if let Some(ref model) = args.model {
      config.analysis_model = resolve_model_name(model);
   }
   if let Some(ref summary_model) = args.summary_model {
      config.summary_model = resolve_model_name(summary_model);
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
fn load_config_from_args(args: &Args) -> Result<CommitConfig> {
   if let Some(ref config_path) = args.config {
      CommitConfig::from_file(config_path)
   } else {
      CommitConfig::load()
   }
}

/// Build footers from CLI args and analysis issue refs
///
/// Deduplicates issue refs from analysis that are already in CLI args
fn build_footers(args: &Args, analysis: &ConventionalAnalysis) -> Vec<String> {
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

   // Add issue refs from analysis if not already in CLI args
   for issue_ref in &analysis.issue_refs {
      let normalized_ref = issue_ref.trim_start_matches('#').to_lowercase();
      let already_present = args.fixes.iter().any(|f| {
         f.trim_start_matches('#')
            .eq_ignore_ascii_case(&normalized_ref)
      }) || args.closes.iter().any(|f| {
         f.trim_start_matches('#')
            .eq_ignore_ascii_case(&normalized_ref)
      }) || args.resolves.iter().any(|f| {
         f.trim_start_matches('#')
            .eq_ignore_ascii_case(&normalized_ref)
      }) || args.refs.iter().any(|r| {
         r.trim_start_matches('#')
            .eq_ignore_ascii_case(&normalized_ref)
      });

      if !already_present {
         footers.push(format!("Refs {issue_ref}"));
      }
   }

   // Add breaking change footer if requested
   if args.breaking {
      footers.push("BREAKING CHANGE: This commit introduces breaking changes".to_string());
   }

   footers
}

/// Main generation pipeline: get diff/stat → truncate → analyze → summarize →
/// build commit
fn run_generation(config: &CommitConfig, args: &Args) -> Result<ConventionalCommit> {
   let diff = get_git_diff(&args.mode, args.target.as_deref(), &args.dir, config)?;
   let stat = get_git_stat(&args.mode, args.target.as_deref(), &args.dir, config)?;

   println!("Using analysis model: {} (temp: {})", config.analysis_model, config.temperature);
   println!("Using summary model: {}", config.summary_model);

   // Smart truncation if needed
   let diff = if diff.len() > config.max_diff_length {
      println!("Warning: Applying smart truncation (diff size: {} characters)", diff.len());
      smart_truncate_diff(&diff, config.max_diff_length, config)
   } else {
      diff
   };

   // Generate conventional commit analysis
   println!("Generating conventional commit analysis...");
   let context = if args.context.is_empty() {
      None
   } else {
      Some(args.context.join(" "))
   };
   let (scope_candidates_str, _is_wide) =
      extract_scope_candidates(&args.mode, args.target.as_deref(), &args.dir, config)?;
   let analysis = generate_conventional_analysis(
      &stat,
      &diff,
      &config.analysis_model,
      context.as_deref(),
      &scope_candidates_str,
      config,
   )?;

   // Log scope selection
   if let Some(ref scope) = analysis.scope {
      println!("Selected scope: {scope}");
   } else {
      println!("No scope selected (broad change)");
   }

   println!("Creating summary...");
   let detail_points = analysis.body.clone();
   let summary = match generate_summary_from_analysis(
      &stat,
      analysis.commit_type.as_str(),
      analysis.scope.as_ref().map(|s| s.as_str()),
      &detail_points,
      context.as_deref(),
      config,
   ) {
      Ok(summary) => summary,
      Err(err) => {
         eprintln!("Warning: Failed to create summary with Haiku: {err}");
         fallback_summary(&stat, &detail_points, analysis.commit_type.as_str(), config)
      },
   };

   let footers = build_footers(args, &analysis);

   Ok(ConventionalCommit {
      commit_type: analysis.commit_type,
      scope: analysis.scope,
      summary,
      body: analysis.body,
      footers,
   })
}

/// Post-process, validate, retry with fallback. Returns validation error if any
fn validate_and_process(
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
            ) {
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
                     let message2 = e2.to_string();
                     eprintln!("Validation failed after scope removal: {message2}");
                     validation_error = Some(message2);
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
fn copy_to_clipboard(text: &str) -> Result<()> {
   let mut clipboard = Clipboard::new().map_err(CommitGenError::ClipboardError)?;
   clipboard
      .set_text(text)
      .map_err(CommitGenError::ClipboardError)?;
   Ok(())
}

fn main() -> Result<()> {
   let args = Args::parse();

   // Load config and apply CLI overrides
   let mut config = load_config_from_args(&args)?;
   apply_cli_overrides(&mut config, &args);

   // Route to compose mode if --compose flag is present
   if args.compose {
      return run_compose_mode(&args, &config);
   }

   // Route to rewrite mode if --rewrite flag is present
   if args.rewrite {
      return rewrite::run_rewrite_mode(&args, &config);
   }

   // Auto-stage all changes if nothing staged in commit mode
   if matches!(args.mode, Mode::Staged) {
      use std::process::Command;
      let staged_check = Command::new("git")
         .args(["diff", "--cached", "--quiet"])
         .current_dir(&args.dir)
         .status()
         .map_err(|e| CommitGenError::GitError(format!("Failed to check staged changes: {e}")))?;

      // exit code 1 = changes exist, 0 = no changes
      if staged_check.success() {
         // Check if there are any unstaged changes before staging
         let unstaged_check = Command::new("git")
            .args(["diff", "--quiet"])
            .current_dir(&args.dir)
            .status()
            .map_err(|e| {
               CommitGenError::GitError(format!("Failed to check unstaged changes: {e}"))
            })?;

         // Check for untracked files
         let untracked_output = Command::new("git")
            .args(["ls-files", "--others", "--exclude-standard"])
            .current_dir(&args.dir)
            .output()
            .map_err(|e| {
               CommitGenError::GitError(format!("Failed to check untracked files: {e}"))
            })?;

         let has_untracked = !untracked_output.stdout.is_empty();

         // If no unstaged changes AND no untracked files, working directory is clean
         if unstaged_check.success() && !has_untracked {
            return Err(CommitGenError::NoChanges {
               mode: "working directory (nothing to commit)".to_string(),
            });
         }

         println!("No staged changes, staging all...");
         let add_output = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&args.dir)
            .output()
            .map_err(|e| CommitGenError::GitError(format!("Failed to stage changes: {e}")))?;

         if !add_output.status.success() {
            let stderr = String::from_utf8_lossy(&add_output.stderr);
            return Err(CommitGenError::GitError(format!("git add -A failed: {stderr}")));
         }
      }
   }

   println!("Analyzing {} changes...", match args.mode {
      Mode::Staged => "staged",
      Mode::Commit => "commit",
      Mode::Unstaged => "unstaged",
      Mode::Compose => unreachable!("compose mode handled separately"),
   });

   // Run generation pipeline
   let mut commit_msg = run_generation(&config, &args)?;

   // Get stat and detail points for validation retry
   let stat = get_git_stat(&args.mode, args.target.as_deref(), &args.dir, &config)?;
   let detail_points = commit_msg.body.clone();
   let context = if args.context.is_empty() {
      None
   } else {
      Some(args.context.join(" "))
   };

   // Validate and process
   let validation_failed =
      validate_and_process(&mut commit_msg, &stat, &detail_points, context.as_deref(), &config);

   if let Some(ref err) = validation_failed {
      eprintln!("Warning: Generated message failed validation even after retry: {err}");
      eprintln!("You may want to manually edit the message before committing.");
   }

   // Check type-scope consistency
   check_type_scope_consistency(&commit_msg, &stat);

   // Format and display
   let formatted_message = format_commit_message(&commit_msg);

   println!("\n{}", "=".repeat(60));
   println!("Generated Commit Message:");
   println!("{}", "=".repeat(60));
   println!("{formatted_message}");
   println!("{}", "=".repeat(60));

   if std::env::var("LLM_GIT_VERBOSE").is_ok() {
      println!("\nJSON Structure:");
      println!("{}", serde_json::to_string_pretty(&commit_msg)?);
   }

   // Copy to clipboard if requested
   if args.copy {
      match copy_to_clipboard(&formatted_message) {
         Ok(()) => println!("\n✓ Copied to clipboard"),
         Err(e) => println!("\nNote: Failed to copy to clipboard: {e}"),
      }
   }

   // Auto-commit for staged mode (unless dry-run)
   // Don't commit if validation failed
   if matches!(args.mode, Mode::Staged) {
      if validation_failed.is_some() {
         eprintln!(
            "\n⚠ Skipping commit due to validation failure. Use --dry-run to test or manually \
             commit."
         );
         return Err(CommitGenError::ValidationError(
            "Commit message validation failed".to_string(),
         ));
      }

      println!("\nPreparing to commit...");
      git_commit(&formatted_message, args.dry_run, &args.dir)?;

      // Auto-push if requested (only if not dry-run)
      if args.push && !args.dry_run {
         git_push(&args.dir)?;
      }
   }

   Ok(())
}

#[cfg(test)]
mod tests {
   use types::{CommitType, Scope};

   use super::*;

   // ========== build_footers Tests ==========

   #[test]
   fn test_build_footers_empty() {
      let args = Args::default();

      let analysis = ConventionalAnalysis {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       None,
         body:        vec![],
         issue_refs:  vec![],
      };

      let footers = build_footers(&args, &analysis);
      assert_eq!(footers, Vec::<String>::new());
   }

   #[test]
   fn test_build_footers_cli_fixes() {
      let args = Args { fixes: vec!["123".to_string(), "#456".to_string()], ..Default::default() };

      let analysis = ConventionalAnalysis {
         commit_type: CommitType::new("fix").unwrap(),
         scope:       None,
         body:        vec![],
         issue_refs:  vec![],
      };

      let footers = build_footers(&args, &analysis);
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

      let analysis = ConventionalAnalysis {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       None,
         body:        vec![],
         issue_refs:  vec![],
      };

      let footers = build_footers(&args, &analysis);
      assert_eq!(footers, vec!["Fixes #1", "Closes #2", "Resolves #3", "Refs #4"]);
   }

   #[test]
   fn test_build_footers_analysis_refs_no_duplicates() {
      let args = Args { fixes: vec!["123".to_string()], ..Default::default() };

      let analysis = ConventionalAnalysis {
         commit_type: CommitType::new("fix").unwrap(),
         scope:       None,
         body:        vec![],
         issue_refs:  vec!["#123".to_string(), "#456".to_string()],
      };

      let footers = build_footers(&args, &analysis);
      // #123 is already in CLI fixes, so should only have Fixes #123 and Refs #456
      assert_eq!(footers, vec!["Fixes #123", "Refs #456"]);
   }

   #[test]
   fn test_build_footers_analysis_refs_only() {
      let args = Args::default();

      let analysis = ConventionalAnalysis {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       None,
         body:        vec![],
         issue_refs:  vec!["#789".to_string()],
      };

      let footers = build_footers(&args, &analysis);
      assert_eq!(footers, vec!["Refs #789"]);
   }

   #[test]
   fn test_build_footers_breaking_change() {
      let args = Args { breaking: true, ..Default::default() };

      let analysis = ConventionalAnalysis {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       None,
         body:        vec![],
         issue_refs:  vec![],
      };

      let footers = build_footers(&args, &analysis);
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

      let analysis = ConventionalAnalysis {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       Some(Scope::new("api").unwrap()),
         body:        vec!["detail 1.".to_string()],
         issue_refs:  vec!["#300".to_string()],
      };

      let footers = build_footers(&args, &analysis);
      assert_eq!(footers, vec![
         "Fixes #100",
         "Refs #200",
         "Refs #300",
         "BREAKING CHANGE: This commit introduces breaking changes"
      ]);
   }
}
