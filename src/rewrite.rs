use std::fmt;

use futures::stream::{self, StreamExt};

use crate::{
   analysis::extract_scope_candidates,
   api::{AnalysisContext, generate_conventional_analysis, generate_summary_from_analysis},
   config::CommitConfig,
   diff::smart_truncate_diff,
   error::{CommitGenError, Result},
   git::{
      check_working_tree_clean, create_backup_branch, get_commit_list, get_commit_metadata,
      get_git_diff, get_git_stat, rewrite_history,
   },
   normalization::{format_commit_message, post_process_commit_message},
   style,
   tokens::create_token_counter,
   types::{Args, CommitMetadata, ConventionalCommit, Mode},
   validation::validate_commit_message,
};

/// Run rewrite mode - regenerate all commit messages in history
pub async fn run_rewrite_mode(args: &Args, config: &CommitConfig) -> Result<()> {
   // 1. Validate preconditions
   if !args.rewrite_dry_run
      && args.rewrite_preview.is_none()
      && !check_working_tree_clean(&args.dir)?
   {
      return Err(CommitGenError::Other(
         "Working directory not clean. Commit or stash changes first.".to_string(),
      ));
   }

   // 2. Get commit list
   println!("{} Collecting commits...", style::info("📋"));
   let mut commit_hashes = get_commit_list(args.rewrite_start.as_deref(), &args.dir)?;

   if let Some(n) = args.rewrite_preview {
      commit_hashes.truncate(n);
   }

   println!("Found {} commits to process", style::bold(&commit_hashes.len().to_string()));

   // 3. Extract metadata
   println!("{} Extracting commit metadata...", style::info("🔍"));
   let commits: Vec<CommitMetadata> = commit_hashes
      .iter()
      .enumerate()
      .map(|(i, hash)| {
         if (i + 1) % 50 == 0 {
            eprintln!("  {}/{}...", style::dim(&(i + 1).to_string()), commit_hashes.len());
         }
         get_commit_metadata(hash, &args.dir)
      })
      .collect::<Result<Vec<_>>>()?;

   // 4. Preview mode (no API calls)
   if args.rewrite_dry_run && args.rewrite_preview.is_some() {
      print_preview_list(&commits);
      return Ok(());
   }

   // 5. Generate new messages (parallel)
   println!(
      "{} Converting to conventional commits (parallel={})...\n",
      style::info("🤖"),
      style::bold(&args.rewrite_parallel.to_string())
   );

   // Force exclude_old_message for rewrite mode
   let mut rewrite_config = config.clone();
   rewrite_config.exclude_old_message = true;

   let new_messages = generate_messages_parallel(&commits, &rewrite_config, args).await?;

   // 6. Show results
   print_conversion_results(&commits, &new_messages);

   // 7. Preview or apply
   if args.rewrite_dry_run {
      println!("\n{}", style::section_header("DRY RUN - No changes made", 50));
      println!("Run without --rewrite-dry-run to apply changes");
      return Ok(());
   }

   if args.rewrite_preview.is_some() {
      println!("\nRun without --rewrite-preview to rewrite all history");
      return Ok(());
   }

   // 8. Create backup
   println!("\n{} Creating backup branch...", style::info("💾"));
   let backup = create_backup_branch(&args.dir)?;
   println!("{} Backup: {}", style::success("✓"), style::bold(&backup));

   // 9. Rewrite history
   println!("\n{} Rewriting history...", style::warning("⚠️"));
   rewrite_history(&commits, &new_messages, &args.dir)?;

   println!(
      "\n{} Done! Rewrote {} commits",
      style::success("✅"),
      style::bold(&commits.len().to_string())
   );
   println!("Restore with: {}", style::dim(&format!("git reset --hard {backup}")));

   Ok(())
}

/// Generate new commit messages in parallel using async streams
async fn generate_messages_parallel(
   commits: &[CommitMetadata],
   config: &CommitConfig,
   args: &Args,
) -> Result<Vec<String>> {
   let mut results = vec![String::new(); commits.len()];
   let mut errors = Vec::new();

   let outputs: Vec<(usize, std::result::Result<String, CommitGenError>)> =
      stream::iter(commits.iter().enumerate())
         .map(|(idx, commit)| async move {
            (idx, generate_for_commit(commit, config, &args.dir).await)
         })
         .buffer_unordered(args.rewrite_parallel)
         .collect()
         .await;

   for (idx, result) in outputs {
      match result {
         Ok(new_msg) => {
            let old = commits[idx].message.lines().next().unwrap_or("");
            let new = new_msg.lines().next().unwrap_or("");
            println!(
               "[{:3}/{:3}] {}",
               idx + 1,
               commits.len(),
               style::dim(&commits[idx].hash[..8])
            );
            println!(
               "  {} {}",
               style::error("-"),
               style::dim(&TruncStr(old, 60).to_string())
            );
            println!("  {} {}", style::success("+"), TruncStr(new, 60));
            println!();
            results[idx].clone_from(&new_msg);
         },
         Err(e) => {
            eprintln!(
               "[{:3}/{:3}] {} {} {}",
               idx + 1,
               commits.len(),
               style::dim(&commits[idx].hash[..8]),
               style::error("❌ ERROR:"),
               e
            );
            results[idx].clone_from(&commits[idx].message);
            errors.push((idx, e.to_string()));
         },
      }
   }

   if !errors.is_empty() {
      eprintln!(
         "\n{} {} commits failed, kept original messages",
         style::warning("⚠\u{fe0f}"),
         style::bold(&errors.len().to_string())
      );
   }

   Ok(results)
}

