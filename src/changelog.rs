//! Changelog maintenance for git commits
//!
//! This module auto-detects CHANGELOG.md files and generates entries
//! for staged changes, grouped by changelog boundary.
//!
//! Uses a single LLM call per changelog that sees existing entries
//! for style matching and deduplication.

use std::{
   collections::HashMap,
   path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
   api::{OneShotSpec, run_oneshot, strict_json_schema},
   config::CommitConfig,
   diff::smart_truncate_diff,
   error::{CommitGenError, Result},
   git::git_command,
   patch::stage_files,
   templates,
   tokens::create_token_counter,
   types::{ChangelogBoundary, ChangelogCategory, UnreleasedSection},
};

/// Response from the changelog generation LLM call
#[derive(Debug, Deserialize, Serialize)]
struct ChangelogResponse {
   entries: HashMap<String, Vec<String>>,
}
fn normalize_changelog_entry(entry: &str) -> Option<String> {
   let trimmed = entry.trim();
   let without_bullet = trimmed
      .strip_prefix("- ")
      .or_else(|| trimmed.strip_prefix("* "))
      .unwrap_or(trimmed)
      .trim();

   (!without_bullet.is_empty()).then(|| format!("- {without_bullet}"))
}

/// Run the changelog maintenance flow
///
/// 1. Get staged files (excluding CHANGELOG.md files)
/// 2. Detect changelog boundaries
/// 3. For each boundary: generate entries via LLM, write to changelog
/// 4. Stage modified changelogs
pub async fn run_changelog_flow(args: &crate::types::Args, config: &CommitConfig) -> Result<()> {
   let token_counter = create_token_counter(config);

   // Get list of staged files
   let staged_files = get_staged_files(&args.dir)?;
   if staged_files.is_empty() {
      return Ok(());
   }

   // Filter out CHANGELOG.md files (don't analyze changelog changes as changes)
   let non_changelog_files: Vec<_> = staged_files
      .iter()
      .filter(|f| !f.to_lowercase().ends_with("changelog.md"))
      .cloned()
      .collect();

   if non_changelog_files.is_empty() {
      return Ok(());
   }

   // Find all changelogs in repo
   let changelogs = find_changelogs(&args.dir)?;
   if changelogs.is_empty() {
      // No changelogs found, skip silently
      return Ok(());
   }

   // Detect boundaries
   let boundaries = detect_boundaries(&non_changelog_files, &changelogs, &args.dir);
   if boundaries.is_empty() {
      return Ok(());
   }

   println!("{}", crate::style::info(&format!("Updating {} changelog(s)...", boundaries.len())));

   let mut untracked_changelogs = Vec::new();

   for boundary in boundaries {
      // Get diff and stat for this boundary's files
      let diff = get_diff_for_files(&boundary.files, &args.dir)?;
      let stat = get_stat_for_files(&boundary.files, &args.dir)?;

      if diff.is_empty() {
         continue;
      }

      // Truncate if needed
      let diff = if diff.len() > config.max_diff_length {
         smart_truncate_diff(&diff, config.max_diff_length, config, &token_counter)
      } else {
         diff
      };
      // The staged (index) copy is what the commit will contain. Generating
      // and staging against it keeps unrelated unstaged edits to the
      // changelog out of the commit.
      let rel_path = boundary
         .changelog_path
         .strip_prefix(&args.dir)
         .unwrap_or(&boundary.changelog_path)
         .to_string_lossy()
         .to_string();
      let staged_content = staged_changelog_content(&rel_path, &args.dir)?;

      let worktree_content = std::fs::read_to_string(&boundary.changelog_path).map_err(|e| {
         CommitGenError::ChangelogParseError {
            path:   boundary.changelog_path.display().to_string(),
            reason: e.to_string(),
         }
      })?;

      let (changelog_content, is_tracked) = match &staged_content {
         Some(content) => (content.as_str(), true),
         None => (worktree_content.as_str(), false),
      };

      let unreleased = match parse_unreleased_section(changelog_content, &boundary.changelog_path) {
         Ok(u) => u,
         Err(CommitGenError::NoUnreleasedSection { path }) => {
            eprintln!(
               "{} No [Unreleased] section in {}, skipping changelog update",
               crate::style::icons::WARNING,
               path
            );
            continue;
         },
         Err(e) => return Err(e),
      };

      // A tracked changelog whose worktree copy diverged gets the entries
      // inserted into both copies independently.
      let worktree_unreleased = if is_tracked && worktree_content != changelog_content {
         match parse_unreleased_section(&worktree_content, &boundary.changelog_path) {
            Ok(u) => Some(u),
            Err(CommitGenError::NoUnreleasedSection { path }) => {
               eprintln!(
                  "{} No [Unreleased] section in worktree copy of {}, skipping changelog update",
                  crate::style::icons::WARNING,
                  path
               );
               continue;
            },
            Err(e) => return Err(e),
         }
      } else {
         None
      };

      // Check if this is a package-scoped changelog (not root)
      let is_package_changelog = boundary
         .changelog_path
         .parent()
         .is_some_and(|p| p != Path::new(&args.dir) && p != Path::new("."));

      // Format existing entries for LLM context
      let existing_entries = format_existing_entries(&unreleased);

      // Generate entries via LLM
      let new_entries = match generate_changelog_entries(
         &boundary.changelog_path,
         is_package_changelog,
         &stat,
         &diff,
         existing_entries.as_deref(),
         config,
      )
      .await
      {
         Ok(entries) => entries,
         Err(e) => {
            eprintln!(
               "{}",
               crate::style::warning(&format!("Failed to generate changelog entries: {e}"))
            );
            continue;
         },
      };

      if new_entries.is_empty() {
         continue;
      }

      // Save changelog debug output if requested
      if let Some(debug_dir) = &args.debug_output {
         let _ = std::fs::create_dir_all(debug_dir);
         let changelog_json: HashMap<String, Vec<String>> = new_entries
            .iter()
            .map(|(cat, entries)| (cat.as_str().to_string(), entries.clone()))
            .collect();
         if let Ok(json_str) = serde_json::to_string_pretty(&changelog_json) {
            let _ = std::fs::write(debug_dir.join("changelog.json"), json_str);
         }
      }

      // Write entries to both copies: the staged copy is pinned directly into
      // the index, the worktree copy keeps the user's unrelated edits.
      let updated_staged = write_entries(changelog_content, &unreleased, &new_entries);
      let updated_worktree = match &worktree_unreleased {
         Some(worktree_section) => write_entries(&worktree_content, worktree_section, &new_entries),
         None => updated_staged.clone(),
      };
      std::fs::write(&boundary.changelog_path, updated_worktree).map_err(|e| {
         CommitGenError::ChangelogParseError {
            path:   boundary.changelog_path.display().to_string(),
            reason: format!("Failed to write: {e}"),
         }
      })?;

      if is_tracked {
         stage_changelog_blob(&rel_path, &updated_staged, &args.dir)?;
      } else {
         untracked_changelogs.push(boundary.changelog_path.display().to_string());
      }

      let entry_count: usize = new_entries.values().map(|v| v.len()).sum();
      println!(
         "{}  Added {} entries to {}",
         crate::style::icons::SUCCESS,
         entry_count,
         boundary.changelog_path.display()
      );
   }

   // Newly created changelogs are staged whole; tracked ones were staged as
   // pinned blobs above so unrelated unstaged edits stay unstaged.
   if !untracked_changelogs.is_empty() {
      stage_files(&untracked_changelogs, &args.dir)?;
   }

   Ok(())
}

