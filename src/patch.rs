use std::{
   borrow::Cow,
   collections::{BTreeMap, HashSet},
   path::Path,
};

use crate::{
   compose_types::{ComposeExecutableGroup, ComposeFile, ComposeHunk, ComposeSnapshot},
   error::{CommitGenError, Result},
   git::{git_command, git_command_with_index},
};

#[derive(Debug, Clone)]
struct ParsedHunk {
   old_start: usize,
   old_count: usize,
   new_start: usize,
   new_count: usize,
   header:    String,
   lines:     Vec<String>,
}

#[derive(Debug, Clone)]
struct ParsedFile {
   path:         String,
   header_lines: Vec<String>,
   hunks:        Vec<ParsedHunk>,
   additions:    usize,
   deletions:    usize,
   is_binary:    bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeGroupPatch {
   pub diff:       String,
   pub stat:       String,
   apply_patches:  Vec<FilePatch>,
   fallback_files: Vec<String>,
   index_blobs:    Vec<IndexBlob>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilePatch {
   path:  String,
   patch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexBlob {
   path:   String,
   mode:   String,
   object: IndexObject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IndexObject {
   BlobContents(String),
   ExistingObject(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageResult {
   Staged,
   AlreadyApplied,
   EmptyPatch,
}

impl StageResult {
   const fn combine(self, other: Self) -> Self {
      match (self, other) {
         (Self::Staged, _) | (_, Self::Staged) => Self::Staged,
         (Self::AlreadyApplied, _) | (_, Self::AlreadyApplied) => Self::AlreadyApplied,
         (Self::EmptyPatch, Self::EmptyPatch) => Self::EmptyPatch,
      }
   }
}

/// Outcome of attempting to apply a single file's patch to the index.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FilePatchOutcome {
   Staged,
   AlreadyApplied,
   Empty,
   Failed(String),
}

/// A planned file whose patch could not be applied against the current state.
///
/// Its changes are intentionally left untouched in the working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFile {
   pub path:   String,
   pub reason: String,
}

/// Result of staging a compose group, including any files whose planned patch
/// no longer applies and were therefore left uncommitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeStageOutcome {
   pub result:  StageResult,
   pub skipped: Vec<SkippedFile>,
}

/// Run `git apply` with a patch supplied on stdin.
fn git_command_for_index(index_file: Option<&Path>) -> std::process::Command {
   if let Some(index_file) = index_file {
      git_command_with_index(index_file)
   } else {
      git_command()
   }
}

fn run_git_apply(
   patch: &str,
   args: &[&str],
   dir: &str,
   index_file: Option<&Path>,
) -> Result<std::process::Output> {
   let mut child = git_command_for_index(index_file)
      .args(args)
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

   child
      .wait_with_output()
      .map_err(|e| CommitGenError::git(format!("Failed to wait for git apply: {e}")))
}

fn patch_is_already_applied_to_index(
   patch: &str,
   dir: &str,
   index_file: Option<&Path>,
) -> Result<bool> {
   let output = run_git_apply(
      patch,
      &["apply", "--cached", "--reverse", "--check", "--recount"],
      dir,
      index_file,
   )?;
   Ok(output.status.success())
}

/// Apply a single file's patch to the staging area.
///
/// A patch that no longer applies against the current index/worktree is
/// reported as [`FilePatchOutcome::Failed`] instead of erroring, so callers can
/// stage the files that do apply and leave the rest untouched in the worktree.
fn apply_file_patch_to_index(
   patch: &str,
   dir: &str,
   index_file: Option<&Path>,
) -> Result<FilePatchOutcome> {
   if patch.trim().is_empty() {
      return Ok(FilePatchOutcome::Empty);
   }

   if patch_is_already_applied_to_index(patch, dir, index_file)? {
      return Ok(FilePatchOutcome::AlreadyApplied);
   }

   let output =
      run_git_apply(patch, &["apply", "--cached", "--3way", "--recount"], dir, index_file)?;
   if output.status.success() {
      return Ok(FilePatchOutcome::Staged);
   }

   Ok(FilePatchOutcome::Failed(String::from_utf8_lossy(&output.stderr).trim().to_string()))
}

/// Restore a single path's index entry to HEAD, discarding any partial or
/// conflicted staging left behind by a failed `git apply` (a 3-way apply leaves
/// unmerged index entries on conflict). The working-tree copy, holding the
/// user's divergent changes, is deliberately left untouched.
fn restore_index_path_to_head(path: &str, dir: &str, index_file: Option<&Path>) -> Result<()> {
   let output = git_command_for_index(index_file)
      .args(["reset", "-q", "HEAD", "--"])
      .arg(path)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to reset index entry {path}: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git reset failed for {path}: {stderr}")));
   }

   Ok(())
}

/// Resolve a (possibly abbreviated) blob id from a diff header to its full oid.
fn resolve_blob_oid(oid: &str, path: &str, dir: &str) -> Result<String> {
   let output = git_command()
      .args(["rev-parse", "--verify", "--quiet"])
      .arg(format!("{oid}^{{blob}}"))
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to resolve base blob for {path}: {e}")))?;

   let full = String::from_utf8_lossy(&output.stdout).trim().to_string();
   if !output.status.success() || full.is_empty() {
      return Err(CommitGenError::git(format!(
         "Cannot resolve base blob {oid} for {path}: object not found"
      )));
   }

   Ok(full)
}

/// Build the index entry that restores a file to its pre-change (base) blob,
/// taken from the snapshot's `index <base>..<target>` diff header.
fn base_index_blob(file: &ComposeFile, dir: &str) -> Result<IndexBlob> {
   let index_line = file
      .patch_header
      .lines()
      .find(|line| line.starts_with("index "))
      .ok_or_else(|| {
         CommitGenError::Other(format!("Cannot reset {} from base: no diff index line", file.path))
      })?;

   let rest = index_line.strip_prefix("index ").unwrap_or_default();
   let mut tokens = rest.split_whitespace();
   let range = tokens.next().unwrap_or_default();
   let mode_token = tokens.next();

   let base_oid = range
      .split_once("..")
      .map(|(base, _)| base)
      .ok_or_else(|| {
         CommitGenError::Other(format!(
            "Cannot parse base blob for {} from '{index_line}'",
            file.path
         ))
      })?;

   if base_oid.is_empty() || base_oid.bytes().all(|byte| byte == b'0') {
      return Err(CommitGenError::Other(format!(
         "{} is newly added and has no base blob to reset from",
         file.path
      )));
   }

   let mode = mode_token
      .map(str::to_string)
      .or_else(|| {
         file.patch_header.lines().find_map(|line| {
            line
               .strip_prefix("old mode ")
               .map(|mode| mode.trim().to_string())
         })
      })
      .unwrap_or_else(|| "100644".to_string());

   let full_oid = resolve_blob_oid(base_oid, &file.path, dir)?;
   Ok(IndexBlob { path: file.path.clone(), mode, object: IndexObject::ExistingObject(full_oid) })
}

/// Force a file's index entry to `base + the selected hunks`, ignoring the
/// current index/worktree state entirely.
///
/// The entry is pinned to the snapshot's base blob (the file's original HEAD
/// content) and the selected hunks are applied against that base. Because every
/// hunk is anchored in the base it was generated from, this applies cleanly
/// where a state-sensitive `git apply` against the live index would conflict.
/// The working tree is never touched: only the index is rewritten.
pub fn force_stage_file_from_base(
   snapshot: &ComposeSnapshot,
   file_id: &str,
   selected_hunk_ids: &[String],
   dir: &str,
) -> Result<()> {
   force_stage_file_from_base_with_index(snapshot, file_id, selected_hunk_ids, dir, None)
}

pub fn force_stage_file_from_base_in_index(
   snapshot: &ComposeSnapshot,
   file_id: &str,
   selected_hunk_ids: &[String],
   dir: &str,
   index_file: &Path,
) -> Result<()> {
   force_stage_file_from_base_with_index(
      snapshot,
      file_id,
      selected_hunk_ids,
      dir,
      Some(index_file),
   )
}

fn force_stage_file_from_base_with_index(
   snapshot: &ComposeSnapshot,
   file_id: &str,
   selected_hunk_ids: &[String],
   dir: &str,
   index_file: Option<&Path>,
) -> Result<()> {
   let file = snapshot
      .file_by_id(file_id)
      .ok_or_else(|| CommitGenError::Other(format!("Unknown compose file id {file_id}")))?;

   // Clear any conflicted residue, then pin the entry to the base blob.
   restore_index_path_to_head(&file.path, dir, index_file)?;
   let base = base_index_blob(file, dir)?;
   stage_index_blob(&base, dir, index_file)?;

   let ordered: Vec<&ComposeHunk> = file
      .hunk_ids
      .iter()
      .filter(|hunk_id| {
         selected_hunk_ids
            .iter()
            .any(|selected| selected == *hunk_id)
      })
      .filter_map(|hunk_id| snapshot.hunk_by_id(hunk_id))
      .collect();

   if ordered.is_empty() {
      return Ok(());
   }

   let patch = create_patch_for_file(file, &ordered);
   let output = run_git_apply(&patch, &["apply", "--cached", "--recount"], dir, index_file)?;
   if output.status.success() {
      return Ok(());
   }

   let stderr = String::from_utf8_lossy(&output.stderr);
   Err(CommitGenError::git(format!(
      "Failed to force-stage {} from base: {}",
      file.path,
      stderr.trim()
   )))
}

/// Stage specific files.
pub fn stage_files(files: &[String], dir: &str) -> Result<()> {
   stage_files_with_index(files, dir, None)
}

fn stage_files_with_index(files: &[String], dir: &str, index_file: Option<&Path>) -> Result<()> {
   if files.is_empty() {
      return Ok(());
   }

   let output = git_command_for_index(index_file)
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

fn hash_blob(contents: &str, path: &str, dir: &str) -> Result<String> {
   let mut child = git_command()
      .args(["hash-object", "-w", "--stdin"])
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
         .write_all(contents.as_bytes())
         .map_err(|e| CommitGenError::git(format!("Failed to write blob for {path}: {e}")))?;
   }

   let output = child
      .wait_with_output()
      .map_err(|e| CommitGenError::git(format!("Failed to wait for git hash-object: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git hash-object failed for {path}: {stderr}")));
   }

   let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
   if oid.is_empty() {
      return Err(CommitGenError::git(format!("git hash-object returned empty oid for {path}")));
   }

   Ok(oid)
}

fn index_blob_oid<'a>(blob: &'a IndexBlob, dir: &str) -> Result<Cow<'a, str>> {
   match &blob.object {
      IndexObject::BlobContents(contents) => Ok(Cow::Owned(hash_blob(contents, &blob.path, dir)?)),
      IndexObject::ExistingObject(oid) => Ok(Cow::Borrowed(oid.as_str())),
   }
}