/// Generate conventional commit message for a single commit
async fn generate_for_commit(
   commit: &CommitMetadata,
   config: &CommitConfig,
   dir: &str,
) -> Result<String> {
   let token_counter = create_token_counter(config);
   // rewrite)
   let diff = get_git_diff(&Mode::Commit, Some(&commit.hash), dir, config)?;
   let stat = get_git_stat(&Mode::Commit, Some(&commit.hash), dir, config)?;
   let diff = if diff.len() > config.max_diff_length {
      smart_truncate_diff(&diff, config.max_diff_length, config, &token_counter)
   } else {
      diff
   };
   // Extract scope candidates
   let (scope_candidates_str, _) =
      extract_scope_candidates(&Mode::Commit, Some(&commit.hash), dir, config)?;
   let ctx = AnalysisContext {
      user_context:    None, // No user context for bulk rewrite
      recent_commits:  None, // No recent commits for rewrite mode
      common_scopes:   None, // No common scopes for rewrite mode
      project_context: None, // No project context for rewrite mode
      debug_output:    None,
      debug_prefix:    None,
   };
   let analysis = generate_conventional_analysis(
      &stat,
      &diff,
      &config.model,
      &scope_candidates_str,
      &ctx,
      config,
   )
   .await?;

   // Phase 2: Summary
   let body_texts = analysis.body_texts();
   let summary = generate_summary_from_analysis(
      &stat,
      analysis.commit_type.as_str(),
      analysis.scope.as_ref().map(|s| s.as_str()),
      &body_texts,
      None, // No user context in rewrite mode
      config,
      None,
      None,
   )
   .await?;
   // Build ConventionalCommit
   // Issue refs are now inlined in body items, so footers are empty (unless added
   // by CLI)
   let mut commit_msg = ConventionalCommit {
      commit_type: analysis.commit_type,
      scope: analysis.scope,
      summary,
      body: body_texts,
      footers: vec![], // Issue refs are inlined in body items now
   };

   // Post-process and validate
   post_process_commit_message(&mut commit_msg, config);
   validate_commit_message(&commit_msg, config)?;

   // Format final message
   Ok(format_commit_message(&commit_msg))
}

/// Print preview list of commits (no API calls)
fn print_preview_list(commits: &[CommitMetadata]) {
   println!(
      "\n{}\n",
      style::section_header(
         &format!("PREVIEW - Showing {} commits (no API calls)", commits.len()),
         70
      )
   );

   for (i, commit) in commits.iter().enumerate() {
      let summary = commit
         .message
         .lines()
         .next()
         .unwrap_or("")
         .chars()
         .take(70)
         .collect::<String>();

      println!("[{:3}] {} - {}", i + 1, style::dim(&commit.hash[..8]), summary);
   }

   println!("\n{}", style::dim("Run without --rewrite-preview to regenerate commits"));
}

/// Print conversion results comparison
fn print_conversion_results(commits: &[CommitMetadata], new_messages: &[String]) {
   println!(
      "\n{} Processed {} commits\n",
      style::success("✓"),
      style::bold(&commits.len().to_string())
   );

   // Show first 3 examples
   let show_count = 3.min(commits.len());
   if show_count > 0 {
      println!("{}\n", style::section_header("Sample conversions", 50));
      for i in 0..show_count {
         let old = commits[i].message.lines().next().unwrap_or("");
         let new = new_messages[i].lines().next().unwrap_or("");

         println!("[{}] {}", i + 1, style::dim(&commits[i].hash[..8]));
         println!("  {} {}", style::error("-"), style::dim(&TruncStr(old, 70).to_string()));
         println!("  {} {}", style::success("+"), TruncStr(new, 70));
         println!();
      }
   }
}

struct TruncStr<'a>(&'a str, usize);

impl fmt::Display for TruncStr<'_> {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      if self.0.len() <= self.1 {
         f.write_str(self.0)
      } else {
         let n = self.0.floor_char_boundary(self.1);
         f.write_str(&self.0[..n])?;
         f.write_str("...")
      }
   }
}