/// Generate changelog entries via LLM
async fn generate_changelog_entries(
   changelog_path: &Path,
   is_package_changelog: bool,
   stat: &str,
   diff: &str,
   existing_entries: Option<&str>,
   config: &CommitConfig,
) -> Result<HashMap<ChangelogCategory, Vec<String>>> {
   let variant = if config.markdown_output {
      "markdown"
   } else {
      "default"
   };
   let parts = templates::render_changelog_prompt(
      variant,
      &changelog_path.display().to_string(),
      is_package_changelog,
      stat,
      diff,
      existing_entries,
   )?;

   let response = call_changelog_api(&parts, config).await?;

   // Convert string keys to categories and drop empty/whitespace-only entries.
   let mut result = HashMap::new();
   for (key, entries) in response.entries {
      let sanitized: Vec<String> = entries
         .iter()
         .filter_map(|entry| normalize_changelog_entry(entry))
         .collect();
      if sanitized.is_empty() {
         continue;
      }
      let category = ChangelogCategory::from_name(&key);
      result.insert(category, sanitized);
   }

   Ok(result)
}

/// Call the LLM API for changelog generation
async fn call_changelog_api(
   parts: &templates::PromptParts,
   config: &CommitConfig,
) -> Result<ChangelogResponse> {
   let changelog_schema = strict_json_schema(
      serde_json::json!({
         "entries": {
            "type": "object",
            "description": "Changelog entries grouped by category",
            "properties": {
               "Added": {
                  "type": "array",
                  "items": { "type": "string" },
                  "description": "New features or capabilities"
               },
               "Changed": {
                  "type": "array",
                  "items": { "type": "string" },
                  "description": "Changes to existing functionality"
               },
               "Fixed": {
                  "type": "array",
                  "items": { "type": "string" },
                  "description": "Bug fixes"
               },
               "Deprecated": {
                  "type": "array",
                  "items": { "type": "string" },
                  "description": "Features marked for removal"
               },
               "Removed": {
                  "type": "array",
                  "items": { "type": "string" },
                  "description": "Removed features"
               },
               "Security": {
                  "type": "array",
                  "items": { "type": "string" },
                  "description": "Security-related changes"
               },
               "Breaking Changes": {
                  "type": "array",
                  "items": { "type": "string" },
                  "description": "Breaking API or behavior changes"
               }
            },
            "additionalProperties": false
         }
      }),
      &["entries"],
   );

   let response = run_oneshot::<ChangelogResponse>(config, &OneShotSpec {
      operation:        "changelog",
      model:            &config.analysis_model,
      prompt_family:    "changelog",
      prompt_variant:   if config.markdown_output {
         "markdown"
      } else {
         "default"
      },
      system_prompt:    &parts.system,
      user_prompt:      &parts.user,
      tool_name:        "create_changelog_entries",
      tool_description: "Generate changelog entries grouped by category",
      schema:           &changelog_schema,
      progress_label:   Some("changelog"),
      debug:            None,
      cacheable:        true,
   })
   .await?;

   Ok(response.output)
}

