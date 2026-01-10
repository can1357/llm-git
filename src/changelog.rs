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
   process::Command,
   thread,
   time::Duration,
};

use serde::Deserialize;

use crate::{
   config::CommitConfig,
   diff::smart_truncate_diff,
   error::{CommitGenError, Result},
   patch::stage_files,
   templates,
   tokens::create_token_counter,
   types::{ChangelogBoundary, ChangelogCategory, UnreleasedSection},
};

/// Response from the changelog generation LLM call
#[derive(Debug, Deserialize)]
struct ChangelogResponse {
   entries: HashMap<String, Vec<String>>,
}

/// Run the changelog maintenance flow
///
/// 1. Get staged files (excluding CHANGELOG.md files)
/// 2. Detect changelog boundaries
/// 3. For each boundary: generate entries via LLM, write to changelog
/// 4. Stage modified changelogs
pub fn run_changelog_flow(args: &crate::types::Args, config: &CommitConfig) -> Result<()> {
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

   let mut modified_changelogs = Vec::new();

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

      // Parse existing [Unreleased] section for context
      let changelog_content = std::fs::read_to_string(&boundary.changelog_path).map_err(|e| {
         CommitGenError::ChangelogParseError {
            path:   boundary.changelog_path.display().to_string(),
            reason: e.to_string(),
         }
      })?;

      let unreleased = match parse_unreleased_section(&changelog_content, &boundary.changelog_path)
      {
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
      ) {
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

      // Write entries to changelog
      let updated = write_entries(&changelog_content, &unreleased, &new_entries);
      std::fs::write(&boundary.changelog_path, updated).map_err(|e| {
         CommitGenError::ChangelogParseError {
            path:   boundary.changelog_path.display().to_string(),
            reason: format!("Failed to write: {e}"),
         }
      })?;

      let entry_count: usize = new_entries.values().map(|v| v.len()).sum();
      modified_changelogs.push(boundary.changelog_path.display().to_string());
      println!(
         "{}  Added {} entries to {}",
         crate::style::icons::SUCCESS,
         entry_count,
         boundary.changelog_path.display()
      );
   }

   // Stage modified changelogs
   if !modified_changelogs.is_empty() {
      stage_files(&modified_changelogs, &args.dir)?;
   }

   Ok(())
}

/// Generate changelog entries via LLM
fn generate_changelog_entries(
   changelog_path: &Path,
   is_package_changelog: bool,
   stat: &str,
   diff: &str,
   existing_entries: Option<&str>,
   config: &CommitConfig,
) -> Result<HashMap<ChangelogCategory, Vec<String>>> {
   let parts = templates::render_changelog_prompt(
      "default",
      &changelog_path.display().to_string(),
      is_package_changelog,
      stat,
      diff,
      existing_entries,
   )?;

   let response = call_changelog_api(&parts, config)?;

   // Convert string keys to ChangelogCategory
   let mut result = HashMap::new();
   for (key, entries) in response.entries {
      if entries.is_empty() {
         continue;
      }
      let category = ChangelogCategory::from_name(&key);
      result.insert(category, entries);
   }

   Ok(result)
}

/// Call the LLM API for changelog generation
fn call_changelog_api(
   parts: &templates::PromptParts,
   config: &CommitConfig,
) -> Result<ChangelogResponse> {
   let client = reqwest::blocking::Client::builder()
      .timeout(Duration::from_secs(config.request_timeout_secs))
      .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
      .build()
      .expect("Failed to build HTTP client");

   let model = config.model.clone();

   let mut attempt = 0;
   loop {
      attempt += 1;

      let mut messages = Vec::new();
      if !parts.system.is_empty() {
         messages.push(serde_json::json!({
            "role": "system",
            "content": parts.system
         }));
      }
      messages.push(serde_json::json!({
         "role": "user",
         "content": parts.user
      }));

      let request_body = serde_json::json!({
         "model": model,
         "max_tokens": 2000,
         "temperature": config.temperature,
         "messages": messages
      });

      let mut request_builder = client
         .post(format!("{}/chat/completions", config.api_base_url))
         .header("content-type", "application/json");

      if let Some(api_key) = &config.api_key {
         request_builder = request_builder.header("Authorization", format!("Bearer {api_key}"));
      }

      let response = request_builder
         .json(&request_body)
         .send()
         .map_err(CommitGenError::HttpError)?;

      let status = response.status();

      if status.is_server_error() {
         if attempt < config.max_retries {
            let backoff_ms = config.initial_backoff_ms * (1 << (attempt - 1));
            eprintln!(
               "{}",
               crate::style::warning(&format!(
                  "Server error {status}, retry {attempt}/{} after {backoff_ms}ms...",
                  config.max_retries
               ))
            );
            thread::sleep(Duration::from_millis(backoff_ms));
            continue;
         }
         let error_text = response
            .text()
            .unwrap_or_else(|_| "Unknown error".to_string());
         return Err(CommitGenError::ApiError { status: status.as_u16(), body: error_text });
      }

      if !status.is_success() {
         let error_text = response
            .text()
            .unwrap_or_else(|_| "Unknown error".to_string());
         return Err(CommitGenError::ApiError { status: status.as_u16(), body: error_text });
      }

      let api_response: serde_json::Value = response.json().map_err(CommitGenError::HttpError)?;

      // Extract content from response
      let content = api_response["choices"][0]["message"]["content"]
         .as_str()
         .ok_or_else(|| CommitGenError::Other("No content in API response".to_string()))?;

      // Parse JSON from content (may be wrapped in markdown code blocks)
      let json_str = extract_json_from_content(content);

      let changelog_response: ChangelogResponse = serde_json::from_str(&json_str).map_err(|e| {
         CommitGenError::Other(format!(
            "Failed to parse changelog response: {e}. Content was: {}",
            json_str.chars().take(500).collect::<String>()
         ))
      })?;

      return Ok(changelog_response);
   }
}

