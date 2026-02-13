use std::{collections::HashMap, path::PathBuf, process::Command};

pub use self::git_push as push;
use crate::{
   config::CommitConfig,
   error::{CommitGenError, Result},
   style,
   types::{CommitMetadata, Mode},
};

/// Detect a stale `index.lock` from git stderr and return a
/// [`CommitGenError::GitIndexLocked`] with the resolved path if found.
fn check_index_lock(stderr: &str, dir: &str) -> Option<CommitGenError> {
   if !stderr.contains("index.lock") {
      return None;
   }

   // Try to extract the exact lock path from the error message.
   // Git says: "Unable to create '/path/to/.git/index.lock': File exists."
   let lock_path = stderr
      .lines()
      .find_map(|line| {
         let start = line.find('\'')?;
         let end = line[start + 1..].find('\'')?;
         let path = &line[start + 1..start + 1 + end];
         if path.ends_with("index.lock") {
            Some(PathBuf::from(path))
         } else {
            None
         }
      })
      .unwrap_or_else(|| PathBuf::from(dir).join(".git/index.lock"));

   Some(CommitGenError::GitIndexLocked { lock_path })
}

/// Ensure the provided directory is inside a git work tree.
///
/// # Errors
/// Returns an error when the directory is not part of a git repository.
pub fn ensure_git_repo(dir: &str) -> Result<()> {
   let output = Command::new("git")
      .args(["rev-parse", "--show-toplevel"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git rev-parse: {e}")))?;

   if output.status.success() {
      return Ok(());
   }

   let stderr = String::from_utf8_lossy(&output.stderr);
   if stderr.contains("not a git repository") {
      return Err(CommitGenError::git(
         "Not a git repository (or any of the parent directories): .git".to_string(),
      ));
   }

   Err(CommitGenError::git(format!("Failed to detect git repository: {stderr}")))
}

/// Get git diff based on the specified mode
pub fn get_git_diff(
   mode: &Mode,
   target: Option<&str>,
   dir: &str,
   config: &CommitConfig,
) -> Result<String> {
   let output = match mode {
      Mode::Staged => Command::new("git")
         .args(["diff", "--cached"])
         .current_dir(dir)
         .output()
         .map_err(|e| CommitGenError::git(format!("Failed to run git diff --cached: {e}")))?,
      Mode::Commit => {
         let target = target.ok_or_else(|| {
            CommitGenError::ValidationError("--target required for commit mode".to_string())
         })?;
         let mut cmd = Command::new("git");
         cmd.arg("show");
         if config.exclude_old_message {
            cmd.arg("--format=");
         }
         cmd.arg(target)
            .current_dir(dir)
            .output()
            .map_err(|e| CommitGenError::git(format!("Failed to run git show: {e}")))?
      },
      Mode::Unstaged => {
         // Get diff for tracked files
         let tracked_output = Command::new("git")
            .args(["diff"])
            .current_dir(dir)
            .output()
            .map_err(|e| CommitGenError::git(format!("Failed to run git diff: {e}")))?;

         if !tracked_output.status.success() {
            let stderr = String::from_utf8_lossy(&tracked_output.stderr);
            return Err(CommitGenError::git(format!("git diff failed: {stderr}")));
         }

         let tracked_diff = String::from_utf8_lossy(&tracked_output.stdout).to_string();

         // Get untracked files
         let untracked_output = Command::new("git")
            .args(["ls-files", "--others", "--exclude-standard"])
            .current_dir(dir)
            .output()
            .map_err(|e| CommitGenError::git(format!("Failed to list untracked files: {e}")))?;

         if !untracked_output.status.success() {
            let stderr = String::from_utf8_lossy(&untracked_output.stderr);
            return Err(CommitGenError::git(format!("git ls-files failed: {stderr}")));
         }

         let untracked_list = String::from_utf8_lossy(&untracked_output.stdout);
         let untracked_files: Vec<&str> =
            untracked_list.lines().filter(|s| !s.is_empty()).collect();

         if untracked_files.is_empty() {
            return Ok(tracked_diff);
         }

         // Generate diffs for untracked files using git diff /dev/null
         let mut combined_diff = tracked_diff;
         for file in untracked_files {
            let file_diff_output = Command::new("git")
               .args(["diff", "--no-index", "/dev/null", file])
               .current_dir(dir)
               .output()
               .map_err(|e| {
                  CommitGenError::git(format!("Failed to diff untracked file {file}: {e}"))
               })?;

            // git diff --no-index exits with 1 when files differ (expected)
            if file_diff_output.status.success() || file_diff_output.status.code() == Some(1) {
               let file_diff = String::from_utf8_lossy(&file_diff_output.stdout);
               // Rewrite the diff header to match standard git format
               let lines: Vec<&str> = file_diff.lines().collect();
               if lines.len() >= 2 {
                  use std::fmt::Write;
                  if !combined_diff.is_empty() {
                     combined_diff.push('\n');
                  }
                  writeln!(combined_diff, "diff --git a/{file} b/{file}").unwrap();
                  combined_diff.push_str("new file mode 100644\n");
                  combined_diff.push_str("index 0000000..0000000\n");
                  combined_diff.push_str("--- /dev/null\n");
                  writeln!(combined_diff, "+++ b/{file}").unwrap();
                  // Skip first 2 lines (---/+++ from --no-index) and copy rest
                  for line in lines.iter().skip(2) {
                     combined_diff.push_str(line);
                     combined_diff.push('\n');
                  }
               }
            }
         }

         return Ok(combined_diff);
      },
      Mode::Compose => unreachable!("compose mode handled separately"),
   };

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("Git command failed: {stderr}")));
   }

   let diff = String::from_utf8_lossy(&output.stdout).to_string();

   if diff.trim().is_empty() {
      let mode_str = match mode {
         Mode::Staged => "staged",
         Mode::Commit => "commit",
         Mode::Unstaged => "unstaged",
         Mode::Compose => "compose",
      };
      return Err(CommitGenError::NoChanges { mode: mode_str.to_string() });
   }

   Ok(diff)
}