fn index_entry_matches(
   path: &str,
   mode: &str,
   oid: &str,
   dir: &str,
   index_file: Option<&Path>,
) -> Result<bool> {
   let output = git_command_for_index(index_file)
      .args(["ls-files", "-s", "--"])
      .arg(path)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to inspect index entry {path}: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git ls-files failed for {path}: {stderr}")));
   }

   let stdout = String::from_utf8_lossy(&output.stdout);
   let Some(line) = stdout.lines().next() else {
      return Ok(false);
   };
   let mut parts = line.split_whitespace();
   Ok(parts.next() == Some(mode) && parts.next() == Some(oid))
}

fn stage_index_blob(blob: &IndexBlob, dir: &str, index_file: Option<&Path>) -> Result<StageResult> {
   let oid = index_blob_oid(blob, dir)?;
   if index_entry_matches(&blob.path, &blob.mode, oid.as_ref(), dir, index_file)? {
      return Ok(StageResult::AlreadyApplied);
   }

   let cacheinfo = format!("{},{},{}", blob.mode, oid, blob.path);
   let output = git_command_for_index(index_file)
      .args(["update-index", "--add", "--cacheinfo"])
      .arg(cacheinfo)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to stage blob {}: {e}", blob.path)))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!(
         "git update-index failed for {}: {stderr}",
         blob.path
      )));
   }

   Ok(StageResult::Staged)
}

/// Reset staging area.
pub fn reset_staging(dir: &str) -> Result<()> {
   let output = git_command()
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

fn parse_hunk_header(header: &str) -> Option<(usize, usize, usize, usize)> {
   let trimmed = header.trim();
   if !trimmed.starts_with("@@") {
      return None;
   }

   let after_first = trimmed.strip_prefix("@@")?;
   let middle = after_first.split("@@").next()?.trim();
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
         Some((s.parse().ok()?, 1))
      }
   };

   let (old_start, old_count) = parse_range(old_part)?;
   let (new_start, new_count) = parse_range(new_part)?;
   Some((old_start, old_count, new_start, new_count))
}

fn parse_file_path(diff_header: &str) -> Result<String> {
   diff_header
      .split_whitespace()
      .nth(3)
      .and_then(|part| part.strip_prefix("b/"))
      .map(str::to_string)
      .ok_or_else(|| {
         CommitGenError::Other(format!("Failed to parse file path from '{diff_header}'"))
      })
}

fn finalize_current_hunk(file: &mut ParsedFile, current_hunk: &mut Option<ParsedHunk>) {
   if let Some(hunk) = current_hunk.take() {
      file.hunks.push(hunk);
   }
}

fn finalize_current_file(
   files: &mut Vec<ParsedFile>,
   current_file: &mut Option<ParsedFile>,
   current_hunk: &mut Option<ParsedHunk>,
) {
   if let Some(mut file) = current_file.take() {
      finalize_current_hunk(&mut file, current_hunk);
      files.push(file);
   }
}

fn join_lines(lines: &[String]) -> String {
   if lines.is_empty() {
      String::new()
   } else {
      let mut joined = lines.join("\n");
      joined.push('\n');
      joined
   }
}

fn diff_lines_preserve_cr(input: &str) -> impl Iterator<Item = &str> {
   input
      .split_inclusive('\n')
      .map(|line| line.strip_suffix('\n').unwrap_or(line))
}

fn truncate_snippet(snippet: &str, max_chars: usize) -> String {
   let trimmed = snippet.trim();
   if trimmed.chars().count() <= max_chars {
      return trimmed.to_string();
   }

   let mut truncated = trimmed.chars().take(max_chars).collect::<String>();
   truncated.push_str("...");
   truncated
}

fn build_hunk_snippet(lines: &[String], fallback: &str) -> String {
   let interesting: Vec<String> = lines
      .iter()
      .skip(1)
      .filter(|line| line.starts_with('+') || line.starts_with('-'))
      .take(3)
      .map(|line| truncate_snippet(line.trim_start_matches(['+', '-']), 80))
      .collect();

   if interesting.is_empty() {
      truncate_snippet(fallback, 80)
   } else {
      interesting.join(" | ")
   }
}

fn build_synthetic_snippet(file: &ParsedFile) -> String {
   let header_text = file
      .header_lines
      .iter()
      .skip(1)
      .find(|line| {
         !line.starts_with("index ")
            && !line.starts_with("--- ")
            && !line.starts_with("+++ ")
            && !line.trim().is_empty()
      })
      .cloned()
      .unwrap_or_else(|| format!("whole-file change in {}", file.path));

   truncate_snippet(&header_text, 80)
}

fn fnv1a_64(input: &str) -> String {
   let mut hash = 0xcbf29ce484222325_u64;
   for byte in input.as_bytes() {
      hash ^= u64::from(*byte);
      hash = hash.wrapping_mul(0x100000001b3);
   }
   format!("{hash:016x}")
}

