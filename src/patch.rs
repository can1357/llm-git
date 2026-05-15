use std::collections::{BTreeMap, HashSet};

use crate::{
   compose_types::{ComposeExecutableGroup, ComposeFile, ComposeHunk, ComposeSnapshot},
   error::{CommitGenError, Result},
   git::git_command,
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
   apply_patch:    String,
   fallback_files: Vec<String>,
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

/// Run `git apply` with a patch supplied on stdin.
fn run_git_apply(patch: &str, args: &[&str], dir: &str) -> Result<std::process::Output> {
   let mut child = git_command()
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

fn patch_is_already_applied_to_index(patch: &str, dir: &str) -> Result<bool> {
   let output =
      run_git_apply(patch, &["apply", "--cached", "--reverse", "--check", "--recount"], dir)?;
   Ok(output.status.success())
}

/// Apply patch to staging area.
pub fn apply_patch_to_index(patch: &str, dir: &str) -> Result<StageResult> {
   if patch.trim().is_empty() {
      return Ok(StageResult::EmptyPatch);
   }

   if patch_is_already_applied_to_index(patch, dir)? {
      return Ok(StageResult::AlreadyApplied);
   }

   let output = run_git_apply(patch, &["apply", "--cached", "--3way", "--recount"], dir)?;
   if output.status.success() {
      return Ok(StageResult::Staged);
   }

   let stderr = String::from_utf8_lossy(&output.stderr);
   Err(CommitGenError::git(format!("git apply --cached --3way --recount failed: {stderr}")))
}

/// Stage specific files.
pub fn stage_files(files: &[String], dir: &str) -> Result<()> {
   if files.is_empty() {
      return Ok(());
   }

   let output = git_command()
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
      .filter(|line| {
         (line.starts_with('+') && !line.starts_with("+++"))
            || (line.starts_with('-') && !line.starts_with("---"))
      })
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

   for line in diff.lines() {
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
         if line.starts_with('+') && !line.starts_with("+++") {
            file.additions += 1;
         } else if line.starts_with('-') && !line.starts_with("---") {
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
      if line.starts_with('+') && !line.starts_with("+++") {
         additions += 1;
      } else if line.starts_with('-') && !line.starts_with("---") {
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

pub fn create_executable_group_patch(
   snapshot: &ComposeSnapshot,
   group: &ComposeExecutableGroup,
) -> Result<ComposeGroupPatch> {
   let selected_by_file = selected_hunks_by_file(snapshot, group)?;
   let mut fallback_files = Vec::new();
   let mut diff = String::new();
   let mut stat = String::new();
   let mut apply_patch = String::new();

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
            fallback_files.push(file.path.clone());
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
      apply_patch.push_str(&file_patch);
      push_stat_line(&mut stat, &file.path, additions, deletions, false);
   }

   fallback_files.sort();
   fallback_files.dedup();

   Ok(ComposeGroupPatch { diff, stat, apply_patch, fallback_files })
}

pub fn stage_executable_group(
   snapshot: &ComposeSnapshot,
   group: &ComposeExecutableGroup,
   dir: &str,
) -> Result<StageResult> {
   let group_patch = create_executable_group_patch(snapshot, group)?;
   let mut result = StageResult::EmptyPatch;

   if !group_patch.fallback_files.is_empty() {
      stage_files(&group_patch.fallback_files, dir)?;
      result = result.combine(StageResult::Staged);
   }

   let patch_result = apply_patch_to_index(&group_patch.apply_patch, dir)?;
   result = result.combine(patch_result);

   Ok(result)
}

#[cfg(test)]
mod tests {
   use std::fs;

   use tempfile::TempDir;

   use super::*;
   use crate::{
      compose_types::ComposeExecutableGroup,
      git::{get_compose_diff, get_compose_stat},
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
         stage_executable_group(&snapshot, &first_group, dir.path().to_str().unwrap()).unwrap(),
         StageResult::Staged
      );
      run_git(&dir, &["commit", "-m", "first"]);
      write_file(&dir, "Dockerfile", "FROM scratch\n");

      assert_eq!(
         stage_executable_group(&snapshot, &second_group, dir.path().to_str().unwrap()).unwrap(),
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
      assert_eq!(first_result, StageResult::Staged);
      run_git(&dir, &["commit", "-m", "applied"]);

      let second_result =
         stage_executable_group(&snapshot, &group, dir.path().to_str().unwrap()).unwrap();
      assert_eq!(second_result, StageResult::AlreadyApplied);
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
      assert_eq!(planned_result, StageResult::Staged);
      let planned_staged = staged_diff(&dir);
      assert!(planned_staged.contains("+planned"));
      assert!(!planned_staged.contains("local edit"));

      reset_staging(dir.path().to_str().unwrap()).unwrap();
      write_file(&dir, "notes.txt", "planned\nlocal edit\n");
      let reused_result =
         stage_executable_group(&snapshot, &group, dir.path().to_str().unwrap()).unwrap();
      assert_eq!(reused_result, StageResult::Staged);
      let reused_staged = staged_diff(&dir);

      assert_eq!(reused_staged, planned_staged);
      assert!(!reused_staged.contains("local edit"));
   }
}