/// Get git diff --stat to show file-level changes summary
pub fn get_git_stat(
   mode: &Mode,
   target: Option<&str>,
   dir: &str,
   config: &CommitConfig,
) -> Result<String> {
   let output = match mode {
      Mode::Staged => Command::new("git")
         .args(["diff", "--cached", "--stat"])
         .current_dir(dir)
         .output()
         .map_err(|e| {
            CommitGenError::git(format!("Failed to run git diff --cached --stat: {e}"))
         })?,
      Mode::Commit => {
         let target = target.ok_or_else(|| {
            CommitGenError::ValidationError("--target required for commit mode".to_string())
         })?;
         let mut cmd = Command::new("git");
         cmd.arg("show");
         if config.exclude_old_message {
            cmd.arg("--format=");
         }
         cmd.arg("--stat")
            .arg(target)
            .current_dir(dir)
            .output()
            .map_err(|e| CommitGenError::git(format!("Failed to run git show --stat: {e}")))?
      },
      Mode::Unstaged => {
         // Get stat for tracked files
         let tracked_output = Command::new("git")
            .args(["diff", "--stat"])
            .current_dir(dir)
            .output()
            .map_err(|e| CommitGenError::git(format!("Failed to run git diff --stat: {e}")))?;

         if !tracked_output.status.success() {
            let stderr = String::from_utf8_lossy(&tracked_output.stderr);
            return Err(CommitGenError::git(format!("git diff --stat failed: {stderr}")));
         }

         let mut stat = String::from_utf8_lossy(&tracked_output.stdout).to_string();

         // Get untracked files and append to stat
         let untracked_output = Command::new("git")
            .args(["ls-files", "--others", "--exclude-standard"])
            .current_dir(dir)
            .output()
            .map_err(|e| CommitGenError::git(format!("Failed to list untracked files: {e}")))?;

         if !untracked_output.status.success() {
            let stderr = String::from_utf8_lossy(&untracked_output.stderr);
            return Err(CommitGenError::git(format!("git ls-files failed: {stderr}")));
         }

         let untracked_list = String::from_utf8_lossy(&untracked_output.stdout);
         let untracked_files: Vec<&str> =
            untracked_list.lines().filter(|s| !s.is_empty()).collect();

         if !untracked_files.is_empty() {
            use std::fmt::Write;
            for file in untracked_files {
               use std::fs;
               if let Ok(metadata) = fs::metadata(format!("{dir}/{file}")) {
                  let lines = if metadata.is_file() {
                     fs::read_to_string(format!("{dir}/{file}"))
                        .map(|content| content.lines().count())
                        .unwrap_or(0)
                  } else {
                     0
                  };
                  if !stat.is_empty() && !stat.ends_with('\n') {
                     stat.push('\n');
                  }
                  writeln!(stat, " {file} | {lines} {}", "+".repeat(lines.min(50))).unwrap();
               }
            }
         }

         return Ok(stat);
      },
      Mode::Compose => unreachable!("compose mode handled separately"),
   };

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("Git stat command failed: {stderr}")));
   }

   Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Execute git commit with the given message