fn build_semantic_key(path: &str, lines: &[String], fallback: &str) -> String {
   let mut changed = Vec::new();
   for line in lines {
      if (line.starts_with('+') && !line.starts_with("+++"))
         || (line.starts_with('-') && !line.starts_with("---"))
      {
         changed.push(line.clone());
      }
   }

   let source = if changed.is_empty() {
      fallback.to_string()
   } else {
      changed.join("\n")
   };

   format!("{path}:{}", fnv1a_64(&source))
}

pub fn build_compose_snapshot(diff: &str, stat: &str) -> Result<ComposeSnapshot> {
   let mut files = Vec::new();
   let mut current_file: Option<ParsedFile> = None;
   let mut current_hunk: Option<ParsedHunk> = None;

   for line in diff_lines_preserve_cr(diff) {
      if line.starts_with("diff --git ") {
         finalize_current_file(&mut files, &mut current_file, &mut current_hunk);
         current_file = Some(ParsedFile {
            path:         parse_file_path(line)?,
            header_lines: vec![line.to_string()],
            hunks:        Vec::new(),
            additions:    0,
            deletions:    0,
            is_binary:    false,
         });
         continue;
      }

      let Some(file) = &mut current_file else {
         continue;
      };

      if line.starts_with("@@ ") {
         finalize_current_hunk(file, &mut current_hunk);
         let (old_start, old_count, new_start, new_count) =
            parse_hunk_header(line).ok_or_else(|| {
               CommitGenError::Other(format!("Failed to parse hunk header '{line}'"))
            })?;
         current_hunk = Some(ParsedHunk {
            old_start,
            old_count,
            new_start,
            new_count,
            header: line.to_string(),
            lines: vec![line.to_string()],
         });
         continue;
      }

      if let Some(hunk) = &mut current_hunk {
         if line.starts_with('+') {
            file.additions += 1;
         } else if line.starts_with('-') {
            file.deletions += 1;
         }

         hunk.lines.push(line.to_string());
         continue;
      }

      if line.starts_with("Binary files ") {
         file.is_binary = true;
      }
      file.header_lines.push(line.to_string());
   }

   finalize_current_file(&mut files, &mut current_file, &mut current_hunk);

   let mut snapshot_files = Vec::new();
   let mut snapshot_hunks = Vec::new();

   for (file_index, file) in files.into_iter().enumerate() {
      let file_id = format!("F{:03}", file_index + 1);
      let patch_header = join_lines(&file.header_lines);
      let mut full_patch = patch_header.clone();
      let mut hunk_ids = Vec::new();

      if file.hunks.is_empty() {
         let hunk_id = format!("{file_id}-H001");
         let snippet = build_synthetic_snippet(&file);
         let semantic_key = build_semantic_key(&file.path, &file.header_lines, &snippet);
         hunk_ids.push(hunk_id.clone());
         snapshot_hunks.push(ComposeHunk {
            hunk_id,
            file_id: file_id.clone(),
            path: file.path.clone(),
            old_start: 0,
            old_count: 0,
            new_start: 0,
            new_count: 0,
            header: snippet.clone(),
            raw_patch: String::new(),
            snippet,
            semantic_key,
            synthetic: true,
         });
      } else {
         for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            let hunk_id = format!("{file_id}-H{:03}", hunk_index + 1);
            let raw_patch = join_lines(&hunk.lines);
            let snippet = build_hunk_snippet(&hunk.lines, &hunk.header);
            let semantic_key = build_semantic_key(&file.path, &hunk.lines, &snippet);

            full_patch.push_str(&raw_patch);
            hunk_ids.push(hunk_id.clone());
            snapshot_hunks.push(ComposeHunk {
               hunk_id,
               file_id: file_id.clone(),
               path: file.path.clone(),
               old_start: hunk.old_start,
               old_count: hunk.old_count,
               new_start: hunk.new_start,
               new_count: hunk.new_count,
               header: hunk.header.clone(),
               raw_patch,
               snippet,
               semantic_key,
               synthetic: false,
            });
         }
      }

      let hunk_word = if hunk_ids.len() == 1 { "hunk" } else { "hunks" };
      let summary = format!(
         "{} (+{}/-{}, {} {})",
         file.path,
         file.additions,
         file.deletions,
         hunk_ids.len(),
         hunk_word
      );

      snapshot_files.push(ComposeFile {
         file_id,
         path: file.path,
         patch_header,
         full_patch,
         summary,
         hunk_ids,
         additions: file.additions,
         deletions: file.deletions,
         is_binary: file.is_binary,
         synthetic_only: file.hunks.is_empty(),
      });
   }

   Ok(ComposeSnapshot {
      diff:  diff.to_string(),
      stat:  stat.to_string(),
      files: snapshot_files,
      hunks: snapshot_hunks,
   })
}

fn create_patch_for_file(file: &ComposeFile, hunks: &[&ComposeHunk]) -> String {
   let mut patch = file.patch_header.clone();
   for hunk in hunks {
      patch.push_str(&hunk.raw_patch);
   }
   patch
}

fn selected_hunks_by_file<'a>(
   snapshot: &'a ComposeSnapshot,
   group: &ComposeExecutableGroup,
) -> Result<BTreeMap<String, Vec<&'a ComposeHunk>>> {
   if group.hunk_ids.is_empty() {
      return Err(CommitGenError::Other(format!("Group {} has no assigned hunks", group.group_id)));
   }

   let mut selected_by_file: BTreeMap<String, Vec<&ComposeHunk>> = BTreeMap::new();
   for hunk_id in &group.hunk_ids {
      let hunk = snapshot.hunk_by_id(hunk_id).ok_or_else(|| {
         CommitGenError::Other(format!(
            "Group {} references unknown hunk id {hunk_id}",
            group.group_id
         ))
      })?;
      selected_by_file
         .entry(hunk.file_id.clone())
         .or_default()
         .push(hunk);
   }

   Ok(selected_by_file)
}

fn ordered_selected_hunks<'a>(
   file: &ComposeFile,
   selected_for_file: &[&'a ComposeHunk],
) -> Result<Vec<&'a ComposeHunk>> {
   let ordered_hunks: Vec<&ComposeHunk> = file
      .hunk_ids
      .iter()
      .filter_map(|hunk_id| {
         selected_for_file
            .iter()
            .find(|hunk| hunk.hunk_id == *hunk_id)
            .copied()
      })
      .collect();

   if ordered_hunks.is_empty() {
      return Err(CommitGenError::Other(format!("Selected no patchable hunks for {}", file.path)));
   }

   Ok(ordered_hunks)
}

fn selected_hunks_cover_file(file: &ComposeFile, selected_for_file: &[&ComposeHunk]) -> bool {
   let selected_ids: HashSet<&str> = selected_for_file
      .iter()
      .map(|hunk| hunk.hunk_id.as_str())
      .collect();
   let file_hunk_ids: HashSet<&str> = file.hunk_ids.iter().map(String::as_str).collect();
   selected_ids == file_hunk_ids
}

fn count_hunk_changes(hunk: &ComposeHunk) -> (usize, usize) {
   let mut additions = 0_usize;
   let mut deletions = 0_usize;

   for line in hunk.raw_patch.lines() {
      if line.starts_with('+') {
         additions += 1;
      } else if line.starts_with('-') {
         deletions += 1;
      }
   }

   (additions, deletions)
}

fn push_stat_line(
   stat: &mut String,
   path: &str,
   additions: usize,
   deletions: usize,
   is_binary: bool,
) {
   use std::fmt::Write;

   if is_binary && additions == 0 && deletions == 0 {
      writeln!(stat, " {path} | Bin").unwrap();
      return;
   }

   let change_count = additions + deletions;
   let pluses = "+".repeat(additions.min(50));
   let minuses = "-".repeat(deletions.min(50));
   writeln!(stat, " {path} | {change_count} {pluses}{minuses}").unwrap();
}