/// Format existing entries for LLM context
fn format_existing_entries(unreleased: &UnreleasedSection) -> Option<String> {
   if unreleased.entries.is_empty() {
      return None;
   }

   let mut lines = Vec::new();
   for category in ChangelogCategory::render_order() {
      if let Some(entries) = unreleased.entries.get(category) {
         if entries.is_empty() {
            continue;
         }
         lines.push(format!("### {}", category.as_str()));
         for entry in entries {
            lines.push(entry.clone());
         }
         lines.push(String::new());
      }
   }

   if lines.is_empty() {
      None
   } else {
      Some(lines.join("\n"))
   }
}

/// Get list of staged files
fn get_staged_files(dir: &str) -> Result<Vec<String>> {
   let output = git_command()
      .args(["diff", "--cached", "--name-only"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to get staged files: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git diff --cached --name-only failed: {stderr}")));
   }

   let files: Vec<String> = String::from_utf8_lossy(&output.stdout)
      .lines()
      .filter(|s| !s.is_empty())
      .map(String::from)
      .collect();

   Ok(files)
}

/// Content of a changelog as currently staged in the index, or `None` when
/// the path is not tracked in the index.
fn staged_changelog_content(rel_path: &str, dir: &str) -> Result<Option<String>> {
   let output = git_command()
      .args(["show", &format!(":{rel_path}")])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to read staged {rel_path}: {e}")))?;

   if !output.status.success() {
      return Ok(None);
   }

   Ok(Some(String::from_utf8_lossy(&output.stdout).to_string()))
}

/// Index mode of a tracked changelog, defaulting to a regular file.
fn staged_changelog_mode(rel_path: &str, dir: &str) -> String {
   git_command()
      .args(["ls-files", "-s", "--", rel_path])
      .current_dir(dir)
      .output()
      .ok()
      .filter(|output| output.status.success())
      .and_then(|output| {
         String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .map(str::to_string)
      })
      .unwrap_or_else(|| "100644".to_string())
}

/// Stage exact changelog content as an index blob, without touching the
/// worktree.
///
/// `git add` would also sweep unrelated unstaged changelog edits into the
/// index; this stages only the generated entries.
fn stage_changelog_blob(rel_path: &str, content: &str, dir: &str) -> Result<()> {
   let mut child = git_command()
      .args(["hash-object", "-w", "--path", rel_path, "--stdin"])
      .current_dir(dir)
      .stdin(std::process::Stdio::piped())
      .stdout(std::process::Stdio::piped())
      .stderr(std::process::Stdio::piped())
      .spawn()
      .map_err(|e| CommitGenError::git(format!("Failed to spawn git hash-object: {e}")))?;

   {
      let Some(mut stdin) = child.stdin.take() else {
         return Err(CommitGenError::git("Failed to open git hash-object stdin".to_string()));
      };

      use std::io::Write;

      stdin
         .write_all(content.as_bytes())
         .map_err(|e| CommitGenError::git(format!("Failed to write blob for {rel_path}: {e}")))?;
   }

   let output = child
      .wait_with_output()
      .map_err(|e| CommitGenError::git(format!("Failed to wait for git hash-object: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git hash-object failed for {rel_path}: {stderr}")));
   }

   let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
   if oid.is_empty() {
      return Err(CommitGenError::git(format!(
         "git hash-object returned empty oid for {rel_path}"
      )));
   }

   let mode = staged_changelog_mode(rel_path, dir);
   let cacheinfo = format!("{mode},{oid},{rel_path}");
   let output = git_command()
      .args(["update-index", "--add", "--cacheinfo", &cacheinfo])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to stage {rel_path}: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git update-index failed for {rel_path}: {stderr}")));
   }

   Ok(())
}