#[allow(clippy::fn_params_excessive_bools, reason = "commit flags are naturally boolean")]
pub fn git_commit(
   message: &str,
   dry_run: bool,
   dir: &str,
   sign: bool,
   signoff: bool,
   skip_hooks: bool,
) -> Result<()> {
   if dry_run {
      let sign_flag = if sign { " -S" } else { "" };
      let signoff_flag = if signoff { " -s" } else { "" };
      let hooks_flag = if skip_hooks { " --no-verify" } else { "" };
      let command = format!(
         "git commit{sign_flag}{signoff_flag}{hooks_flag} -m \"{}\"",
         message.replace('\n', "\\n")
      );
      println!("\n{}", style::boxed_message("DRY RUN", &command, 60));
      return Ok(());
   }

   let mut args = vec!["commit"];
   if sign {
      args.push("-S");
   }
   if signoff {
      args.push("-s");
   }
   if skip_hooks {
      args.push("--no-verify");
   }
   args.push("-m");
   args.push(message);

   let output = Command::new("git")
      .args(&args)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git commit: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      let stdout = String::from_utf8_lossy(&output.stdout);
      if let Some(err) = check_index_lock(&stderr, dir) {
         return Err(err);
      }
      return Err(CommitGenError::git(format!("git commit failed: {stderr}{stdout}")));
   }

   let stdout = String::from_utf8_lossy(&output.stdout);
   println!("\n{stdout}");
   println!(
      "{} {}",
      style::success(style::icons::SUCCESS),
      style::success("Successfully committed!")
   );

   Ok(())
}