fn new_file_mode(file: &ComposeFile) -> Option<&str> {
   file
      .patch_header
      .lines()
      .find_map(|line| line.strip_prefix("new file mode ").map(str::trim))
}

fn validate_new_file_mode(file: &ComposeFile) -> Result<String> {
   let mode = new_file_mode(file).unwrap_or("100644");
   if matches!(mode, "100644" | "100755" | "120000" | "160000") {
      Ok(mode.to_string())
   } else {
      Err(CommitGenError::Other(format!("Invalid new file mode {mode:?} for {}", file.path)))
   }
}

fn materialize_new_file_contents(hunks: &[&ComposeHunk]) -> String {
   let mut contents = String::new();
   let mut last_emitted_line_had_newline = false;

   for hunk in hunks {
      for line in diff_lines_preserve_cr(&hunk.raw_patch) {
         if line.starts_with("@@") {
            last_emitted_line_had_newline = false;
            continue;
         }

         if line == r"\ No newline at end of file" {
            if last_emitted_line_had_newline {
               contents.pop();
               last_emitted_line_had_newline = false;
            }
            continue;
         }

         if let Some(added) = line.strip_prefix('+') {
            contents.push_str(added);
            contents.push('\n');
            last_emitted_line_had_newline = true;
         } else if let Some(context) = line.strip_prefix(' ') {
            contents.push_str(context);
            contents.push('\n');
            last_emitted_line_had_newline = true;
         } else {
            last_emitted_line_had_newline = false;
         }
      }
   }

   contents
}

fn new_file_index_oid(file: &ComposeFile) -> Option<&str> {
   file.patch_header.lines().find_map(|line| {
      let index_range = line.strip_prefix("index ")?;
      let (_, new_oid) = index_range.split_once("..")?;
      new_oid.split_whitespace().next()
   })
}

fn validate_git_object_id(oid: &str, file: &ComposeFile) -> Result<String> {
   let oid = oid.trim();
   if !oid.is_empty()
      && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
      && oid.bytes().any(|byte| byte != b'0')
   {
      Ok(oid.to_string())
   } else {
      Err(CommitGenError::Other(format!("Invalid gitlink object id {oid:?} for {}", file.path)))
   }
}

fn materialize_gitlink_oid(file: &ComposeFile, hunks: &[&ComposeHunk]) -> Result<String> {
   let contents = materialize_new_file_contents(hunks);
   if let Some(oid) = contents.lines().find_map(|line| {
      line
         .strip_prefix("Subproject commit ")
         .and_then(|rest| rest.split_whitespace().next())
   }) {
      return validate_git_object_id(oid, file);
   }

   if let Some(oid) = new_file_index_oid(file) {
      return validate_git_object_id(oid, file);
   }

   Err(CommitGenError::Other(format!("Missing gitlink object id for {}", file.path)))
}

fn new_file_index_blob(file: &ComposeFile, hunks: &[&ComposeHunk]) -> Result<IndexBlob> {
   let mode = validate_new_file_mode(file)?;
   let object = if mode == "160000" {
      IndexObject::ExistingObject(materialize_gitlink_oid(file, hunks)?)
   } else {
      IndexObject::BlobContents(materialize_new_file_contents(hunks))
   };

   Ok(IndexBlob { path: file.path.clone(), mode, object })
}

pub fn create_executable_group_patch(
   snapshot: &ComposeSnapshot,
   group: &ComposeExecutableGroup,
) -> Result<ComposeGroupPatch> {
   let selected_by_file = selected_hunks_by_file(snapshot, group)?;
   let mut fallback_files = Vec::new();
   let mut diff = String::new();
   let mut stat = String::new();
   let mut apply_patches: Vec<FilePatch> = Vec::new();
   let mut index_blobs = Vec::new();

   for file in &snapshot.files {
      let Some(selected_for_file) = selected_by_file.get(&file.file_id) else {
         continue;
      };

      let ordered_hunks = ordered_selected_hunks(file, selected_for_file).map_err(|_| {
         CommitGenError::Other(format!(
            "Group {} selected no patchable hunks for {}",
            group.group_id, file.path
         ))
      })?;

      if file.synthetic_only || file.is_binary {
         if selected_hunks_cover_file(file, selected_for_file) {
            if file.synthetic_only && !file.is_binary && new_file_mode(file).is_some() {
               index_blobs.push(new_file_index_blob(file, &ordered_hunks)?);
            } else {
               fallback_files.push(file.path.clone());
            }
            diff.push_str(&file.full_patch);
            push_stat_line(&mut stat, &file.path, file.additions, file.deletions, file.is_binary);
            continue;
         }

         return Err(CommitGenError::Other(format!(
            "Group {} cannot partially stage unpatchable file {}",
            group.group_id, file.path
         )));
      }

      let file_patch = create_patch_for_file(file, &ordered_hunks);
      let (additions, deletions) = ordered_hunks.iter().fold(
         (0_usize, 0_usize),
         |(total_additions, total_deletions), hunk| {
            let (hunk_additions, hunk_deletions) = count_hunk_changes(hunk);
            (total_additions + hunk_additions, total_deletions + hunk_deletions)
         },
      );
      diff.push_str(&file_patch);
      if new_file_mode(file).is_some() && selected_hunks_cover_file(file, selected_for_file) {
         index_blobs.push(new_file_index_blob(file, &ordered_hunks)?);
      } else {
         apply_patches.push(FilePatch { path: file.path.clone(), patch: file_patch });
      }
      push_stat_line(&mut stat, &file.path, additions, deletions, false);
   }

   fallback_files.sort();
   fallback_files.dedup();

   Ok(ComposeGroupPatch { diff, stat, apply_patches, fallback_files, index_blobs })
}

pub fn stage_executable_group(
   snapshot: &ComposeSnapshot,
   group: &ComposeExecutableGroup,
   dir: &str,
) -> Result<ComposeStageOutcome> {
   stage_executable_group_with_index(snapshot, group, dir, None)
}

pub fn stage_executable_group_in_index(
   snapshot: &ComposeSnapshot,
   group: &ComposeExecutableGroup,
   dir: &str,
   index_file: &Path,
) -> Result<ComposeStageOutcome> {
   stage_executable_group_with_index(snapshot, group, dir, Some(index_file))
}

fn stage_executable_group_with_index(
   snapshot: &ComposeSnapshot,
   group: &ComposeExecutableGroup,
   dir: &str,
   index_file: Option<&Path>,
) -> Result<ComposeStageOutcome> {
   let group_patch = create_executable_group_patch(snapshot, group)?;
   let mut result = StageResult::EmptyPatch;
   let mut skipped = Vec::new();

   for file_patch in &group_patch.apply_patches {
      match apply_file_patch_to_index(&file_patch.patch, dir, index_file)? {
         FilePatchOutcome::Staged => result = result.combine(StageResult::Staged),
         FilePatchOutcome::AlreadyApplied => {
            result = result.combine(StageResult::AlreadyApplied);
         },
         FilePatchOutcome::Empty => result = result.combine(StageResult::EmptyPatch),
         FilePatchOutcome::Failed(reason) => {
            // The planned patch no longer applies against the current state.
            // Drop any conflicted index residue and keep the worktree change.
            restore_index_path_to_head(&file_patch.path, dir, index_file)?;
            skipped.push(SkippedFile { path: file_patch.path.clone(), reason });
         },
      }
   }

   if !group_patch.fallback_files.is_empty() {
      stage_files_with_index(&group_patch.fallback_files, dir, index_file)?;
      result = result.combine(StageResult::Staged);
   }

   for blob in &group_patch.index_blobs {
      result = result.combine(stage_index_blob(blob, dir, index_file)?);
   }

   Ok(ComposeStageOutcome { result, skipped })
}

#[cfg(test)]
mod tests {
   use std::fs;

   use tempfile::TempDir;

