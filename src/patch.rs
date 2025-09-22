use std::process::Command;

use crate::{
   error::{CommitGenError, Result},
   types::{ChangeGroup, FileChange},
};

/// Create a patch for specific files
pub fn create_patch_for_files(files: &[String], dir: &str) -> Result<String> {
   let output = Command::new("git")
      .arg("diff")
      .arg("HEAD")
      .arg("--")
      .args(files)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::GitError(format!("Failed to create patch: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::GitError(format!("git diff failed: {stderr}")));
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
      .map_err(|e| CommitGenError::GitError(format!("Failed to spawn git apply: {e}")))?;

   if let Some(mut stdin) = child.stdin.take() {
      use std::io::Write;
      stdin
         .write_all(patch.as_bytes())
         .map_err(|e| CommitGenError::GitError(format!("Failed to write patch: {e}")))?;
   }

   let output = child
      .wait_with_output()
      .map_err(|e| CommitGenError::GitError(format!("Failed to wait for git apply: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::GitError(format!("git apply --cached failed: {stderr}")));
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
      .map_err(|e| CommitGenError::GitError(format!("Failed to stage files: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::GitError(format!("git add failed: {stderr}")));
   }

   Ok(())
}

/// Reset staging area
pub fn reset_staging(dir: &str) -> Result<()> {
   let output = Command::new("git")
      .args(["reset", "HEAD"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::GitError(format!("Failed to reset staging: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::GitError(format!("git reset HEAD failed: {stderr}")));
   }

   Ok(())
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
   let mut current_hunk_header = String::new();
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
      let file_patch = extract_hunks_for_file(full_diff, &change.path, &change.hunks)?;
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
      if change.hunks.len() == 1 && change.hunks[0] == "ALL" {
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