/// Execute git push
pub fn git_push(dir: &str) -> Result<()> {
   println!("\n{}", style::info("Pushing changes..."));

   let output = Command::new("git")
      .args(["push"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git push: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      let stdout = String::from_utf8_lossy(&output.stdout);
      return Err(CommitGenError::git(format!(
         "Git push failed:\nstderr: {stderr}\nstdout: {stdout}"
      )));
   }

   let stdout = String::from_utf8_lossy(&output.stdout);
   let stderr = String::from_utf8_lossy(&output.stderr);
   if !stdout.is_empty() {
      println!("{stdout}");
   }
   if !stderr.is_empty() {
      println!("{stderr}");
   }
   println!("{} {}", style::success(style::icons::SUCCESS), style::success("Successfully pushed!"));

   Ok(())
}

/// Get the current HEAD commit hash
pub fn get_head_hash(dir: &str) -> Result<String> {
   let output = Command::new("git")
      .args(["rev-parse", "HEAD"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to get HEAD hash: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git rev-parse HEAD failed: {stderr}")));
   }

   Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// === History Rewrite Operations ===

/// Get list of commit hashes to rewrite (in chronological order)
pub fn get_commit_list(start_ref: Option<&str>, dir: &str) -> Result<Vec<String>> {
   let mut args = vec!["rev-list", "--reverse"];
   let range;
   if let Some(start) = start_ref {
      range = format!("{start}..HEAD");
      args.push(&range);
   } else {
      args.push("HEAD");
   }

   let output = Command::new("git")
      .args(&args)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git rev-list: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git rev-list failed: {stderr}")));
   }

   let stdout = String::from_utf8_lossy(&output.stdout);
   Ok(stdout.lines().map(|s| s.to_string()).collect())
}

/// Extract complete metadata for a commit (for rewriting)
pub fn get_commit_metadata(hash: &str, dir: &str) -> Result<CommitMetadata> {
   // Format: author_name\0author_email\0author_date\0committer_name\
   // 0committer_email\0committer_date\0message
   let format_str = "%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B";

   let info_output = Command::new("git")
      .args(["show", "-s", &format!("--format={format_str}"), hash])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git show: {e}")))?;

   if !info_output.status.success() {
      let stderr = String::from_utf8_lossy(&info_output.stderr);
      return Err(CommitGenError::git(format!("git show failed for {hash}: {stderr}")));
   }

   let info = String::from_utf8_lossy(&info_output.stdout);
   let parts: Vec<&str> = info.splitn(7, '\0').collect();

   if parts.len() < 7 {
      return Err(CommitGenError::git(format!("Failed to parse commit metadata for {hash}")));
   }

   // Get tree hash
   let tree_output = Command::new("git")
      .args(["rev-parse", &format!("{hash}^{{tree}}")])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to get tree hash: {e}")))?;
   let tree_hash = String::from_utf8_lossy(&tree_output.stdout)
      .trim()
      .to_string();

   // Get parent hashes
   let parents_output = Command::new("git")
      .args(["rev-list", "--parents", "-n", "1", hash])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to get parent hashes: {e}")))?;
   let parents_line = String::from_utf8_lossy(&parents_output.stdout);
   let parent_hashes: Vec<String> = parents_line
      .split_whitespace()
      .skip(1) // First is the commit itself
      .map(|s| s.to_string())
      .collect();

   Ok(CommitMetadata {
      hash: hash.to_string(),
      author_name: parts[0].to_string(),
      author_email: parts[1].to_string(),
      author_date: parts[2].to_string(),
      committer_name: parts[3].to_string(),
      committer_email: parts[4].to_string(),
      committer_date: parts[5].to_string(),
      message: parts[6].trim().to_string(),
      parent_hashes,
      tree_hash,
   })
}

/// Check if working directory is clean
pub fn check_working_tree_clean(dir: &str) -> Result<bool> {
   let output = Command::new("git")
      .args(["status", "--porcelain"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to check working tree: {e}")))?;

   Ok(output.stdout.is_empty())
}

/// Create timestamped backup branch
pub fn create_backup_branch(dir: &str) -> Result<String> {
   use chrono::Local;

   let timestamp = Local::now().format("%Y%m%d-%H%M%S");
   let backup_name = format!("backup-rewrite-{timestamp}");

   let output = Command::new("git")
      .args(["branch", &backup_name])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to create backup branch: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git branch failed: {stderr}")));
   }

   Ok(backup_name)
}

/// Get recent commit messages for style consistency (last N commits)
pub fn get_recent_commits(dir: &str, count: usize) -> Result<Vec<String>> {
   let output = Command::new("git")
      .args(["log", &format!("-{count}"), "--pretty=format:%s"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git log: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git log failed: {stderr}")));
   }

   let stdout = String::from_utf8_lossy(&output.stdout);
   Ok(stdout.lines().map(|s| s.to_string()).collect())
}

/// Extract common scopes from git history by parsing commit messages
pub fn get_common_scopes(dir: &str, limit: usize) -> Result<Vec<(String, usize)>> {
   let output = Command::new("git")
      .args(["log", &format!("-{limit}"), "--pretty=format:%s"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git log: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git log failed: {stderr}")));
   }

   let stdout = String::from_utf8_lossy(&output.stdout);
   let mut scope_counts: HashMap<String, usize> = HashMap::new();

   // Parse conventional commit format: type(scope): message
   for line in stdout.lines() {
      if let Some(scope) = extract_scope_from_commit(line) {
         *scope_counts.entry(scope).or_insert(0) += 1;
      }
   }

   // Sort by frequency (descending)
   let mut scopes: Vec<(String, usize)> = scope_counts.into_iter().collect();
   scopes.sort_by(|a, b| b.1.cmp(&a.1));

   Ok(scopes)
}

/// Extract scope from a conventional commit message
fn extract_scope_from_commit(commit_msg: &str) -> Option<String> {
   // Match pattern: type(scope): message
   let parts: Vec<&str> = commit_msg.splitn(2, ':').collect();
   if parts.len() < 2 {
      return None;
   }

   let prefix = parts[0];
   if let Some(scope_start) = prefix.find('(')
      && let Some(scope_end) = prefix.find(')')
      && scope_start < scope_end
   {
      return Some(prefix[scope_start + 1..scope_end].to_string());
   }

   None
}

/// Quantified style patterns extracted from commit history
#[derive(Debug, Clone)]
pub struct StylePatterns {
   /// Percentage of commits using scopes (0.0-100.0)
   pub scope_usage_pct: f32,
   /// Common verbs with counts (sorted by count descending)
   pub common_verbs:    Vec<(String, usize)>,
   /// Average summary length in chars
   pub avg_length:      usize,
   /// Summary length range (min, max)
   pub length_range:    (usize, usize),
   /// Percentage of commits starting with lowercase (0.0-100.0)
   pub lowercase_pct:   f32,
   /// Top scopes with counts (sorted by count descending)
   pub top_scopes:      Vec<(String, usize)>,
}

impl StylePatterns {
   /// Format patterns for prompt injection
   pub fn format_for_prompt(&self) -> String {
      let mut lines = Vec::new();

      lines.push(format!("Scope usage: {:.0}% of commits use scopes", self.scope_usage_pct));

      if !self.common_verbs.is_empty() {
         let verbs: Vec<_> = self
            .common_verbs
            .iter()
            .take(5)
            .map(|(v, c)| format!("{v} ({c})"))
            .collect();
         lines.push(format!("Common verbs: {}", verbs.join(", ")));
      }

      lines.push(format!(
         "Average length: {} chars (range: {}-{})",
         self.avg_length, self.length_range.0, self.length_range.1
      ));

      lines.push(format!("Capitalization: {:.0}% start lowercase", self.lowercase_pct));

      if !self.top_scopes.is_empty() {
         let scopes: Vec<_> = self
            .top_scopes
            .iter()
            .take(5)
            .map(|(s, c)| format!("{s} ({c})"))
            .collect();
         lines.push(format!("Top scopes: {}", scopes.join(", ")));
      }

      lines.join("\n")
   }
}

/// Extract style patterns from commit history
pub fn extract_style_patterns(commits: &[String]) -> Option<StylePatterns> {
   if commits.is_empty() {
      return None;
   }

   let mut scope_count = 0;
   let mut lowercase_count = 0;
   let mut verb_counts: HashMap<String, usize> = HashMap::new();
   let mut scope_counts: HashMap<String, usize> = HashMap::new();
   let mut lengths = Vec::new();

   for commit in commits {
      // Parse: type(scope): summary
      if let Some(colon_pos) = commit.find(':') {
         let prefix = &commit[..colon_pos];
         let summary = commit[colon_pos + 1..].trim();

         // Check for scope
         if let Some(paren_start) = prefix.find('(')
            && let Some(paren_end) = prefix.find(')')
         {
            scope_count += 1;
            let scope = &prefix[paren_start + 1..paren_end];
            *scope_counts.entry(scope.to_string()).or_insert(0) += 1;
         }

         // Check capitalization of summary
         if let Some(first_char) = summary.chars().next() {
            if first_char.is_lowercase() {
               lowercase_count += 1;
            }

            // Extract first word as verb
            let first_word = summary.split_whitespace().next().unwrap_or("");
            if !first_word.is_empty() {
               *verb_counts.entry(first_word.to_lowercase()).or_insert(0) += 1;
            }
         }

         lengths.push(summary.len());
      }
   }

   let total = commits.len();
   let scope_usage_pct = (scope_count as f32 / total as f32) * 100.0;
   let lowercase_pct = (lowercase_count as f32 / total as f32) * 100.0;

   // Sort verbs by count
   let mut common_verbs: Vec<_> = verb_counts.into_iter().collect();
   common_verbs.sort_by(|a, b| b.1.cmp(&a.1));

   // Sort scopes by count
   let mut top_scopes: Vec<_> = scope_counts.into_iter().collect();
   top_scopes.sort_by(|a, b| b.1.cmp(&a.1));

   // Calculate length stats
   let avg_length = if lengths.is_empty() {
      0
   } else {
      lengths.iter().sum::<usize>() / lengths.len()
   };
   let length_range = if lengths.is_empty() {
      (0, 0)
   } else {
      (*lengths.iter().min().unwrap_or(&0), *lengths.iter().max().unwrap_or(&0))
   };

   Some(StylePatterns {
      scope_usage_pct,
      common_verbs,
      avg_length,
      length_range,
      lowercase_pct,
      top_scopes,
   })
}

/// Rewrite git history with new commit messages
pub fn rewrite_history(
   commits: &[CommitMetadata],
   new_messages: &[String],
   dir: &str,
) -> Result<()> {
   if commits.len() != new_messages.len() {
      return Err(CommitGenError::Other("Commit count mismatch".to_string()));
   }

   // Get current branch
   let branch_output = Command::new("git")
      .args(["rev-parse", "--abbrev-ref", "HEAD"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to get current branch: {e}")))?;
   let current_branch = String::from_utf8_lossy(&branch_output.stdout)
      .trim()
      .to_string();

   // Map old commit hashes to new ones
   let mut parent_map: HashMap<String, String> = HashMap::new();
   let mut new_head: Option<String> = None;

   for (idx, (commit, new_msg)) in commits.iter().zip(new_messages.iter()).enumerate() {
      // Map old parents to new parents
      let new_parents: Vec<String> = commit
         .parent_hashes
         .iter()
         .map(|old_parent| {
            parent_map
               .get(old_parent)
               .cloned()
               .unwrap_or_else(|| old_parent.clone())
         })
         .collect();

      // Build commit-tree command
      let mut cmd = Command::new("git");
      cmd.arg("commit-tree")
         .arg(&commit.tree_hash)
         .arg("-m")
         .arg(new_msg)
         .current_dir(dir);

      for parent in &new_parents {
         cmd.arg("-p").arg(parent);
      }

      // Preserve original author/committer metadata
      cmd.env("GIT_AUTHOR_NAME", &commit.author_name)
         .env("GIT_AUTHOR_EMAIL", &commit.author_email)
         .env("GIT_AUTHOR_DATE", &commit.author_date)
         .env("GIT_COMMITTER_NAME", &commit.committer_name)
         .env("GIT_COMMITTER_EMAIL", &commit.committer_email)
         .env("GIT_COMMITTER_DATE", &commit.committer_date);

      let output = cmd
         .output()
         .map_err(|e| CommitGenError::git(format!("Failed to run git commit-tree: {e}")))?;

      if !output.status.success() {
         let stderr = String::from_utf8_lossy(&output.stderr);
         return Err(CommitGenError::git(format!(
            "commit-tree failed for {}: {}",
            commit.hash, stderr
         )));
      }

      let new_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();

      parent_map.insert(commit.hash.clone(), new_hash.clone());
      new_head = Some(new_hash);

      // Progress reporting
      if (idx + 1) % 50 == 0 {
         eprintln!("  Rewrote {}/{} commits...", idx + 1, commits.len());
      }
   }

   // Update branch to new head
   if let Some(head) = new_head {
      let update_output = Command::new("git")
         .args(["update-ref", &format!("refs/heads/{current_branch}"), &head])
         .current_dir(dir)
         .output()
         .map_err(|e| CommitGenError::git(format!("Failed to update ref: {e}")))?;

      if !update_output.status.success() {
         let stderr = String::from_utf8_lossy(&update_output.stderr);
         return Err(CommitGenError::git(format!("git update-ref failed: {stderr}")));
      }

      let reset_output = Command::new("git")
         .args(["reset", "--hard", &head])
         .current_dir(dir)
         .output()
         .map_err(|e| CommitGenError::git(format!("Failed to reset: {e}")))?;

      if !reset_output.status.success() {
         let stderr = String::from_utf8_lossy(&reset_output.stderr);
         return Err(CommitGenError::git(format!("git reset failed: {stderr}")));
      }
   }

   Ok(())
}
