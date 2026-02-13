use std::process::Command;

use crate::{
   error::{CommitGenError, Result},
   types::{ChangeGroup, FileChange, HunkSelector},
};

/// Represents a parsed hunk from a diff
#[derive(Debug, Clone)]
struct ParsedHunk {
   header:         String,
   #[allow(dead_code, reason = "Useful metadata for future enhancements")]
   old_start:      usize,
   #[allow(dead_code, reason = "Useful metadata for future enhancements")]
   old_count:      usize,
   #[allow(dead_code, reason = "Useful metadata for future enhancements")]
   new_start:      usize,
   #[allow(dead_code, reason = "Useful metadata for future enhancements")]
   new_count:      usize,
   lines:          Vec<String>,
   old_line_range: (usize, usize), // (start, end) in original file
}

/// Create a patch for specific files
pub fn create_patch_for_files(files: &[String], dir: &str) -> Result<String> {
   let output = Command::new("git")
      .arg("diff")
      .arg("HEAD")
      .arg("--")
      .args(files)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to create patch: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git diff failed: {stderr}")));
   }

   Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Apply patch to staging area
pub fn apply_patch_to_index(patch: &str, dir: &str) -> Result<()> {
   let mut child = Command::new("git")
      .args(["apply", "--cached"])
      .current_dir(dir)
      .stdin(std::process::Stdio::piped())
      .stdout(std::process::Stdio::piped())
      .stderr(std::process::Stdio::piped())
      .spawn()
      .map_err(|e| CommitGenError::git(format!("Failed to spawn git apply: {e}")))?;

   if let Some(mut stdin) = child.stdin.take() {
      use std::io::Write;
      stdin
         .write_all(patch.as_bytes())
         .map_err(|e| CommitGenError::git(format!("Failed to write patch: {e}")))?;
   }

   let output = child
      .wait_with_output()
      .map_err(|e| CommitGenError::git(format!("Failed to wait for git apply: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git apply --cached failed: {stderr}")));
   }

   Ok(())
}

/// Stage specific files (simpler alternative to patch application)
pub fn stage_files(files: &[String], dir: &str) -> Result<()> {
   if files.is_empty() {
      return Ok(());
   }

   let output = Command::new("git")
      .arg("add")
      .arg("--")
      .args(files)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to stage files: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git add failed: {stderr}")));
   }

   Ok(())
}