   use super::*;
   use crate::{
      compose_types::ComposeExecutableGroup,
      git::{TempGitIndex, get_compose_diff, get_compose_stat, read_tree_into_index},
      types::CommitType,
   };

   fn write_file(dir: &TempDir, path: &str, contents: &str) {
      let full_path = dir.path().join(path);
      if let Some(parent) = full_path.parent() {
         fs::create_dir_all(parent).unwrap();
      }
      fs::write(full_path, contents).unwrap();
   }

   fn run_git(dir: &TempDir, args: &[&str]) -> String {
      let output = git_command()
         .args(args)
         .current_dir(dir.path())
         .output()
         .unwrap_or_else(|err| panic!("git {args:?} failed to spawn: {err}"));

      assert!(
         output.status.success(),
         "git {:?} failed: stdout={} stderr={}",
         args,
         String::from_utf8_lossy(&output.stdout),
         String::from_utf8_lossy(&output.stderr)
      );

      String::from_utf8_lossy(&output.stdout).to_string()
   }

   fn init_repo() -> TempDir {
      let dir = TempDir::new().unwrap();
      run_git(&dir, &["init"]);
      run_git(&dir, &["config", "user.name", "Compose Test"]);
      run_git(&dir, &["config", "user.email", "compose@test.local"]);
      run_git(&dir, &["config", "commit.gpgsign", "false"]);
      dir
   }

   fn fixture_file_original() -> String {
      [
         "fn alpha() {",
         "    println!(\"alpha\");",
         "}",
         "",
         "// spacer 1",
         "// spacer 2",
         "// spacer 3",
         "// spacer 4",
         "// spacer 5",
         "// spacer 6",
         "// spacer 7",
         "// spacer 8",
         "fn beta() {",
         "    println!(\"beta\");",
         "}",
         "",
      ]
      .join("\n")
   }

   fn fixture_file_stage_only() -> String {
      fixture_file_original().replace("alpha", "alpha staged")
   }

   fn fixture_file_stage_and_unstaged() -> String {
      fixture_file_stage_only().replace("beta", "beta unstaged")
   }

   fn fixture_file_two_hunks() -> String {
      [
         "fn alpha() {",
         "    println!(\"alpha changed\");",
         "}",
         "",
         "// spacer 1",
         "// spacer 2",
         "// spacer 3",
         "// spacer 4",
         "// spacer 5",
         "// spacer 6",
         "// spacer 7",
         "// spacer 8",
         "fn beta() {",
         "    println!(\"beta changed\");",
         "}",
         "",
      ]
      .join("\n")
   }

   fn commit_all(dir: &TempDir, message: &str) {
      run_git(dir, &["add", "."]);
      run_git(dir, &["commit", "-m", message]);
   }

   fn staged_diff(dir: &TempDir) -> String {
      run_git(dir, &["diff", "--cached"])
   }

   fn staged_diff_in_index(dir: &TempDir, index: &TempGitIndex) -> String {
      let output = crate::git::git_command_with_index(index.path())
         .args(["diff", "--cached"])
         .current_dir(dir.path())
         .output()
         .unwrap();
      assert!(
         output.status.success(),
         "git diff --cached with temp index failed: {}",
         String::from_utf8_lossy(&output.stderr)
      );
      String::from_utf8_lossy(&output.stdout).to_string()
   }

   #[test]
   fn test_build_compose_snapshot_stable_ids() {
      let diff = r#"diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
-fn alpha() {
+fn alpha_changed() {
     println!("alpha");
 }
diff --git a/tests/lib.rs b/tests/lib.rs
index 3333333..4444444 100644
--- a/tests/lib.rs
+++ b/tests/lib.rs
@@ -10,3 +10,4 @@
 fn test_it() {
+    assert!(true);
 }
"#;

      let stat = " src/lib.rs | 2 +-\n tests/lib.rs | 1 +\n";
      let first = build_compose_snapshot(diff, stat).unwrap();
      let second = build_compose_snapshot(diff, stat).unwrap();

      assert_eq!(first.files.len(), 2);
      assert_eq!(
         first
            .files
            .iter()
            .map(|file| file.file_id.clone())
            .collect::<Vec<_>>(),
         second
            .files
            .iter()
            .map(|file| file.file_id.clone())
            .collect::<Vec<_>>()
      );
      assert_eq!(
         first
            .hunks
            .iter()
            .map(|hunk| hunk.hunk_id.clone())
            .collect::<Vec<_>>(),
         second
            .hunks
            .iter()
            .map(|hunk| hunk.hunk_id.clone())
            .collect::<Vec<_>>()
      );
   }

   #[test]
   fn test_get_compose_diff_merges_staged_unstaged_and_untracked() {
      let dir = init_repo();
      write_file(&dir, "src/lib.rs", &fixture_file_original());
      commit_all(&dir, "initial");

      write_file(&dir, "src/lib.rs", &fixture_file_stage_only());
      run_git(&dir, &["add", "src/lib.rs"]);
      write_file(&dir, "src/lib.rs", &fixture_file_stage_and_unstaged());
      write_file(&dir, "notes.txt", "new untracked file\n");

      let diff = get_compose_diff(dir.path().to_str().unwrap()).unwrap();
      let stat = get_compose_stat(dir.path().to_str().unwrap()).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();

      assert_eq!(snapshot.files.len(), 2);
      assert!(snapshot.file_by_path("src/lib.rs").is_some());
      assert!(snapshot.file_by_path("notes.txt").is_some());

      let source_file = snapshot.file_by_path("src/lib.rs").unwrap();
      assert!(
         source_file.hunk_ids.len() >= 2,
         "expected staged + unstaged edits in one file to produce multiple hunks"
      );
   }

   #[test]
   fn test_stage_executable_group_partial_hunk_from_one_file() {
      let dir = init_repo();
      write_file(&dir, "src/lib.rs", &fixture_file_original());
      commit_all(&dir, "initial");
      write_file(&dir, "src/lib.rs", &fixture_file_two_hunks());

      let diff = get_compose_diff(dir.path().to_str().unwrap()).unwrap();
      let stat = get_compose_stat(dir.path().to_str().unwrap()).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();
      let source_file = snapshot.file_by_path("src/lib.rs").unwrap();
      assert_eq!(source_file.hunk_ids.len(), 2);

      reset_staging(dir.path().to_str().unwrap()).unwrap();
      let group = ComposeExecutableGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     vec![source_file.file_id.clone()],
         rationale:    "first hunk".to_string(),
         dependencies: vec![],
         hunk_ids:     vec![source_file.hunk_ids[0].clone()],
      };
      stage_executable_group(&snapshot, &group, dir.path().to_str().unwrap()).unwrap();

      let staged = staged_diff(&dir);
      assert!(staged.contains("alpha changed"));
      assert!(!staged.contains("beta changed"));
   }

   #[test]
   fn test_stage_executable_group_across_sequential_commits_same_file() {
      let dir = init_repo();
      write_file(&dir, "src/lib.rs", &fixture_file_original());
      commit_all(&dir, "initial");
      write_file(&dir, "src/lib.rs", &fixture_file_two_hunks());

      let diff = get_compose_diff(dir.path().to_str().unwrap()).unwrap();
      let stat = get_compose_stat(dir.path().to_str().unwrap()).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();
      let source_file = snapshot.file_by_path("src/lib.rs").unwrap();
      assert_eq!(source_file.hunk_ids.len(), 2);

      let first_group = ComposeExecutableGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     vec![source_file.file_id.clone()],
         rationale:    "first hunk".to_string(),
         dependencies: vec![],
         hunk_ids:     vec![source_file.hunk_ids[0].clone()],
      };
      let second_group = ComposeExecutableGroup {
         group_id:     "G2".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     vec![source_file.file_id.clone()],
         rationale:    "second hunk".to_string(),
         dependencies: vec![],
         hunk_ids:     vec![source_file.hunk_ids[1].clone()],
      };