/// Find all CHANGELOG.md files in the repo
fn find_changelogs(dir: &str) -> Result<Vec<PathBuf>> {
   let output = git_command()
      .args(["ls-files", "--full-name", "**/CHANGELOG.md", "CHANGELOG.md"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to find changelogs: {e}")))?;

   // git ls-files returns empty if no matches, which is fine
   let files: Vec<PathBuf> = String::from_utf8_lossy(&output.stdout)
      .lines()
      .filter(|s| !s.is_empty())
      .map(|s| PathBuf::from(dir).join(s))
      .collect();

   Ok(files)
}

/// Detect changelog boundaries for files
fn detect_boundaries(
   files: &[String],
   changelogs: &[PathBuf],
   dir: &str,
) -> Vec<ChangelogBoundary> {
   let mut file_to_changelog: HashMap<String, PathBuf> = HashMap::new();

   // Build a map of directory path (relative) -> changelog
   // e.g., "packages/core" -> "packages/core/CHANGELOG.md"
   //       "" (empty) -> "CHANGELOG.md" (root)
   let mut dir_to_changelog: HashMap<String, PathBuf> = HashMap::new();
   let mut root_changelog: Option<PathBuf> = None;

   for changelog in changelogs {
      // Get the relative path from repo root
      let rel_path = changelog
         .strip_prefix(dir)
         .unwrap_or(changelog)
         .to_string_lossy();

      // Parent directory of the changelog
      if let Some(parent) = Path::new(&*rel_path).parent() {
         let parent_str = parent.to_string_lossy().to_string();
         if parent_str.is_empty() || parent_str == "." {
            root_changelog = Some(changelog.clone());
         } else {
            dir_to_changelog.insert(parent_str, changelog.clone());
         }
      }
   }

   for file in files {
      // Walk up from file's directory to find matching changelog
      let mut current_path = Path::new(file)
         .parent()
         .map(|p| p.to_string_lossy().to_string());
      let mut found = false;

      while let Some(ref dir_path) = current_path {
         if let Some(changelog) = dir_to_changelog.get(dir_path) {
            file_to_changelog.insert(file.clone(), changelog.clone());
            found = true;
            break;
         }

         // Move up one directory
         let path = Path::new(dir_path);
         current_path = path.parent().and_then(|p| {
            let s = p.to_string_lossy().to_string();
            if s.is_empty() { None } else { Some(s) }
         });
      }

      // Fallback to root changelog
      if !found && let Some(ref root) = root_changelog {
         file_to_changelog.insert(file.clone(), root.clone());
      }
      // If no root changelog, file is skipped
   }

   // Group files by changelog
   let mut changelog_to_files: HashMap<PathBuf, Vec<String>> = HashMap::new();
   for (file, changelog) in file_to_changelog {
      changelog_to_files.entry(changelog).or_default().push(file);
   }

   // Build boundaries
   let boundaries: Vec<ChangelogBoundary> = changelog_to_files
      .into_iter()
      .map(|(changelog_path, files)| ChangelogBoundary {
         changelog_path,
         files,
         diff: String::new(), // Filled later
         stat: String::new(), // Filled later
      })
      .collect();

   boundaries
}

/// Get diff for specific files
fn get_diff_for_files(files: &[String], dir: &str) -> Result<String> {
   if files.is_empty() {
      return Ok(String::new());
   }

   let output = git_command()
      .args(["diff", "--cached", "--"])
      .args(files)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to get diff for files: {e}")))?;

   Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Get stat for specific files
fn get_stat_for_files(files: &[String], dir: &str) -> Result<String> {
   if files.is_empty() {
      return Ok(String::new());
   }

   let output = git_command()
      .args(["diff", "--cached", "--stat", "--"])
      .args(files)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to get stat for files: {e}")))?;

   Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Parse the [Unreleased] section from changelog content
fn parse_unreleased_section(content: &str, path: &Path) -> Result<UnreleasedSection> {
   let lines: Vec<&str> = content.lines().collect();

   // Find [Unreleased] header
   let header_line = lines
      .iter()
      .position(|l| {
         let trimmed = l.trim().to_lowercase();
         trimmed.contains("[unreleased]") || trimmed == "## unreleased"
      })
      .ok_or_else(|| CommitGenError::NoUnreleasedSection { path: path.display().to_string() })?;

   // Find end of unreleased section (next version header or EOF)
   let end_line = lines
      .iter()
      .skip(header_line + 1)
      .position(|l| {
         let trimmed = l.trim();
         // Look for version headers like ## [1.0.0] or ## 1.0.0
         trimmed.starts_with("## [") && trimmed.contains(']')
            || (trimmed.starts_with("## ")
               && trimmed.chars().nth(3).is_some_and(|c| c.is_ascii_digit()))
      })
      .map_or(lines.len(), |pos| header_line + 1 + pos);

   // Parse existing entries
   let mut entries: HashMap<ChangelogCategory, Vec<String>> = HashMap::new();
   let mut current_category: Option<ChangelogCategory> = None;

   for line in &lines[header_line + 1..end_line] {
      let trimmed = line.trim();

      // Check for category headers
      if trimmed.starts_with("### ") {
         let cat_name = trimmed.trim_start_matches("### ").trim();
         current_category = match cat_name.to_lowercase().as_str() {
            "added" => Some(ChangelogCategory::Added),
            "changed" => Some(ChangelogCategory::Changed),
            "fixed" => Some(ChangelogCategory::Fixed),
            "deprecated" => Some(ChangelogCategory::Deprecated),
            "removed" => Some(ChangelogCategory::Removed),
            "security" => Some(ChangelogCategory::Security),
            "breaking changes" | "breaking" => Some(ChangelogCategory::Breaking),
            _ => None,
         };
      } else if let Some(cat) = current_category {
         // Collect entry lines
         if (trimmed.starts_with("- ") || trimmed.starts_with("* "))
            && let Some(entry) = normalize_changelog_entry(trimmed)
         {
            entries.entry(cat).or_default().push(entry);
         }
      }
   }

   Ok(UnreleasedSection { header_line, end_line, entries })
}

/// Write entries to changelog content
fn write_entries(
   content: &str,
   unreleased: &UnreleasedSection,
   new_entries: &HashMap<ChangelogCategory, Vec<String>>,
) -> String {
   let lines: Vec<&str> = content.lines().collect();

   // Build new content
   let mut result = Vec::new();

   // Copy lines up to and including [Unreleased] header
   result.extend(
      lines[..=unreleased.header_line]
         .iter()
         .map(|s| s.to_string()),
   );

   // Add blank line after header if not present
   if unreleased.header_line + 1 < lines.len() && !lines[unreleased.header_line + 1].is_empty() {
      result.push(String::new());
   }

   // Write categories in order
   for category in ChangelogCategory::render_order() {
      let new_in_category: Vec<String> = new_entries
         .get(category)
         .into_iter()
         .flat_map(|entries| entries.iter())
         .filter_map(|entry| normalize_changelog_entry(entry))
         .collect();
      let existing_in_category = unreleased.entries.get(category);

      let has_existing = existing_in_category.is_some_and(|v| !v.is_empty());
      if new_in_category.is_empty() && !has_existing {
         continue;
      }

      result.push(format!("### {}", category.as_str()));
      result.push(String::new());

      // New entries first
      result.extend(new_in_category);

      // Then existing entries
      if let Some(entries) = existing_in_category {
         result.extend(entries.iter().cloned());
      }

      result.push(String::new());
   }

   // Copy remaining lines (after [Unreleased] section)
   if unreleased.end_line < lines.len() {
      result.extend(lines[unreleased.end_line..].iter().map(|s| s.to_string()));
   }

   result.join("\n")
}

#[cfg(test)]
mod tests {
   use super::*;

   fn run_git(dir: &tempfile::TempDir, args: &[&str]) {
      let output = git_command()
         .args(args)
         .current_dir(dir.path())
         .output()
         .unwrap_or_else(|err| panic!("git {args:?} failed to spawn: {err}"));
      assert!(
         output.status.success(),
         "git {:?} failed: {}",
         args,
         String::from_utf8_lossy(&output.stderr)
      );
   }

   const BASE_CHANGELOG: &str =
      "# Changelog\n\n## [Unreleased]\n\n## [1.0.0] - 2020-01-01\n\n### Added\n\n- Old entry.\n";

   #[test]
   fn test_changelog_staging_keeps_unrelated_unstaged_edits_out() {
      let dir = tempfile::TempDir::new().unwrap();
      let dir_str = dir.path().to_str().unwrap();
      run_git(&dir, &["init"]);
      run_git(&dir, &["config", "user.name", "Changelog Test"]);
      run_git(&dir, &["config", "user.email", "changelog@test.local"]);
      run_git(&dir, &["config", "commit.gpgsign", "false"]);
      let changelog_path = dir.path().join("CHANGELOG.md");
      std::fs::write(&changelog_path, BASE_CHANGELOG).unwrap();
      run_git(&dir, &["add", "."]);
      run_git(&dir, &["commit", "-m", "base"]);

      // Unrelated unstaged edit the user left in the changelog.
      let worktree_content = format!("{BASE_CHANGELOG}\nUNRELATED DRAFT NOTES\n");
      std::fs::write(&changelog_path, &worktree_content).unwrap();

      // Mirror run_changelog_flow's tracked-changelog path with fixed entries.
      let staged_content = staged_changelog_content("CHANGELOG.md", dir_str)
         .unwrap()
         .expect("changelog is tracked");
      assert_eq!(staged_content, BASE_CHANGELOG);

      let unreleased = parse_unreleased_section(&staged_content, &changelog_path).unwrap();
      let worktree_unreleased =
         parse_unreleased_section(&worktree_content, &changelog_path).unwrap();

      let mut new_entries = HashMap::new();
      new_entries.insert(ChangelogCategory::Added, vec!["New pinned entry.".to_string()]);

      let updated_staged = write_entries(&staged_content, &unreleased, &new_entries);
      let updated_worktree = write_entries(&worktree_content, &worktree_unreleased, &new_entries);
      std::fs::write(&changelog_path, &updated_worktree).unwrap();
      stage_changelog_blob("CHANGELOG.md", &updated_staged, dir_str).unwrap();

      let staged_now = staged_changelog_content("CHANGELOG.md", dir_str)
         .unwrap()
         .expect("changelog still tracked");
      assert!(staged_now.contains("New pinned entry."));
      assert!(
         !staged_now.contains("UNRELATED DRAFT NOTES"),
         "unstaged edits must stay out of the index"
      );

      let on_disk = std::fs::read_to_string(&changelog_path).unwrap();
      assert!(on_disk.contains("New pinned entry."));
      assert!(on_disk.contains("UNRELATED DRAFT NOTES"), "worktree keeps the user's edits");
   }

   #[test]
   fn test_extract_json_from_content_raw() {
      let content = r#"{"entries": {"Added": ["entry 1"]}}"#;
      let result = crate::api::extract_json_from_content(content);
      assert_eq!(result, r#"{"entries": {"Added": ["entry 1"]}}"#);
   }

   #[test]
   fn test_extract_json_from_content_code_block() {
      let content = r#"Here's the changelog:

```json
{"entries": {"Added": ["entry 1"]}}
```

That's all!"#;
      let result = crate::api::extract_json_from_content(content);
      assert_eq!(result, r#"{"entries": {"Added": ["entry 1"]}}"#);
   }

   #[test]
   fn test_extract_json_from_content_generic_block() {
      let content = r#"```
{"entries": {"Fixed": ["bug fix"]}}
```"#;
      let result = crate::api::extract_json_from_content(content);
      assert_eq!(result, r#"{"entries": {"Fixed": ["bug fix"]}}"#);
   }

   #[test]
   fn test_parse_unreleased_section() {
      let content = r"# Changelog

## [Unreleased]

### Added

- Feature one
- Feature two

### Fixed

- Bug fix

## [1.0.0] - 2024-01-01

### Added

- Initial release
";

      let section = parse_unreleased_section(content, Path::new("CHANGELOG.md")).unwrap();
      assert_eq!(section.header_line, 2);
      assert_eq!(section.end_line, 13); // Line 13 is "## [1.0.0] - 2024-01-01"
      assert_eq!(
         section
            .entries
            .get(&ChangelogCategory::Added)
            .unwrap()
            .len(),
         2
      );
      assert_eq!(
         section
            .entries
            .get(&ChangelogCategory::Fixed)
            .unwrap()
            .len(),
         1
      );
   }

   #[test]
   fn test_format_existing_entries() {
      let mut entries = HashMap::new();
      entries.insert(ChangelogCategory::Added, vec![
         "- Feature one".to_string(),
         "- Feature two".to_string(),
      ]);
      entries.insert(ChangelogCategory::Fixed, vec!["- Bug fix".to_string()]);

      let unreleased = UnreleasedSection { header_line: 0, end_line: 10, entries };

      let formatted = format_existing_entries(&unreleased).unwrap();
      assert!(formatted.contains("### Added"));
      assert!(formatted.contains("- Feature one"));
      assert!(formatted.contains("### Fixed"));
      assert!(formatted.contains("- Bug fix"));
   }

   #[test]
   fn test_write_entries_trims_and_skips_empty_bullets() {
      let content = r"# Changelog

## [Unreleased]

## [1.0.0] - 2024-01-01
";
      let unreleased = parse_unreleased_section(content, Path::new("CHANGELOG.md")).unwrap();
      let mut new_entries = HashMap::new();
      new_entries.insert(ChangelogCategory::Added, vec![
         "  Added configurable power assertions  ".to_string(),
         " -   ".to_string(),
         String::new(),
         "* Fixed prompt cancellation cleanup ".to_string(),
      ]);

      let updated = write_entries(content, &unreleased, &new_entries);

      assert!(updated.contains("- Added configurable power assertions\n"));
      assert!(updated.contains("- Fixed prompt cancellation cleanup\n"));
      assert!(!updated.contains("- \n"));
      assert!(!updated.contains("* Fixed"));
   }

   #[test]
   fn test_parse_unreleased_section_skips_empty_bullets() {
      let content = r"# Changelog

## [Unreleased]

### Fixed

- 
- Fixed cancellation cleanup
*    

## [1.0.0] - 2024-01-01
";

      let section = parse_unreleased_section(content, Path::new("CHANGELOG.md")).unwrap();

      assert_eq!(section.entries.get(&ChangelogCategory::Fixed).unwrap(), &vec![
         "- Fixed cancellation cleanup".to_string()
      ]);
   }

   #[test]
   fn test_format_existing_entries_empty() {
      let unreleased =
         UnreleasedSection { header_line: 0, end_line: 10, entries: HashMap::new() };

      assert!(format_existing_entries(&unreleased).is_none());
   }
}