/// Reset staging area
pub fn reset_staging(dir: &str) -> Result<()> {
   let output = Command::new("git")
      .args(["reset", "HEAD"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to reset staging: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git reset HEAD failed: {stderr}")));
   }

   Ok(())
}

/// Parse hunk header to extract line numbers
/// Format: @@ -`old_start,old_count` +`new_start,new_count` @@
fn parse_hunk_header(header: &str) -> Option<(usize, usize, usize, usize)> {
   let trimmed = header.trim();
   if !trimmed.starts_with("@@") {
      return None;
   }

   // Extract the part between @@ markers
   let middle = if let Some(start) = trimmed.find("@@") {
      let after_first = &trimmed[start + 2..];
      if let Some(end) = after_first.find("@@") {
         &after_first[..end].trim()
      } else {
         return None;
      }
   } else {
      return None;
   };

   // Parse "-old_start,old_count +new_start,new_count"
   let parts: Vec<&str> = middle.split_whitespace().collect();
   if parts.len() < 2 {
      return None;
   }

   let old_part = parts[0].strip_prefix('-')?;
   let new_part = parts[1].strip_prefix('+')?;

   let parse_range = |s: &str| -> Option<(usize, usize)> {
      if let Some((start, count)) = s.split_once(',') {
         Some((start.parse().ok()?, count.parse().ok()?))
      } else {
         // If no comma, it's just a line number (count is 1)
         Some((s.parse().ok()?, 1))
      }
   };

   let (old_start, old_count) = parse_range(old_part)?;
   let (new_start, new_count) = parse_range(new_part)?;

   Some((old_start, old_count, new_start, new_count))
}

/// Parse all hunks from a file's diff
fn parse_file_hunks(file_diff: &str) -> Vec<ParsedHunk> {
   let mut hunks = Vec::new();
   let mut in_header = true;
   let mut current_hunk: Option<ParsedHunk> = None;

   for line in file_diff.lines() {
      if in_header {
         if line.starts_with("+++") {
            in_header = false;
         }
         continue;
      }

      if line.starts_with("@@ ") {
         // Save previous hunk
         if let Some(hunk) = current_hunk.take() {
            hunks.push(hunk);
         }

         // Parse new hunk header
         if let Some((old_start, old_count, new_start, new_count)) = parse_hunk_header(line) {
            let old_end = if old_count == 0 {
               old_start
            } else {
               old_start + old_count - 1
            };

            current_hunk = Some(ParsedHunk {
               header: line.to_string(),
               old_start,
               old_count,
               new_start,
               new_count,
               lines: vec![line.to_string()],
               old_line_range: (old_start, old_end),
            });
         }
      } else if let Some(hunk) = &mut current_hunk {
         hunk.lines.push(line.to_string());
      }
   }

   // Don't forget the last hunk
   if let Some(hunk) = current_hunk {
      hunks.push(hunk);
   }

   hunks
}

/// Map line range to hunks that overlap with it
fn find_hunks_for_line_range(hunks: &[ParsedHunk], start: usize, end: usize) -> Vec<String> {
   hunks
      .iter()
      .filter(|hunk| {
         // Check if line range overlaps with hunk's old line range
         let (hunk_start, hunk_end) = hunk.old_line_range;
         !(end < hunk_start || start > hunk_end)
      })
      .map(|hunk| hunk.header.clone())
      .collect()
}

/// Convert `HunkSelectors` to actual hunk headers deterministically
fn resolve_selectors_to_headers(
   full_diff: &str,
   file_path: &str,
   selectors: &[HunkSelector],
) -> Result<Vec<String>> {
   // Extract file diff
   let file_diff = extract_file_diff(full_diff, file_path)?;

   // Parse all hunks from the file
   let hunks = parse_file_hunks(&file_diff);

   let mut headers = Vec::new();

   for selector in selectors {
      match selector {
         HunkSelector::All => {
            // Return all hunk headers
            return Ok(hunks.iter().map(|h| h.header.clone()).collect());
         },
         HunkSelector::Lines { start, end } => {
            // Find hunks that overlap with this line range
            let matching = find_hunks_for_line_range(&hunks, *start, *end);
            if matching.is_empty() {
               // Check if there are any nearby hunks to suggest
               let nearby: Vec<_> = hunks
                  .iter()
                  .map(|h| {
                     let (hunk_start, hunk_end) = h.old_line_range;
                     let distance = if *end < hunk_start {
                        hunk_start - *end
                     } else {
                        (*start).saturating_sub(hunk_end)
                     };
                     (distance, hunk_start, hunk_end)
                  })
                  .filter(|(dist, ..)| *dist > 0 && *dist < 20)
                  .collect();

               let hint = if nearby.is_empty() {
                  String::new()
               } else {
                  let (_, nearest_start, nearest_end) =
                     nearby.iter().min_by_key(|(dist, ..)| dist).unwrap();
                  format!(" (nearest hunk: lines {nearest_start}-{nearest_end})")
               };

               return Err(CommitGenError::Other(format!(
                  "No changes found in lines {start}-{end} of {file_path}. These lines may be \
                   context (unchanged) rather than modifications{hint}"
               )));
            }
            headers.extend(matching);
         },
         HunkSelector::Search { pattern } => {
            // If it looks like a hunk header, try to match it directly
            if pattern.starts_with("@@") {
               let normalized_pattern = normalize_hunk_header(pattern);
               let matching: Vec<String> = hunks
                  .iter()
                  .filter(|h| normalize_hunk_header(&h.header) == normalized_pattern)
                  .map(|h| h.header.clone())
                  .collect();

               if matching.is_empty() {
                  return Err(CommitGenError::Other(format!(
                     "Hunk header not found: {pattern} in {file_path}"
                  )));
               }
               headers.extend(matching);
            } else {
               // Search for pattern in hunk lines
               let matching: Vec<String> = hunks
                  .iter()
                  .filter(|h| h.lines.iter().any(|line| line.contains(pattern)))
                  .map(|h| h.header.clone())
                  .collect();

               if matching.is_empty() {
                  return Err(CommitGenError::Other(format!(
                     "Pattern '{pattern}' not found in any hunk in {file_path}"
                  )));
               }
               headers.extend(matching);
            }
         },
      }
   }

   // Deduplicate headers while preserving order
   let mut seen = std::collections::HashSet::new();
   Ok(headers
      .into_iter()
      .filter(|h| seen.insert(h.clone()))
      .collect())
}

/// Extract specific hunks from a full diff for a file
fn extract_hunks_for_file(
   full_diff: &str,
   file_path: &str,
   hunk_headers: &[String],
) -> Result<String> {
   // If "ALL", return entire file diff
   if hunk_headers.len() == 1 && hunk_headers[0] == "ALL" {
      return extract_file_diff(full_diff, file_path);
   }

   let file_diff = extract_file_diff(full_diff, file_path)?;
   let mut result = String::new();
   let mut in_header = true;
   let mut current_hunk = String::new();
   let mut current_hunk_header;
   let mut include_current = false;

   for line in file_diff.lines() {
      if in_header {
         result.push_str(line);
         result.push('\n');
         if line.starts_with("+++") {
            in_header = false;
         }
      } else if line.starts_with("@@ ") {
         // Save previous hunk if we were including it
         if include_current && !current_hunk.is_empty() {
            result.push_str(&current_hunk);
         }

         // Start new hunk
         current_hunk_header = line.to_string();
         current_hunk = format!("{line}\n");

         // Check if this hunk should be included
         include_current = hunk_headers.iter().any(|h| {
            // Normalize comparison - just compare the numeric parts
            normalize_hunk_header(h) == normalize_hunk_header(&current_hunk_header)
         });
      } else {
         current_hunk.push_str(line);
         current_hunk.push('\n');
      }
   }

   // Don't forget the last hunk
   if include_current && !current_hunk.is_empty() {
      result.push_str(&current_hunk);
   }

   if result
      .lines()
      .filter(|l| !l.starts_with("---") && !l.starts_with("+++") && !l.starts_with("diff "))
      .count()
      == 0
   {
      return Err(CommitGenError::Other(format!(
         "No hunks found for {file_path} with headers {hunk_headers:?}"
      )));
   }

   Ok(result)
}

/// Normalize hunk header for fuzzy comparison
/// Extracts line numbers only, ignoring whitespace variations and context
fn normalize_hunk_header(header: &str) -> String {
   let trimmed = header.trim();

   // Extract the part between @@ markers
   let middle = if let Some(start) = trimmed.find("@@") {
      let after_first = &trimmed[start + 2..];
      if let Some(end) = after_first.find("@@") {
         &after_first[..end]
      } else {
         after_first
      }
   } else {
      trimmed
   };

   // Remove all whitespace for fuzzy matching
   // Keep only: digits, commas, hyphens, plus signs
   middle
      .chars()
      .filter(|c| c.is_ascii_digit() || *c == ',' || *c == '-' || *c == '+')
      .collect()
}

/// Extract the diff for a specific file from a full diff
fn extract_file_diff(full_diff: &str, file_path: &str) -> Result<String> {
   let mut result = String::new();
   let mut in_file = false;
   let mut found = false;

   for line in full_diff.lines() {
      if line.starts_with("diff --git") {
         // Check if this is our file
         if line.contains(&format!("b/{file_path}")) || line.ends_with(&format!(" b/{file_path}")) {
            in_file = true;
            found = true;
            result.push_str(line);
            result.push('\n');
         } else {
            in_file = false;
         }
      } else if in_file {
         result.push_str(line);
         result.push('\n');
      }
   }

   if !found {
      return Err(CommitGenError::Other(format!("File {file_path} not found in diff")));
   }

   Ok(result)
}

/// Create a patch for specific file changes with hunk selection
pub fn create_patch_for_changes(full_diff: &str, changes: &[FileChange]) -> Result<String> {
   let mut patch = String::new();

   for change in changes {
      // Resolve selectors to actual hunk headers
      let hunk_headers = resolve_selectors_to_headers(full_diff, &change.path, &change.hunks)?;
      let file_patch = extract_hunks_for_file(full_diff, &change.path, &hunk_headers)?;
      patch.push_str(&file_patch);
   }

   Ok(patch)
}

/// Stage changes for a specific group (hunk-aware).
/// The `full_diff` argument must be taken before any compose commits run so the
/// recorded hunk headers remain stable across groups.
pub fn stage_group_changes(group: &ChangeGroup, dir: &str, full_diff: &str) -> Result<()> {
   let mut full_files = Vec::new();
   let mut partial_changes = Vec::new();

   for change in &group.changes {
      // Check if all selectors are "All" variant
      let is_all = change.hunks.len() == 1 && matches!(change.hunks[0], HunkSelector::All);

      if is_all {
         full_files.push(change.path.clone());
      } else {
         partial_changes.push(change.clone());
      }
   }

   if !full_files.is_empty() {
      // Deduplicate to avoid redundant git add calls
      full_files.sort();
      full_files.dedup();
      stage_files(&full_files, dir)?;
   }

   if partial_changes.is_empty() {
      return Ok(());
   }

   let patch = create_patch_for_changes(full_diff, &partial_changes)?;
   apply_patch_to_index(&patch, dir)
}