      reset_staging(dir.path().to_str().unwrap()).unwrap();
      stage_executable_group(&snapshot, &first_group, dir.path().to_str().unwrap()).unwrap();
      run_git(&dir, &["commit", "-m", "first"]);

      stage_executable_group(&snapshot, &second_group, dir.path().to_str().unwrap()).unwrap();
      let staged = staged_diff(&dir);
      assert!(staged.contains("beta changed"));
      assert!(!staged.contains("alpha changed"));
   }

   #[test]
   fn test_create_executable_group_patch_derives_diff_without_staging() {
      let dir = init_repo();
      write_file(&dir, "src/lib.rs", &fixture_file_original());
      commit_all(&dir, "initial");
      write_file(&dir, "src/lib.rs", &fixture_file_two_hunks());

      let diff = get_compose_diff(dir.path().to_str().unwrap()).unwrap();
      let stat = get_compose_stat(dir.path().to_str().unwrap()).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();
      let source_file = snapshot.file_by_path("src/lib.rs").unwrap();
      let group = ComposeExecutableGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     vec![source_file.file_id.clone()],
         rationale:    "first hunk".to_string(),
         dependencies: vec![],
         hunk_ids:     vec![source_file.hunk_ids[0].clone()],
      };

      reset_staging(dir.path().to_str().unwrap()).unwrap();
      let group_patch = create_executable_group_patch(&snapshot, &group).unwrap();

      assert!(staged_diff(&dir).trim().is_empty());
      assert!(group_patch.diff.contains("alpha changed"));
      assert!(!group_patch.diff.contains("beta changed"));
      assert!(group_patch.stat.contains("src/lib.rs | 2 +-"));
   }

   #[test]
   fn test_stage_executable_groups_ignore_unplanned_files_between_commits() {
      let dir = init_repo();
      write_file(&dir, "src/a.rs", "fn a() {}\n");
      write_file(&dir, "src/b.rs", "fn b() {}\n");
      commit_all(&dir, "initial");
      write_file(&dir, "src/a.rs", "fn a_changed() {}\n");
      write_file(&dir, "src/b.rs", "fn b_changed() {}\n");

      let diff = get_compose_diff(dir.path().to_str().unwrap()).unwrap();
      let stat = get_compose_stat(dir.path().to_str().unwrap()).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();
      let first_file = snapshot.file_by_path("src/a.rs").unwrap();
      let second_file = snapshot.file_by_path("src/b.rs").unwrap();
      let first_group = ComposeExecutableGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     vec![first_file.file_id.clone()],
         rationale:    "first file".to_string(),
         dependencies: vec![],
         hunk_ids:     first_file.hunk_ids.clone(),
      };
      let second_group = ComposeExecutableGroup {
         group_id:     "G2".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     vec![second_file.file_id.clone()],
         rationale:    "second file".to_string(),
         dependencies: vec![],
         hunk_ids:     second_file.hunk_ids.clone(),
      };

      reset_staging(dir.path().to_str().unwrap()).unwrap();
      assert_eq!(
         stage_executable_group(&snapshot, &first_group, dir.path().to_str().unwrap())
            .unwrap()
            .result,
         StageResult::Staged
      );
      run_git(&dir, &["commit", "-m", "first"]);
      write_file(&dir, "Dockerfile", "FROM scratch\n");

      assert_eq!(
         stage_executable_group(&snapshot, &second_group, dir.path().to_str().unwrap())
            .unwrap()
            .result,
         StageResult::Staged
      );
      let staged = staged_diff(&dir);
      assert!(staged.contains("b_changed"));
      assert!(!staged.contains("Dockerfile"));
      run_git(&dir, &["commit", "-m", "second"]);

      assert!(
         get_compose_diff(dir.path().to_str().unwrap())
            .unwrap()
            .contains("Dockerfile")
      );
   }

   #[test]
   fn test_stage_executable_group_ignores_same_file_local_edit_between_commits() {
      let dir = init_repo();
      write_file(&dir, "src/lib.rs", &fixture_file_original());
      commit_all(&dir, "initial");
      write_file(&dir, "src/lib.rs", &fixture_file_two_hunks());

      let diff = get_compose_diff(dir.path().to_str().unwrap()).unwrap();
      let stat = get_compose_stat(dir.path().to_str().unwrap()).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();
      let source_file = snapshot.file_by_path("src/lib.rs").unwrap();
      let first_group = ComposeExecutableGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     vec![source_file.file_id.clone()],
         rationale:    "first hunk".to_string(),
         dependencies: vec![],
         hunk_ids:     vec![source_file.hunk_ids[0].clone()],
      };
      let second_group = ComposeExecutableGroup {
         group_id:     "G2".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     vec![source_file.file_id.clone()],
         rationale:    "second hunk".to_string(),
         dependencies: vec![],
         hunk_ids:     vec![source_file.hunk_ids[1].clone()],
      };

      reset_staging(dir.path().to_str().unwrap()).unwrap();
      stage_executable_group(&snapshot, &first_group, dir.path().to_str().unwrap()).unwrap();
      run_git(&dir, &["commit", "-m", "first"]);
      write_file(
         &dir,
         "src/lib.rs",
         &fixture_file_two_hunks().replace("// spacer 4", "// local edit"),
      );

      stage_executable_group(&snapshot, &second_group, dir.path().to_str().unwrap()).unwrap();
      let staged = staged_diff(&dir);
      assert!(staged.contains("beta changed"));
      assert!(!staged.contains("local edit"));
   }

   #[test]
   fn test_stage_executable_group_noops_when_snapshot_patch_already_applied() {
      let dir = init_repo();
      write_file(&dir, "src/lib.rs", &fixture_file_original());
      commit_all(&dir, "initial");
      write_file(&dir, "src/lib.rs", &fixture_file_stage_only());

      let diff = get_compose_diff(dir.path().to_str().unwrap()).unwrap();
      let stat = get_compose_stat(dir.path().to_str().unwrap()).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();
      let source_file = snapshot.file_by_path("src/lib.rs").unwrap();
      let group = ComposeExecutableGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     vec![source_file.file_id.clone()],
         rationale:    "all hunks".to_string(),
         dependencies: vec![],
         hunk_ids:     source_file.hunk_ids.clone(),
      };

      reset_staging(dir.path().to_str().unwrap()).unwrap();
      let first_result =
         stage_executable_group(&snapshot, &group, dir.path().to_str().unwrap()).unwrap();
      assert_eq!(first_result.result, StageResult::Staged);
      run_git(&dir, &["commit", "-m", "applied"]);

      let second_result =
         stage_executable_group(&snapshot, &group, dir.path().to_str().unwrap()).unwrap();
      assert_eq!(second_result.result, StageResult::AlreadyApplied);
      assert!(staged_diff(&dir).trim().is_empty());
   }

   #[test]
   fn test_stage_executable_group_reuses_snapshot_patch_not_worktree_contents() {
      let dir = init_repo();
      write_file(&dir, "README.md", "initial\n");
      commit_all(&dir, "initial");
      write_file(&dir, "notes.txt", "planned\n");

      let diff = get_compose_diff(dir.path().to_str().unwrap()).unwrap();
      let stat = get_compose_stat(dir.path().to_str().unwrap()).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();
      let notes_file = snapshot.file_by_path("notes.txt").unwrap();
      let group = ComposeExecutableGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("docs").unwrap(),
         scope:        None,
         file_ids:     vec![notes_file.file_id.clone()],
         rationale:    "new notes".to_string(),
         dependencies: vec![],
         hunk_ids:     notes_file.hunk_ids.clone(),
      };

      reset_staging(dir.path().to_str().unwrap()).unwrap();
      let planned_result =
         stage_executable_group(&snapshot, &group, dir.path().to_str().unwrap()).unwrap();
      assert_eq!(planned_result.result, StageResult::Staged);
      let planned_staged = staged_diff(&dir);
      assert!(planned_staged.contains("+planned"));
      assert!(!planned_staged.contains("local edit"));

      reset_staging(dir.path().to_str().unwrap()).unwrap();
      write_file(&dir, "notes.txt", "planned\nlocal edit\n");
      let reused_result =
         stage_executable_group(&snapshot, &group, dir.path().to_str().unwrap()).unwrap();
      assert_eq!(reused_result.result, StageResult::Staged);
      let reused_staged = staged_diff(&dir);

      assert_eq!(reused_staged, planned_staged);
      assert!(!reused_staged.contains("local edit"));
   }

   #[test]
   fn test_stage_executable_group_materializes_new_file_from_snapshot() {
      let dir = init_repo();
      write_file(&dir, "README.md", "initial\n");
      commit_all(&dir, "initial");

      let diff = r"diff --git a/notes.txt b/notes.txt
new file mode 100644
index 0000000..0000000
--- /dev/null
+++ b/notes.txt
@@ -1,1 +1,3 @@
-old
+old
+new
+++literal plus
";
      let stat = " notes.txt | 4 +++-\n";
      let snapshot = build_compose_snapshot(diff, stat).unwrap();
      let notes_file = snapshot.file_by_path("notes.txt").unwrap();
      let group = ComposeExecutableGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("docs").unwrap(),
         scope:        None,
         file_ids:     vec![notes_file.file_id.clone()],
         rationale:    "new notes".to_string(),
         dependencies: vec![],
         hunk_ids:     notes_file.hunk_ids.clone(),
      };

      write_file(&dir, "notes.txt", "worktree edit\n");
      reset_staging(dir.path().to_str().unwrap()).unwrap();
      let result = stage_executable_group(&snapshot, &group, dir.path().to_str().unwrap()).unwrap();

      assert_eq!(result.result, StageResult::Staged);
      let staged = staged_diff(&dir);
      assert!(staged.contains("+old"));
      assert!(staged.contains("+new"));
      assert!(staged.contains("+++literal plus"));
      assert!(!staged.contains("worktree edit"));
      let second_result =
         stage_executable_group(&snapshot, &group, dir.path().to_str().unwrap()).unwrap();
      assert_eq!(second_result.result, StageResult::AlreadyApplied);
   }

   #[test]
   fn test_stage_executable_group_materializes_empty_new_file_from_snapshot() {
      let dir = init_repo();
      write_file(&dir, "README.md", "initial\n");
      commit_all(&dir, "initial");

      let diff = r"diff --git a/empty.txt b/empty.txt
new file mode 100644
index 0000000..0000000
--- /dev/null
+++ b/empty.txt
";
      let stat = " empty.txt | 0\n";
      let snapshot = build_compose_snapshot(diff, stat).unwrap();
      let empty_file = snapshot.file_by_path("empty.txt").unwrap();
      let group = ComposeExecutableGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("docs").unwrap(),
         scope:        None,
         file_ids:     vec![empty_file.file_id.clone()],
         rationale:    "empty notes".to_string(),
         dependencies: vec![],
         hunk_ids:     empty_file.hunk_ids.clone(),
      };

      write_file(&dir, "empty.txt", "worktree edit\n");
      reset_staging(dir.path().to_str().unwrap()).unwrap();
      let result = stage_executable_group(&snapshot, &group, dir.path().to_str().unwrap()).unwrap();

      assert_eq!(result.result, StageResult::Staged);
      let staged = staged_diff(&dir);
      assert!(staged.contains("new file mode 100644"));
      assert!(!staged.contains("worktree edit"));
   }

   #[test]
   fn test_stage_executable_group_materializes_new_gitlink_from_snapshot() {
      let dir = init_repo();
      write_file(&dir, "README.md", "initial\n");
      commit_all(&dir, "initial");

      let oid = "1234567890abcdef1234567890abcdef12345678";
      let diff = format!(
         "diff --git a/vendor/lib b/vendor/lib\nnew file mode 160000\nindex 0000000..{oid}\n--- \
          /dev/null\n+++ b/vendor/lib\n@@ -0,0 +1 @@\n+Subproject commit {oid}\n"
      );
      let stat = " vendor/lib | 1 +\n";
      let snapshot = build_compose_snapshot(&diff, stat).unwrap();
      let gitlink_file = snapshot.file_by_path("vendor/lib").unwrap();
      let group = ComposeExecutableGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("chore").unwrap(),
         scope:        None,
         file_ids:     vec![gitlink_file.file_id.clone()],
         rationale:    "add submodule".to_string(),
         dependencies: vec![],
         hunk_ids:     gitlink_file.hunk_ids.clone(),
      };

      reset_staging(dir.path().to_str().unwrap()).unwrap();
      let result = stage_executable_group(&snapshot, &group, dir.path().to_str().unwrap()).unwrap();

      assert_eq!(result.result, StageResult::Staged);
      let staged = staged_diff(&dir);
      assert!(staged.contains("new file mode 160000"));
      assert!(staged.contains(&format!("+Subproject commit {oid}")));
   }

   #[test]
   fn test_stage_executable_group_skips_file_whose_patch_no_longer_applies() {
      let dir = init_repo();
      write_file(&dir, "src/a.rs", &fixture_file_original());
      write_file(&dir, "src/b.rs", "fn b() {}\n");
      commit_all(&dir, "initial");

      write_file(&dir, "src/a.rs", &fixture_file_two_hunks());
      write_file(&dir, "src/b.rs", "fn b_changed() {}\n");

      let diff = get_compose_diff(dir.path().to_str().unwrap()).unwrap();
      let stat = get_compose_stat(dir.path().to_str().unwrap()).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();
      let a_file = snapshot.file_by_path("src/a.rs").unwrap();
      let b_file = snapshot.file_by_path("src/b.rs").unwrap();
      let group = ComposeExecutableGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     vec![a_file.file_id.clone(), b_file.file_id.clone()],
         rationale:    "both files".to_string(),
         dependencies: vec![],
         hunk_ids:     a_file
            .hunk_ids
            .iter()
            .chain(b_file.hunk_ids.iter())
            .cloned()
            .collect(),
      };

      // Diverge src/a.rs at the same lines the plan touches and commit it, so the
      // planned hunks for that file no longer apply (3-way merge conflicts).
      write_file(&dir, "src/a.rs", &fixture_file_original().replace("alpha", "alpha diverged"));
      run_git(&dir, &["add", "src/a.rs"]);
      run_git(&dir, &["commit", "-m", "diverge a"]);

      reset_staging(dir.path().to_str().unwrap()).unwrap();
      write_file(&dir, "src/b.rs", "fn b_changed() {}\n");

      let outcome =
         stage_executable_group(&snapshot, &group, dir.path().to_str().unwrap()).unwrap();

      // src/b.rs still applies, so the group is committable; src/a.rs is skipped.
      assert_eq!(outcome.result, StageResult::Staged);
      assert_eq!(outcome.skipped.len(), 1);
      assert_eq!(outcome.skipped[0].path, "src/a.rs");

      let staged = staged_diff(&dir);
      assert!(staged.contains("b_changed"));
      assert!(!staged.contains("alpha changed"));
      // The skipped file's index entry is restored to HEAD: no conflict residue.
      assert!(!staged.contains("src/a.rs"));
   }

   #[test]
   fn test_stage_executable_group_in_index_preserves_real_staged_diff() {
      let dir = init_repo();
      write_file(&dir, "src/lib.rs", &fixture_file_original());
      write_file(&dir, "sentinel.txt", "base\n");
      commit_all(&dir, "initial");
      write_file(&dir, "src/lib.rs", &fixture_file_stage_only());

      let diff = get_compose_diff(dir.path().to_str().unwrap()).unwrap();
      let stat = get_compose_stat(dir.path().to_str().unwrap()).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();
      let source_file = snapshot.file_by_path("src/lib.rs").unwrap();
      let group = ComposeExecutableGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     vec![source_file.file_id.clone()],
         rationale:    "source change".to_string(),
         dependencies: vec![],
         hunk_ids:     source_file.hunk_ids.clone(),
      };

      write_file(&dir, "sentinel.txt", "base\nstaged sentinel\n");
      run_git(&dir, &["add", "sentinel.txt"]);
      let real_staged_before = staged_diff(&dir);
      assert!(real_staged_before.contains("staged sentinel"));

      let index = TempGitIndex::new(dir.path().to_str().unwrap()).unwrap();
      read_tree_into_index(index.path(), "HEAD", dir.path().to_str().unwrap()).unwrap();
      let outcome = stage_executable_group_in_index(
         &snapshot,
         &group,
         dir.path().to_str().unwrap(),
         index.path(),
      )
      .unwrap();

      assert_eq!(outcome.result, StageResult::Staged);
      assert_eq!(staged_diff(&dir), real_staged_before);
      let temp_staged = staged_diff_in_index(&dir, &index);
      assert!(temp_staged.contains("alpha staged"));
      assert!(!temp_staged.contains("staged sentinel"));
   }

   #[test]
   fn test_force_stage_file_from_base_in_index_preserves_real_staged_diff() {
      let dir = init_repo();
      run_git(&dir, &["config", "core.autocrlf", "false"]);
      let original = [
         "fn alpha() {",
         "    println!(\"alpha\");",
         "}",
         "",
         "fn beta() {",
         "    println!(\"beta\");",
         "}",
         "",
      ]
      .join("\r\n");
      let modified = original.replace("println!(\"beta\")", "println!(\"beta changed\")");
      write_file(&dir, "src/crlf.rs", &original);
      write_file(&dir, "sentinel.txt", "base\n");
      commit_all(&dir, "initial");
      write_file(&dir, "src/crlf.rs", &modified);

      let diff = get_compose_diff(dir.path().to_str().unwrap()).unwrap();
      let stat = get_compose_stat(dir.path().to_str().unwrap()).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();
      let source_file = snapshot.file_by_path("src/crlf.rs").unwrap();

      write_file(&dir, "sentinel.txt", "base\nstaged sentinel\n");
      run_git(&dir, &["add", "sentinel.txt"]);
      let real_staged_before = staged_diff(&dir);

      let index = TempGitIndex::new(dir.path().to_str().unwrap()).unwrap();
      read_tree_into_index(index.path(), "HEAD", dir.path().to_str().unwrap()).unwrap();
      force_stage_file_from_base_in_index(
         &snapshot,
         &source_file.file_id,
         &source_file.hunk_ids.clone(),
         dir.path().to_str().unwrap(),
         index.path(),
      )
      .unwrap();

      assert_eq!(staged_diff(&dir), real_staged_before);
      let staged_blob = crate::git::git_command_with_index(index.path())
         .args(["show", ":src/crlf.rs"])
         .current_dir(dir.path())
         .output()
         .unwrap();
      assert!(staged_blob.status.success());
      assert_eq!(String::from_utf8_lossy(&staged_blob.stdout).to_string(), modified);
   }

   #[test]
   fn test_force_stage_file_from_base_preserves_crlf_patch_lines() {
      let dir = init_repo();
      run_git(&dir, &["config", "core.autocrlf", "false"]);
      let original = [
         "fn alpha() {",
         "    println!(\"alpha\");",
         "}",
         "",
         "fn beta() {",
         "    println!(\"beta\");",
         "}",
         "",
      ]
      .join("\r\n");
      let modified = original.replace("println!(\"beta\")", "println!(\"beta changed\")");
      write_file(&dir, "src/crlf.rs", &original);
      commit_all(&dir, "initial");
      write_file(&dir, "src/crlf.rs", &modified);

      let diff = get_compose_diff(dir.path().to_str().unwrap()).unwrap();
      assert!(diff.contains("-    println!(\"beta\");\r\n"));
      assert!(diff.contains("+    println!(\"beta changed\");\r\n"));
      let stat = get_compose_stat(dir.path().to_str().unwrap()).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();
      let source_file = snapshot.file_by_path("src/crlf.rs").unwrap();

      reset_staging(dir.path().to_str().unwrap()).unwrap();
      force_stage_file_from_base(
         &snapshot,
         &source_file.file_id,
         &source_file.hunk_ids.clone(),
         dir.path().to_str().unwrap(),
      )
      .unwrap();

      let staged_blob = run_git(&dir, &["show", ":src/crlf.rs"]);
      assert_eq!(staged_blob, modified);
   }
   #[test]
   fn test_force_stage_file_from_base_ignores_index_drift() {
      let dir = init_repo();
      write_file(&dir, "src/lib.rs", &fixture_file_original());
      commit_all(&dir, "initial");
      write_file(&dir, "src/lib.rs", &fixture_file_two_hunks());

      let diff = get_compose_diff(dir.path().to_str().unwrap()).unwrap();
      let stat = get_compose_stat(dir.path().to_str().unwrap()).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();
      let source_file = snapshot.file_by_path("src/lib.rs").unwrap();
      assert_eq!(source_file.hunk_ids.len(), 2);

      // Drift the index far from base: stage an unrelated full-file rewrite, so a
      // normal `git apply` of the planned hunks against this index would fail.
      write_file(&dir, "src/lib.rs", "fn totally_different() {}\n");
      run_git(&dir, &["add", "src/lib.rs"]);

      // Force-stage only the first planned hunk from base, ignoring the drift.
      force_stage_file_from_base(
         &snapshot,
         &source_file.file_id,
         &[source_file.hunk_ids[0].clone()],
         dir.path().to_str().unwrap(),
      )
      .unwrap();

      let staged = staged_diff(&dir);
      assert!(staged.contains("alpha changed"));
      assert!(!staged.contains("beta changed"));
      assert!(!staged.contains("totally_different"));

      // Applying both hunks reconstructs the full planned target from base.
      force_stage_file_from_base(
         &snapshot,
         &source_file.file_id,
         &source_file.hunk_ids.clone(),
         dir.path().to_str().unwrap(),
      )
      .unwrap();
      let staged = staged_diff(&dir);
      assert!(staged.contains("alpha changed"));
      assert!(staged.contains("beta changed"));
      assert!(!staged.contains("totally_different"));
   }

   #[test]
   fn test_force_stage_split_across_commits_leaves_worktree_clean() {
      let dir = init_repo();
      write_file(&dir, "src/lib.rs", &fixture_file_original());
      commit_all(&dir, "initial");
      // The working tree holds the full planned change and is never rewritten.
      write_file(&dir, "src/lib.rs", &fixture_file_two_hunks());

      let dirs = dir.path().to_str().unwrap();
      let diff = get_compose_diff(dirs).unwrap();
      let stat = get_compose_stat(dirs).unwrap();
      let snapshot = build_compose_snapshot(&diff, &stat).unwrap();
      let file = snapshot.file_by_path("src/lib.rs").unwrap();
      assert_eq!(file.hunk_ids.len(), 2);

      reset_staging(dirs).unwrap();

      // Commit 1 takes the first hunk (cumulative = [h0]).
      force_stage_file_from_base(&snapshot, &file.file_id, &[file.hunk_ids[0].clone()], dirs)
         .unwrap();
      run_git(&dir, &["commit", "-m", "first"]);

      // Commit 2 takes both hunks (cumulative = [h0, h1]).
      force_stage_file_from_base(&snapshot, &file.file_id, &file.hunk_ids.clone(), dirs).unwrap();
      run_git(&dir, &["commit", "-m", "second"]);

      // The two commits together reproduce the working tree exactly: nothing is
      // left uncommitted on disk and no file was modified by staging.
      let status = run_git(&dir, &["status", "--porcelain"]);
      assert!(status.trim().is_empty(), "working tree should be clean, got: {status:?}");
   }
}