/// Extract JSON from content that may be wrapped in markdown code blocks
fn extract_json_from_content(content: &str) -> String {
   let trimmed = content.trim();

   // Try to find JSON in code blocks
   if let Some(start) = trimmed.find("```json") {
      let after_marker = &trimmed[start + 7..];
      if let Some(end) = after_marker.find("```") {
         return after_marker[..end].trim().to_string();
      }
   }

   // Try generic code block
   if let Some(start) = trimmed.find("```") {
      let after_marker = &trimmed[start + 3..];
      // Skip optional language identifier
      let content_start = after_marker.find('\n').map_or(0, |i| i + 1);
      let after_newline = &after_marker[content_start..];
      if let Some(end) = after_newline.find("```") {
         return after_newline[..end].trim().to_string();
      }
   }

   // Try to find raw JSON object
   if let Some(start) = trimmed.find('{')
      && let Some(end) = trimmed.rfind('}')
   {
      return trimmed[start..=end].to_string();
   }

   trimmed.to_string()
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
   let output = Command::new("git")
      .args(["diff", "--cached", "--name-only"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::GitError(format!("Failed to get staged files: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::GitError(format!(
         "git diff --cached --name-only failed: {stderr}"
      )));
   }

   let files: Vec<String> = String::from_utf8_lossy(&output.stdout)
      .lines()
      .filter(|s| !s.is_empty())
      .map(String::from)
      .collect();

   Ok(files)
}

/// Find all CHANGELOG.md files in the repo
fn find_changelogs(dir: &str) -> Result<Vec<PathBuf>> {
   let output = Command::new("git")
      .args(["ls-files", "--full-name", "**/CHANGELOG.md", "CHANGELOG.md"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::GitError(format!("Failed to find changelogs: {e}")))?;

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

   let output = Command::new("git")
      .args(["diff", "--cached", "--"])
      .args(files)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::GitError(format!("Failed to get diff for files: {e}")))?;

   Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Get stat for specific files
fn get_stat_for_files(files: &[String], dir: &str) -> Result<String> {
   if files.is_empty() {
      return Ok(String::new());
   }

   let output = Command::new("git")
      .args(["diff", "--cached", "--stat", "--"])
      .args(files)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::GitError(format!("Failed to get stat for files: {e}")))?;

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
         if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            entries.entry(cat).or_default().push(trimmed.to_string());
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
      let new_in_category = new_entries.get(category);
      let existing_in_category = unreleased.entries.get(category);

      let has_new = new_in_category.is_some_and(|v| !v.is_empty());
      let has_existing = existing_in_category.is_some_and(|v| !v.is_empty());

      if !has_new && !has_existing {
         continue;
      }

      result.push(format!("### {}", category.as_str()));
      result.push(String::new());

      // New entries first
      if let Some(entries) = new_in_category {
         for entry in entries {
            // Ensure entry starts with "- "
            if entry.starts_with("- ") || entry.starts_with("* ") {
               result.push(entry.clone());
            } else {
               result.push(format!("- {entry}"));
            }
         }
      }

      // Then existing entries
      if let Some(entries) = existing_in_category {
         for entry in entries {
            result.push(entry.clone());
         }
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

   #[test]
   fn test_extract_json_from_content_raw() {
      let content = r#"{"entries": {"Added": ["entry 1"]}}"#;
      let result = extract_json_from_content(content);
      assert_eq!(result, r#"{"entries": {"Added": ["entry 1"]}}"#);
   }

   #[test]
   fn test_extract_json_from_content_code_block() {
      let content = r#"Here's the changelog:

```json
{"entries": {"Added": ["entry 1"]}}
```

That's all!"#;
      let result = extract_json_from_content(content);
      assert_eq!(result, r#"{"entries": {"Added": ["entry 1"]}}"#);
   }

   #[test]
   fn test_extract_json_from_content_generic_block() {
      let content = r#"```
{"entries": {"Fixed": ["bug fix"]}}
```"#;
      let result = extract_json_from_content(content);
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
   fn test_format_existing_entries_empty() {
      let unreleased =
         UnreleasedSection { header_line: 0, end_line: 10, entries: HashMap::new() };

      assert!(format_existing_entries(&unreleased).is_none());
   }
}
