use std::{
   collections::{BTreeMap, BTreeSet, HashMap, HashSet},
   fmt::Write,
   fs,
   path::{Path, PathBuf},
};

use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use crate::{
   api::{
      AnalysisContext, OneShotDebug, OneShotSpec, generate_conventional_analysis,
      generate_summary_from_analysis, run_oneshot, strict_json_schema,
   },
   compose_types::{
      ComposeBindingAssignment, ComposeExecutableGroup, ComposeExecutablePlan, ComposeFile,
      ComposeHunk, ComposeIntentGroup, ComposeIntentPlan, ComposeSnapshot,
   },
   config::CommitConfig,
   error::{CommitGenError, Result},
   git::{
      get_compose_diff, get_compose_stat, get_git_diff, get_git_dir, get_git_stat, get_head_hash,
      git_commit,
   },
   map_reduce::{FileObservation, observe_diff_files, should_use_map_reduce},
   normalization::{format_commit_message, post_process_commit_message},
   patch::{build_compose_snapshot, reset_staging, stage_executable_group},
   style, templates,
   tokens::{TokenCounter, create_token_counter},
   types::{Args, CommitType, ConventionalCommit, Mode, Scope},
   validation::validate_commit_message,
};

const MAX_OBSERVATIONS_PER_FILE: usize = 3;
const COMPOSE_PLAN_SCHEMA_VERSION: &str = "v3";
const COMPOSE_PLANNER_TEMPERATURE: f32 = 0.0;
const COMPOSE_SUMMARY_MEDIUM_FILE_THRESHOLD: usize = 60;
const COMPOSE_SUMMARY_MEDIUM_HUNK_THRESHOLD: usize = 200;
const COMPOSE_SUMMARY_LARGE_FILE_THRESHOLD: usize = 150;
const COMPOSE_SUMMARY_LARGE_HUNK_THRESHOLD: usize = 500;
const COMPOSE_AREA_TARGET_MAX_FILES: usize = 60;
const COMPOSE_AREA_TARGET_MAX_HUNKS: usize = 140;
const COMPOSE_AREA_TARGET_MAX_DEPTH: usize = 6;
const COMPOSE_MONOLITH_FALLBACK_TARGET_THRESHOLD: usize = 8;
const COMPOSE_MONOLITH_FALLBACK_WORKSTREAM_THRESHOLD: usize = 3;
const MAX_BIND_FILES_PER_REQUEST: usize = 18;
const MAX_BIND_HUNKS_PER_REQUEST: usize = 120;
/// Maximum number of commit messages to generate concurrently during
/// `execute_compose`. Matches the per-file fan-out used in `map_reduce`.
const COMPOSE_MESSAGE_PARALLELISM: usize = 8;

#[derive(Debug, Deserialize)]
struct ComposeIntentResponse {
   groups: Vec<ComposeIntentGroup>,
}

#[derive(Debug, Deserialize)]
struct ComposeBindingResponse {
   assignments: Vec<ComposeBindingAssignment>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ComposeCachedPlan {
   schema_version: String,
   cache_key:      String,
   plan:           ComposeExecutablePlan,
}

#[derive(Debug, Clone)]
struct AmbiguousFileBinding {
   file_id:             String,
   path:                String,
   candidate_group_ids: Vec<String>,
   hunk_ids:            Vec<String>,
}

#[derive(Debug, Clone)]
struct AmbiguousHunkContext {
   candidate_group_ids: Vec<String>,
}

type HunkAssignments = HashMap<String, BTreeSet<String>>;

#[derive(Debug)]
struct BindingEvaluation {
   assigned:   HashMap<String, Vec<String>>,
   unresolved: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct SnapshotSummaryBudget {
   max_observations_per_file: usize,
   max_hunks_per_file:        Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanningMode {
   File,
   Area,
}

#[derive(Debug, Clone)]
struct PlanningTarget {
   target_id:  String,
   label:      String,
   file_ids:   Vec<String>,
   hunk_count: usize,
   additions:  usize,
   deletions:  usize,
}

#[derive(Debug, Clone)]
struct PlanningIndex {
   mode:    PlanningMode,
   targets: Vec<PlanningTarget>,
   aliases: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct PlanningBucket {
   label:    String,
   file_ids: Vec<String>,
}

impl PlanningIndex {
   fn expand_target_ids(&self, target_ids: &[String]) -> Vec<String> {
      let mut expanded = Vec::new();
      let mut seen_file_ids = HashSet::new();

      for target_id in target_ids {
         if let Some(target) = self
            .targets
            .iter()
            .find(|candidate| candidate.target_id == *target_id)
         {
            for file_id in &target.file_ids {
               if seen_file_ids.insert(file_id.clone()) {
                  expanded.push(file_id.clone());
               }
            }
         }
      }

      expanded
   }
}

impl SnapshotSummaryBudget {
   const fn is_compacted(self) -> bool {
      self.max_hunks_per_file.is_some()
   }
}

fn is_dependency_manifest(path: &str) -> bool {
   const DEP_MANIFESTS: &[&str] = &[
      "Cargo.toml",
      "Cargo.lock",
      "package.json",
      "package-lock.json",
      "pnpm-lock.yaml",
      "yarn.lock",
      "bun.lock",
      "bun.lockb",
      "go.mod",
      "go.sum",
      "requirements.txt",
      "Pipfile",
      "Pipfile.lock",
      "pyproject.toml",
      "Gemfile",
      "Gemfile.lock",
      "composer.json",
      "composer.lock",
      "build.gradle",
      "build.gradle.kts",
      "gradle.properties",
      "pom.xml",
   ];

   let path = Path::new(path);
   let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
      return false;
   };

   if DEP_MANIFESTS.contains(&file_name) {
      return true;
   }

   Path::new(file_name)
      .extension()
      .is_some_and(|ext| ext.eq_ignore_ascii_case("lock") || ext.eq_ignore_ascii_case("lockb"))
}

fn save_debug_artifact<T: Serialize>(
   debug_dir: Option<&Path>,
   filename: &str,
   value: &T,
) -> Result<()> {
   let Some(debug_dir) = debug_dir else {
      return Ok(());
   };

   fs::create_dir_all(debug_dir)?;
   let path = debug_dir.join(filename);
   let json = serde_json::to_string_pretty(value)?;
   fs::write(path, json)?;
   Ok(())
}

fn fnv1a_64(input: &str) -> String {
   let mut hash = 0xcbf29ce484222325_u64;
   for byte in input.as_bytes() {
      hash ^= u64::from(*byte);
      hash = hash.wrapping_mul(0x100000001b3);
   }
   format!("{hash:016x}")
}

fn compose_plan_cache_key(
   snapshot: &ComposeSnapshot,
   max_commits: usize,
   analysis_model: &str,
) -> String {
   fnv1a_64(&format!(
      "{COMPOSE_PLAN_SCHEMA_VERSION}\n{analysis_model}\n{max_commits}\n{}\n{}",
      snapshot.diff, snapshot.stat
   ))
}

fn compose_plan_cache_path(
   dir: &str,
   snapshot: &ComposeSnapshot,
   max_commits: usize,
   analysis_model: &str,
) -> Result<PathBuf> {
   let git_dir = get_git_dir(dir)?;
   Ok(git_dir.join("llm-git").join(format!(
      "compose-plan-{}.json",
      compose_plan_cache_key(snapshot, max_commits, analysis_model)
   )))
}

fn load_cached_plan(
   dir: &str,
   snapshot: &ComposeSnapshot,
   max_commits: usize,
   analysis_model: &str,
) -> Result<Option<ComposeExecutablePlan>> {
   let cache_path = compose_plan_cache_path(dir, snapshot, max_commits, analysis_model)?;
   if !cache_path.exists() {
      return Ok(None);
   }

   let content = fs::read_to_string(&cache_path)
      .map_err(|err| CommitGenError::Other(format!("Failed to read compose cache: {err}")))?;
   let cached: ComposeCachedPlan = serde_json::from_str(&content)
      .map_err(|err| CommitGenError::Other(format!("Failed to parse compose cache: {err}")))?;
   let expected_key = compose_plan_cache_key(snapshot, max_commits, analysis_model);

   if cached.schema_version != COMPOSE_PLAN_SCHEMA_VERSION || cached.cache_key != expected_key {
      return Ok(None);
   }

   validate_executable_plan(snapshot, &cached.plan)?;
   Ok(Some(cached.plan))
}

fn save_cached_plan(
   dir: &str,
   snapshot: &ComposeSnapshot,
   max_commits: usize,
   analysis_model: &str,
   plan: &ComposeExecutablePlan,
) -> Result<()> {
   let cache_path = compose_plan_cache_path(dir, snapshot, max_commits, analysis_model)?;
   if let Some(parent) = cache_path.parent() {
      fs::create_dir_all(parent)?;
   }

   let cached = ComposeCachedPlan {
      schema_version: COMPOSE_PLAN_SCHEMA_VERSION.to_string(),
      cache_key:      compose_plan_cache_key(snapshot, max_commits, analysis_model),
      plan:           plan.clone(),
   };
   fs::write(cache_path, serde_json::to_string_pretty(&cached)?)?;
   Ok(())
}

fn format_line_range(start: usize, count: usize) -> String {
   match count {
      0 => "0".to_string(),
      1 => start.to_string(),
      _ => format!("{start}-{}", start + count - 1),
   }
}

const fn snapshot_summary_budget(snapshot: &ComposeSnapshot) -> SnapshotSummaryBudget {
   if snapshot.files.len() > COMPOSE_SUMMARY_LARGE_FILE_THRESHOLD
      || snapshot.hunks.len() > COMPOSE_SUMMARY_LARGE_HUNK_THRESHOLD
   {
      SnapshotSummaryBudget { max_observations_per_file: 1, max_hunks_per_file: Some(2) }
   } else if snapshot.files.len() > COMPOSE_SUMMARY_MEDIUM_FILE_THRESHOLD
      || snapshot.hunks.len() > COMPOSE_SUMMARY_MEDIUM_HUNK_THRESHOLD
   {
      SnapshotSummaryBudget { max_observations_per_file: 2, max_hunks_per_file: Some(3) }
   } else {
      SnapshotSummaryBudget {
         max_observations_per_file: MAX_OBSERVATIONS_PER_FILE,
         max_hunks_per_file:        None,
      }
   }
}

fn sample_positions(count: usize, max_samples: usize) -> Vec<usize> {
   if count <= max_samples {
      return (0..count).collect();
   }

   if max_samples <= 1 {
      return vec![0];
   }

   let last = count - 1;
   let mut positions = Vec::with_capacity(max_samples);
   for slot in 0..max_samples {
      let position = slot * last / (max_samples - 1);
      if positions.last().copied() != Some(position) {
         positions.push(position);
      }
   }
   positions
}

fn sampled_hunk_ids_for_summary(file: &ComposeFile, budget: SnapshotSummaryBudget) -> Vec<&str> {
   match budget.max_hunks_per_file {
      None => file.hunk_ids.iter().map(String::as_str).collect(),
      Some(max_hunks_per_file) => sample_positions(file.hunk_ids.len(), max_hunks_per_file)
         .into_iter()
         .filter_map(|idx| file.hunk_ids.get(idx).map(String::as_str))
         .collect(),
   }
}

fn render_snapshot_summary(snapshot: &ComposeSnapshot, observations: &[FileObservation]) -> String {
   let budget = snapshot_summary_budget(snapshot);
   let observations_by_file: HashMap<&str, Vec<&str>> = observations
      .iter()
      .map(|observation| {
         (
            observation.file.as_str(),
            observation
               .observations
               .iter()
               .map(String::as_str)
               .take(budget.max_observations_per_file)
               .collect(),
         )
      })
      .collect();

   let mut out = String::new();
   if budget.is_compacted() {
      let max_hunks_per_file = budget.max_hunks_per_file.unwrap_or_default();
      writeln!(
         out,
         "# snapshot compacted: all file IDs are preserved; showing up to {max_hunks_per_file} \
          representative hunks and {} observation(s) per file",
         budget.max_observations_per_file
      )
      .unwrap();
   }

   for file in &snapshot.files {
      writeln!(out, "- {} {}", file.file_id, file.summary).unwrap();
      if let Some(file_observations) = observations_by_file.get(file.path.as_str()) {
         for observation in file_observations {
            writeln!(out, "  observation: {observation}").unwrap();
         }
      }

      let rendered_hunk_ids = sampled_hunk_ids_for_summary(file, budget);
      for hunk_id in &rendered_hunk_ids {
         if let Some(hunk) = snapshot.hunk_by_id(hunk_id) {
            if hunk.synthetic {
               writeln!(out, "  - {} :: {}", hunk.hunk_id, hunk.snippet).unwrap();
            } else {
               writeln!(
                  out,
                  "  - {} old:{} new:{} :: {}",
                  hunk.hunk_id,
                  format_line_range(hunk.old_start, hunk.old_count),
                  format_line_range(hunk.new_start, hunk.new_count),
                  hunk.snippet
               )
               .unwrap();
            }
         }
      }

      let omitted_hunks = file.hunk_ids.len().saturating_sub(rendered_hunk_ids.len());
      if omitted_hunks > 0 {
         writeln!(out, "  ... {omitted_hunks} more hunks omitted from {}", file.file_id).unwrap();
      }
   }

   out
}

const fn planning_mode_for_snapshot(snapshot: &ComposeSnapshot) -> PlanningMode {
   if snapshot.files.len() > COMPOSE_SUMMARY_LARGE_FILE_THRESHOLD
      || snapshot.hunks.len() > COMPOSE_SUMMARY_LARGE_HUNK_THRESHOLD
   {
      PlanningMode::Area
   } else {
      PlanningMode::File
   }
}

fn path_depth(path: &str) -> usize {
   path.split('/').count()
}

fn prefix_at_depth(path: &str, depth: usize) -> String {
   if depth == 0 {
      return String::new();
   }

   let segments: Vec<&str> = path.split('/').collect();
   let effective_depth = depth.min(segments.len());
   segments[..effective_depth].join("/")
}

fn common_path_prefix(paths: &[String]) -> String {
   let Some(first_path) = paths.first() else {
      return String::new();
   };

   let mut prefix: Vec<&str> = first_path.split('/').collect();
   for path in paths.iter().skip(1) {
      let segments: Vec<&str> = path.split('/').collect();
      let shared = prefix
         .iter()
         .zip(segments.iter())
         .take_while(|(left, right)| left == right)
         .count();
      prefix.truncate(shared);
      if prefix.is_empty() {
         break;
      }
   }

   prefix.join("/")
}

fn bucket_hunk_count(snapshot: &ComposeSnapshot, file_ids: &[String]) -> usize {
   file_ids
      .iter()
      .filter_map(|file_id| snapshot.file_by_id(file_id))
      .map(|file| file.hunk_ids.len())
      .sum()
}

fn group_file_ids_by_prefix(
   snapshot: &ComposeSnapshot,
   file_ids: &[String],
   depth: usize,
) -> BTreeMap<String, Vec<String>> {
   let mut groups = BTreeMap::new();

   for file_id in file_ids {
      if let Some(file) = snapshot.file_by_id(file_id) {
         groups
            .entry(prefix_at_depth(&file.path, depth))
            .or_insert_with(Vec::new)
            .push(file_id.clone());
      }
   }

   groups
}

fn planning_bucket_label(snapshot: &ComposeSnapshot, file_ids: &[String]) -> String {
   let paths: Vec<String> = file_ids
      .iter()
      .filter_map(|file_id| snapshot.file_by_id(file_id).map(|file| file.path.clone()))
      .collect();

   let common_prefix = common_path_prefix(&paths);
   if common_prefix.is_empty() {
      paths.first().cloned().unwrap_or_else(|| "misc".to_string())
   } else {
      common_prefix
   }
}

fn collect_planning_buckets(
   snapshot: &ComposeSnapshot,
   file_ids: &[String],
   depth: usize,
) -> Vec<PlanningBucket> {
   let file_count = file_ids.len();
   let hunk_count = bucket_hunk_count(snapshot, file_ids);
   let max_path_depth = file_ids
      .iter()
      .filter_map(|file_id| snapshot.file_by_id(file_id))
      .map(|file| path_depth(&file.path))
      .max()
      .unwrap_or(depth);

   let should_stop =
      file_count <= COMPOSE_AREA_TARGET_MAX_FILES && hunk_count <= COMPOSE_AREA_TARGET_MAX_HUNKS;
   if should_stop || depth >= COMPOSE_AREA_TARGET_MAX_DEPTH || depth >= max_path_depth {
      return vec![PlanningBucket {
         label:    planning_bucket_label(snapshot, file_ids),
         file_ids: file_ids.to_vec(),
      }];
   }

   let next_depth = depth + 1;
   let groups = group_file_ids_by_prefix(snapshot, file_ids, next_depth);
   if groups.len() <= 1 {
      return collect_planning_buckets(snapshot, file_ids, next_depth);
   }

   groups
      .into_values()
      .flat_map(|group_file_ids| collect_planning_buckets(snapshot, &group_file_ids, next_depth))
      .collect()
}

fn build_area_planning_targets(snapshot: &ComposeSnapshot) -> Vec<PlanningTarget> {
   let all_file_ids: Vec<String> = snapshot
      .files
      .iter()
      .map(|file| file.file_id.clone())
      .collect();
   let buckets = collect_planning_buckets(snapshot, &all_file_ids, 0);

   buckets
      .into_iter()
      .enumerate()
      .map(|(idx, bucket)| {
         let mut additions = 0_usize;
         let mut deletions = 0_usize;
         let mut hunk_count = 0_usize;

         for file_id in &bucket.file_ids {
            if let Some(file) = snapshot.file_by_id(file_id) {
               additions = additions.saturating_add(file.additions);
               deletions = deletions.saturating_add(file.deletions);
               hunk_count = hunk_count.saturating_add(file.hunk_ids.len());
            }
         }

         PlanningTarget {
            target_id: format!("A{:03}", idx + 1),
            label: bucket.label,
            file_ids: bucket.file_ids,
            hunk_count,
            additions,
            deletions,
         }
      })
      .collect()
}

fn build_file_planning_targets(snapshot: &ComposeSnapshot) -> Vec<PlanningTarget> {
   snapshot
      .files
      .iter()
      .map(|file| PlanningTarget {
         target_id:  file.file_id.clone(),
         label:      file.path.clone(),
         file_ids:   vec![file.file_id.clone()],
         hunk_count: file.hunk_ids.len(),
         additions:  file.additions,
         deletions:  file.deletions,
      })
      .collect()
}

fn build_planning_index(snapshot: &ComposeSnapshot) -> PlanningIndex {
   let mode = planning_mode_for_snapshot(snapshot);
   let targets = match mode {
      PlanningMode::File => build_file_planning_targets(snapshot),
      PlanningMode::Area => build_area_planning_targets(snapshot),
   };

   let aliases = targets
      .iter()
      .flat_map(|target| {
         let normalized_label = normalize_file_reference(&target.label);
         [
            (target.target_id.clone(), target.target_id.clone()),
            (target.target_id.to_ascii_uppercase(), target.target_id.clone()),
            (normalized_label, target.target_id.clone()),
         ]
      })
      .collect();

   PlanningIndex { mode, targets, aliases }
}

fn sample_file_ids_for_target(target: &PlanningTarget) -> Vec<&str> {
   sample_positions(target.file_ids.len(), 4)
      .into_iter()
      .filter_map(|idx| target.file_ids.get(idx).map(String::as_str))
      .collect()
}

fn sample_hunk_ids_for_target(target: &PlanningTarget, snapshot: &ComposeSnapshot) -> Vec<String> {
   let hunk_ids: Vec<&String> = target
      .file_ids
      .iter()
      .filter_map(|file_id| snapshot.file_by_id(file_id))
      .flat_map(|file| file.hunk_ids.iter())
      .collect();

   sample_positions(hunk_ids.len(), 4)
      .into_iter()
      .filter_map(|idx| hunk_ids.get(idx).map(|hunk_id| (*hunk_id).clone()))
      .collect()
}

fn render_planning_stat(index: &PlanningIndex) -> String {
   let mut out = String::new();

   match index.mode {
      PlanningMode::File => {
         writeln!(out, "# planning over individual file IDs").unwrap();
      },
      PlanningMode::Area => {
         writeln!(
            out,
            "# planning over {} area IDs spanning {} files",
            index.targets.len(),
            index
               .targets
               .iter()
               .flat_map(|target| target.file_ids.iter())
               .collect::<HashSet<_>>()
               .len()
         )
         .unwrap();
      },
   }

   for target in &index.targets {
      writeln!(
         out,
         "{} {} | {} files | {} hunks | +{}/-{}",
         target.target_id,
         target.label,
         target.file_ids.len(),
         target.hunk_count,
         target.additions,
         target.deletions
      )
      .unwrap();
   }

   out
}

fn render_planning_snapshot_summary(
   snapshot: &ComposeSnapshot,
   observations: &[FileObservation],
   index: &PlanningIndex,
) -> String {
   if index.mode == PlanningMode::File {
      return render_snapshot_summary(snapshot, observations);
   }

   let observations_by_file: HashMap<&str, Vec<&str>> = observations
      .iter()
      .map(|observation| {
         (
            observation.file.as_str(),
            observation
               .observations
               .iter()
               .map(String::as_str)
               .take(1)
               .collect(),
         )
      })
      .collect();

   let mut out = String::new();
   writeln!(
      out,
      "# snapshot compacted into path-based planning areas; use the area IDs below in `file_ids`"
   )
   .unwrap();

   for target in &index.targets {
      writeln!(
         out,
         "- {} {} ({} files, {} hunks, +{}/-{})",
         target.target_id,
         target.label,
         target.file_ids.len(),
         target.hunk_count,
         target.additions,
         target.deletions
      )
      .unwrap();

      let sample_file_ids = sample_file_ids_for_target(target);
      if !sample_file_ids.is_empty() {
         let sample_files: Vec<String> = sample_file_ids
            .iter()
            .filter_map(|file_id| snapshot.file_by_id(file_id).map(|file| file.path.clone()))
            .collect();
         writeln!(out, "  files: {}", sample_files.join(", ")).unwrap();
         let omitted = target.file_ids.len().saturating_sub(sample_files.len());
         if omitted > 0 {
            writeln!(out, "  ... {omitted} more files omitted from {}", target.target_id).unwrap();
         }
      }

      let mut rendered_observations = 0_usize;
      for file_id in &target.file_ids {
         let Some(file) = snapshot.file_by_id(file_id) else {
            continue;
         };
         let Some(file_observations) = observations_by_file.get(file.path.as_str()) else {
            continue;
         };

         for observation in file_observations {
            writeln!(out, "  observation: {observation}").unwrap();
            rendered_observations += 1;
            if rendered_observations >= 2 {
               break;
            }
         }

         if rendered_observations >= 2 {
            break;
         }
      }

      for hunk_id in sample_hunk_ids_for_target(target, snapshot) {
         if let Some(hunk) = snapshot.hunk_by_id(&hunk_id) {
            if hunk.synthetic {
               writeln!(out, "  - {} :: {}", hunk.hunk_id, hunk.snippet).unwrap();
            } else {
               writeln!(
                  out,
                  "  - {} old:{} new:{} :: {}",
                  hunk.hunk_id,
                  format_line_range(hunk.old_start, hunk.old_count),
                  format_line_range(hunk.new_start, hunk.new_count),
                  hunk.snippet
               )
               .unwrap();
            }
         }
      }
   }

   out
}

fn render_planning_targets(index: &PlanningIndex, snapshot: &ComposeSnapshot) -> String {
   match index.mode {
      PlanningMode::File => format!(
         "File IDs only. Each target maps to exactly one file. Coverage: {} files.",
         snapshot.files.len()
      ),
      PlanningMode::Area => format!(
         "Area IDs only. Each target may expand to multiple files by shared path prefix. \
          Coverage: {} areas spanning {} files.",
         index.targets.len(),
         snapshot.files.len()
      ),
   }
}

fn render_planning_notes(index: &PlanningIndex) -> String {
   match index.mode {
      PlanningMode::File => {
         "Use only the provided file IDs and keep the grouping conservative.".to_string()
      },
      PlanningMode::Area => "This snapshot is large, so files were compacted into path-based \
                             planning areas. Split along independent subsystems or workstreams \
                             when the areas point at unrelated changes."
         .to_string(),
   }
}

fn render_split_bias(index: &PlanningIndex) -> String {
   match index.mode {
      PlanningMode::File => "Prefer fewer groups when the split is uncertain.".to_string(),
      PlanningMode::Area => "Prefer splitting unrelated areas into separate groups. Only return \
                             one broad group if nearly every area clearly belongs to the same \
                             atomic change."
         .to_string(),
   }
}

fn build_intent_schema(config: &CommitConfig) -> serde_json::Value {
   let type_enum: Vec<&str> = config.types.keys().map(String::as_str).collect();

   strict_json_schema(
      serde_json::json!({
         "groups": {
            "type": "array",
            "items": {
               "type": "object",
               "properties": {
                  "group_id": {
                     "type": "string",
                     "description": "Stable identifier like G1, G2, G3"
                  },
                  "file_ids": {
                     "type": "array",
                     "description": "Planning target IDs that belong to this logical commit. Use the exact IDs supplied in the prompt, even when they represent path-based areas instead of individual files. Never place group IDs or placeholder strings here. Repeat IDs across groups when a target is shared.",
                     "items": { "type": "string" }
                  },
                  "type": {
                     "type": "string",
                     "enum": type_enum,
                     "description": "Conventional commit type for this group"
                  },
                  "scope": {
                     "type": "string",
                     "description": "Optional scope (module/component). Omit if broad."
                  },
                  "rationale": {
                     "type": "string",
                     "description": "Brief explanation of the logical change"
                  },
                  "dependencies": {
                     "type": "array",
                     "description": "Group IDs this group depends on",
                     "items": { "type": "string" }
                  }
               },
               "required": ["group_id", "file_ids", "type", "rationale", "dependencies"],
               "additionalProperties": false
            }
         }
      }),
      &["groups"],
   )
}

fn build_binding_schema() -> serde_json::Value {
   strict_json_schema(
      serde_json::json!({
         "assignments": {
            "type": "array",
            "items": {
               "type": "object",
               "properties": {
                  "group_id": { "type": "string" },
                  "hunk_ids": {
                     "type": "array",
                     "items": { "type": "string" }
                  }
               },
               "required": ["group_id", "hunk_ids"],
               "additionalProperties": false
            }
         }
      }),
      &["assignments"],
   )
}

fn compute_dependency_order<T, FId, FDeps>(
   groups: &[T],
   group_id: FId,
   dependencies: FDeps,
) -> Result<Vec<usize>>
where
   FId: Fn(&T) -> &str,
   FDeps: Fn(&T) -> &[String],
{
   let mut index_by_id = HashMap::new();
   for (idx, group) in groups.iter().enumerate() {
      let id = group_id(group);
      if id.trim().is_empty() {
         return Err(CommitGenError::Other("Compose group_id cannot be empty".to_string()));
      }
      if index_by_id.insert(id.to_string(), idx).is_some() {
         return Err(CommitGenError::Other(format!("Duplicate compose group_id '{id}'")));
      }
   }

   let mut in_degree = vec![0_usize; groups.len()];
   let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); groups.len()];

   for (idx, group) in groups.iter().enumerate() {
      for dependency in dependencies(group) {
         let dependency_idx = index_by_id.get(dependency).copied().ok_or_else(|| {
            CommitGenError::Other(format!(
               "Group {} depends on unknown group_id '{}'",
               group_id(group),
               dependency
            ))
         })?;
         if dependency_idx == idx {
            return Err(CommitGenError::Other(format!(
               "Group {} depends on itself",
               group_id(group)
            )));
         }

         adjacency[dependency_idx].push(idx);
         in_degree[idx] += 1;
      }
   }

   let mut queue: Vec<usize> = (0..groups.len())
      .filter(|idx| in_degree[*idx] == 0)
      .collect();
   let mut order = Vec::with_capacity(groups.len());

   while let Some(node) = queue.pop() {
      order.push(node);
      for neighbor in &adjacency[node] {
         in_degree[*neighbor] -= 1;
         if in_degree[*neighbor] == 0 {
            queue.push(*neighbor);
         }
      }
   }

   if order.len() != groups.len() {
      return Err(CommitGenError::Other(
         "Circular dependency detected in compose groups".to_string(),
      ));
   }

   Ok(order)
}

fn normalize_file_reference(raw_file_ref: &str) -> String {
   raw_file_ref
      .trim()
      .trim_matches(|ch| matches!(ch, '`' | '"' | '\''))
      .trim_start_matches("./")
      .trim_end_matches([',', ';'])
      .to_string()
}

fn planning_text_tokens(text: &str) -> Vec<String> {
   const STOP_WORDS: &[&str] = &[
      "and",
      "for",
      "the",
      "with",
      "from",
      "into",
      "after",
      "before",
      "over",
      "under",
      "plus",
      "across",
      "update",
      "updated",
      "refactor",
      "refactored",
      "changes",
      "change",
      "logical",
      "group",
      "groups",
      "commit",
      "commits",
   ];

   let mut tokens = Vec::new();
   let mut current = String::new();
   let mut seen = HashSet::new();

   for ch in text.chars() {
      if ch.is_ascii_alphanumeric() {
         current.push(ch.to_ascii_lowercase());
      } else if current.len() >= 3 {
         if !STOP_WORDS.contains(&current.as_str()) && seen.insert(current.clone()) {
            tokens.push(current.clone());
         }
         current.clear();
      } else {
         current.clear();
      }
   }

   if current.len() >= 3 && !STOP_WORDS.contains(&current.as_str()) && seen.insert(current.clone())
   {
      tokens.push(current);
   }

   tokens
}

fn extract_group_id_candidate(raw: &str) -> Option<String> {
   let normalized = normalize_file_reference(raw);
   let uppercase = normalized.to_ascii_uppercase();

   if uppercase.chars().all(|ch| ch.is_ascii_digit()) {
      return Some(format!("G{uppercase}"));
   }

   if let Some(rest) = uppercase.strip_prefix('G')
      && !rest.is_empty()
      && rest.chars().all(|ch| ch.is_ascii_digit())
   {
      return Some(format!("G{rest}"));
   }

   let digits: String = uppercase.chars().filter(|ch| ch.is_ascii_digit()).collect();
   let compact = uppercase
      .chars()
      .filter(|ch| !matches!(ch, ' ' | '_' | '-'))
      .collect::<String>();
   if compact.starts_with("GROUP") && !digits.is_empty() {
      return Some(format!("G{digits}"));
   }

   None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeFileCategory {
   Binary,
   Dependency,
   Docs,
   Test,
   Config,
   Source,
   Other,
}

fn compose_file_category(file: &ComposeFile) -> ComposeFileCategory {
   if file.is_binary {
      return ComposeFileCategory::Binary;
   }

   if is_dependency_manifest(&file.path) {
      return ComposeFileCategory::Dependency;
   }

   let path = file.path.to_ascii_lowercase();
   let file_name = Path::new(&path)
      .file_name()
      .and_then(|name| name.to_str())
      .unwrap_or_default();
   let extension = Path::new(&path)
      .extension()
      .and_then(|ext| ext.to_str())
      .unwrap_or_default();

   if extension == "md" || file_name == "readme" || file_name == "readme.md" {
      return ComposeFileCategory::Docs;
   }

   if path.contains("/tests/")
      || path.starts_with("tests/")
      || file_name.contains("test")
      || file_name.contains("spec")
   {
      return ComposeFileCategory::Test;
   }

   if matches!(extension, "toml" | "yaml" | "yml" | "json" | "ini" | "cfg" | "conf" | "env") {
      return ComposeFileCategory::Config;
   }

   if matches!(
      extension,
      "rs"
         | "py"
         | "js"
         | "jsx"
         | "ts"
         | "tsx"
         | "go"
         | "java"
         | "kt"
         | "c"
         | "cc"
         | "cpp"
         | "h"
         | "hpp"
         | "cs"
         | "rb"
         | "php"
         | "swift"
         | "scala"
         | "m"
         | "mm"
   ) {
      return ComposeFileCategory::Source;
   }

   ComposeFileCategory::Other
}

fn common_path_prefix_depth(left: &str, right: &str) -> usize {
   left
      .split('/')
      .zip(right.split('/'))
      .take_while(|(left_segment, right_segment)| left_segment == right_segment)
      .count()
}

fn file_similarity_score(missing_file: &ComposeFile, candidate_file: &ComposeFile) -> i32 {
   let mut score = (common_path_prefix_depth(&missing_file.path, &candidate_file.path) as i32) * 25;

   if Path::new(&missing_file.path).parent() == Path::new(&candidate_file.path).parent() {
      score += 40;
   }

   if Path::new(&missing_file.path).extension() == Path::new(&candidate_file.path).extension() {
      score += 12;
   }

   if compose_file_category(missing_file) == compose_file_category(candidate_file) {
      score += 18;
   }

   score
}

fn group_type_bonus(file: &ComposeFile, group: &ComposeIntentGroup) -> i32 {
   match (compose_file_category(file), group.commit_type.as_str()) {
      (ComposeFileCategory::Docs, "docs") => 25,
      (ComposeFileCategory::Test, "test") => 25,
      (ComposeFileCategory::Dependency, "build" | "chore" | "ci") => 18,
      (ComposeFileCategory::Config, "build" | "chore" | "ci") => 12,
      (ComposeFileCategory::Source, "feat" | "fix" | "refactor" | "perf") => 10,
      _ => 0,
   }
}

fn best_group_for_missing_file(
   snapshot: &ComposeSnapshot,
   groups: &[ComposeIntentGroup],
   missing_file: &ComposeFile,
) -> usize {
   let mut best_group_idx = 0;
   let mut best_score = i32::MIN;
   let mut best_group_size = usize::MAX;

   for (group_idx, group) in groups.iter().enumerate() {
      let similarity = group
         .file_ids
         .iter()
         .filter_map(|file_id| snapshot.file_by_id(file_id))
         .map(|candidate_file| file_similarity_score(missing_file, candidate_file))
         .max()
         .unwrap_or_default();
      let score = similarity + group_type_bonus(missing_file, group);
      let group_size = group.file_ids.len();

      if score > best_score || (score == best_score && group_size < best_group_size) {
         best_group_idx = group_idx;
         best_score = score;
         best_group_size = group_size;
      }
   }

   best_group_idx
}

fn normalize_dependency_reference(
   raw_dependency: &str,
   known_group_ids: &HashSet<String>,
) -> Option<String> {
   let normalized = normalize_file_reference(raw_dependency);
   if normalized.is_empty() {
      return None;
   }

   if known_group_ids.contains(&normalized) {
      return Some(normalized);
   }

   let uppercase = normalized.to_ascii_uppercase();
   if known_group_ids.contains(&uppercase) {
      return Some(uppercase);
   }

   let candidate = extract_group_id_candidate(&normalized)?;
   known_group_ids.contains(&candidate).then_some(candidate)
}

fn planning_target_match_score(target: &PlanningTarget, group: &ComposeIntentGroup) -> i32 {
   let label = target.label.to_ascii_lowercase();
   let workstream = workstream_key_for_label(&target.label).to_ascii_lowercase();
   let mut score = (target.hunk_count.min(40) as i32) + (target.file_ids.len().min(20) as i32);

   if let Some(scope) = &group.scope {
      let scope = scope.as_str().to_ascii_lowercase();
      if label.contains(&scope) || workstream.contains(&scope) {
         score += 140;
      }

      for segment in scope.split('/') {
         if !segment.is_empty() && (label.contains(segment) || workstream.contains(segment)) {
            score += 45;
         }
      }
   }

   for token in planning_text_tokens(&group.rationale) {
      if label.contains(&token) || workstream.contains(&token) {
         score += 16;
      }
   }

   match group.commit_type.as_str() {
      "ci" if target.label.starts_with(".github/") => score += 120,
      "docs"
         if target.label.starts_with("docs/")
            || Path::new(&target.label)
               .extension()
               .is_some_and(|ext| ext.eq_ignore_ascii_case("md")) =>
         score += 80,
      "build" | "chore"
         if target.label.contains("Cargo")
            || target.label.contains("package")
            || target.label.contains("lock")
            || target.label.contains("tsconfig")
            || target.label.contains("biome")
            || target.label.contains("bun") =>
      {
         score += 55;
      },
      _ => {},
   }

   score
}

fn seed_group_targets(
   groups: &[ComposeIntentGroup],
   planning_index: &PlanningIndex,
   group_targets: &mut [Vec<String>],
   repair_notes: &mut Vec<String>,
) {
   let mut claimed_target_ids: HashSet<String> = group_targets.iter().flatten().cloned().collect();

   for (group_idx, group) in groups.iter().enumerate() {
      if !group_targets[group_idx].is_empty() {
         continue;
      }

      let fallback_target = planning_index
         .targets
         .iter()
         .max_by_key(|target| {
            let mut score = planning_target_match_score(target, group);
            if !claimed_target_ids.contains(&target.target_id) {
               score += 60;
            }
            (score, target.hunk_count, target.file_ids.len())
         })
         .or_else(|| planning_index.targets.first());

      let Some(fallback_target) = fallback_target else {
         continue;
      };

      group_targets[group_idx].push(fallback_target.target_id.clone());
      claimed_target_ids.insert(fallback_target.target_id.clone());
      repair_notes.push(format!(
         "Compose planner left {} without valid planning targets; seeded it with {} ({})",
         group.group_id, fallback_target.target_id, fallback_target.label
      ));
   }
}

fn normalize_intent_plan(
   snapshot: &ComposeSnapshot,
   planning_index: &PlanningIndex,
   mut groups: Vec<ComposeIntentGroup>,
) -> Result<(Vec<ComposeIntentGroup>, Vec<String>)> {
   if groups.is_empty() {
      return Err(CommitGenError::Other("Compose intent plan returned no groups".to_string()));
   }

   let known_target_ids: HashSet<&str> = planning_index
      .targets
      .iter()
      .map(|target| target.target_id.as_str())
      .collect();
   let mut repair_notes = Vec::new();
   let mut covered_file_ids = HashSet::new();
   let mut normalized_group_targets = Vec::with_capacity(groups.len());

   for group in &groups {
      if group.file_ids.is_empty() {
         repair_notes.push(format!(
            "Compose planner left {} without planning targets; assigning targets heuristically",
            group.group_id
         ));
      }

      let mut normalized_target_ids = Vec::new();
      let mut seen_target_ids = HashSet::new();
      for raw_target_ref in &group.file_ids {
         let normalized_ref = normalize_file_reference(raw_target_ref);
         let canonical_target_id = if known_target_ids.contains(normalized_ref.as_str()) {
            normalized_ref.clone()
         } else {
            let uppercase_ref = normalized_ref.to_ascii_uppercase();
            if known_target_ids.contains(uppercase_ref.as_str()) {
               uppercase_ref
            } else if let Some(target_id) = planning_index.aliases.get(&normalized_ref) {
               if raw_target_ref != target_id {
                  repair_notes.push(format!(
                     "Mapped compose planner target reference '{raw_target_ref}' to {target_id}"
                  ));
               }
               target_id.clone()
            } else {
               repair_notes.push(format!(
                  "Dropped unknown planning target '{}' from {}",
                  raw_target_ref, group.group_id
               ));
               continue;
            }
         };

         if seen_target_ids.insert(canonical_target_id.clone()) {
            normalized_target_ids.push(canonical_target_id);
         }
      }

      normalized_group_targets.push(normalized_target_ids);
   }

   seed_group_targets(&groups, planning_index, &mut normalized_group_targets, &mut repair_notes);

   let known_group_ids: HashSet<String> =
      groups.iter().map(|group| group.group_id.clone()).collect();
   for group in &mut groups {
      let mut normalized_dependencies = Vec::new();
      let mut seen_dependencies = HashSet::new();

      for raw_dependency in &group.dependencies {
         let Some(dependency) = normalize_dependency_reference(raw_dependency, &known_group_ids)
         else {
            repair_notes.push(format!(
               "Dropped unknown dependency '{}' from {}",
               raw_dependency, group.group_id
            ));
            continue;
         };

         if dependency == group.group_id {
            repair_notes.push(format!(
               "Dropped self-dependency '{}' from {}",
               raw_dependency, group.group_id
            ));
            continue;
         }

         if seen_dependencies.insert(dependency.clone()) {
            if raw_dependency != &dependency {
               repair_notes.push(format!(
                  "Mapped compose planner dependency '{raw_dependency}' to {dependency}"
               ));
            }
            normalized_dependencies.push(dependency);
         }
      }

      group.dependencies = normalized_dependencies;
   }

   for (group, target_ids) in groups.iter_mut().zip(normalized_group_targets) {
      let expanded_file_ids = planning_index.expand_target_ids(&target_ids);
      for file_id in &expanded_file_ids {
         covered_file_ids.insert(file_id.clone());
      }
      group.file_ids = expanded_file_ids;
   }

   for file in &snapshot.files {
      if covered_file_ids.contains(file.file_id.as_str()) {
         continue;
      }

      let target_group_idx = best_group_for_missing_file(snapshot, &groups, file);
      let target_group = &mut groups[target_group_idx];
      target_group.file_ids.push(file.file_id.clone());
      covered_file_ids.insert(file.file_id.clone());
      repair_notes.push(format!(
         "Compose planner omitted {} ({}); assigned it to {}",
         file.file_id, file.path, target_group.group_id
      ));
   }

   Ok((groups, repair_notes))
}

fn workstream_key_for_label(label: &str) -> String {
   let segments: Vec<&str> = label
      .split('/')
      .filter(|segment| !segment.is_empty())
      .collect();
   let Some(first) = segments.first() else {
      return label.to_string();
   };

   match *first {
      ".github" => match segments.get(1) {
         Some(second) => format!("{first}/{second}"),
         None => (*first).to_string(),
      },
      "apps" | "packages" | "crates" | "services" | "libs" | "pass" => match segments.get(1) {
         Some(second) => format!("{first}/{second}"),
         None => (*first).to_string(),
      },
      _ => (*first).to_string(),
   }
}

fn workstream_display_name(label: &str) -> String {
   let key = workstream_key_for_label(label);
   match key.as_str() {
      ".github/workflows" => "CI workflows".to_string(),
      ".github" => "GitHub automation".to_string(),
      _ => key
         .split('/')
         .next_back()
         .map(|segment| segment.replace(['_', '-'], " "))
         .unwrap_or(key),
   }
}

fn sanitize_scope_fragment(raw: &str) -> Option<String> {
   let mut out = String::new();
   let mut last_was_separator = false;

   for ch in raw.trim().chars() {
      if ch.is_ascii_alphanumeric() {
         out.push(ch.to_ascii_lowercase());
         last_was_separator = false;
      } else if matches!(ch, '-' | '_' | '/' | '.' | ' ') && !out.is_empty() && !last_was_separator
      {
         out.push('-');
         last_was_separator = true;
      }
   }

   let trimmed = out.trim_matches('-').to_string();
   (!trimmed.is_empty()).then_some(trimmed)
}

fn fallback_scope_for_label(label: &str) -> Option<Scope> {
   let key = workstream_key_for_label(label);
   let candidate = key
      .split('/')
      .next_back()
      .and_then(sanitize_scope_fragment)?;
   Scope::new(candidate).ok()
}

fn fallback_rationale_for_labels(labels: &[String]) -> String {
   if labels.len() == 1 {
      let label = labels[0].as_str();
      let display = workstream_display_name(label);
      if label.starts_with("apps/") {
         return format!("{display} application updates");
      }
      if label.starts_with("packages/") {
         return format!("{display} package updates");
      }
      if label.starts_with("crates/") {
         return format!("{display} crate updates");
      }
      if label.starts_with(".github/") || label == ".github" {
         return format!("{display} updates");
      }
      return format!("{display} updates");
   }

   let display_labels: Vec<String> = labels
      .iter()
      .take(3)
      .map(|label| workstream_display_name(label))
      .collect();
   format!("cross-cutting updates for {}", display_labels.join(", "))
}

fn fallback_commit_type_for_group(
   snapshot: &ComposeSnapshot,
   labels: &[String],
   file_ids: &[String],
) -> Result<CommitType> {
   if labels
      .iter()
      .any(|label| label == ".github" || label.starts_with(".github/"))
   {
      return CommitType::new("ci");
   }

   let files: Vec<&ComposeFile> = file_ids
      .iter()
      .filter_map(|file_id| snapshot.file_by_id(file_id))
      .collect();
   let all_docs = !files.is_empty()
      && files
         .iter()
         .all(|file| compose_file_category(file) == ComposeFileCategory::Docs);
   if all_docs {
      return CommitType::new("docs");
   }

   let all_tests = !files.is_empty()
      && files
         .iter()
         .all(|file| compose_file_category(file) == ComposeFileCategory::Test);
   if all_tests {
      return CommitType::new("test");
   }

   let all_dependencies =
      !files.is_empty() && files.iter().all(|file| is_dependency_manifest(&file.path));
   if all_dependencies {
      return CommitType::new("build");
   }

   let all_config = !files.is_empty()
      && files.iter().all(|file| {
         matches!(
            compose_file_category(file),
            ComposeFileCategory::Config | ComposeFileCategory::Dependency
         )
      });
   if all_config {
      return CommitType::new("chore");
   }

   CommitType::new("refactor")
}

fn ordered_file_ids(snapshot: &ComposeSnapshot, file_ids: &HashSet<String>) -> Vec<String> {
   snapshot
      .files
      .iter()
      .filter(|file| file_ids.contains(&file.file_id))
      .map(|file| file.file_id.clone())
      .collect()
}

fn is_monolithic_intent_plan(snapshot: &ComposeSnapshot, groups: &[ComposeIntentGroup]) -> bool {
   if groups.is_empty() {
      return false;
   }

   let largest_group = groups
      .iter()
      .map(|group| group.file_ids.iter().collect::<HashSet<_>>().len())
      .max()
      .unwrap_or_default();

   groups.len() == 1
      || (groups.len() <= 2
         && largest_group.saturating_mul(10) >= snapshot.files.len().saturating_mul(9))
}

fn should_force_large_patch_fallback(
   snapshot: &ComposeSnapshot,
   planning_index: &PlanningIndex,
   groups: &[ComposeIntentGroup],
   max_commits: usize,
) -> bool {
   if max_commits <= 1
      || planning_index.mode != PlanningMode::Area
      || planning_index.targets.len() < COMPOSE_MONOLITH_FALLBACK_TARGET_THRESHOLD
      || !is_monolithic_intent_plan(snapshot, groups)
   {
      return false;
   }

   let workstream_count = planning_index
      .targets
      .iter()
      .map(|target| workstream_key_for_label(&target.label))
      .collect::<HashSet<_>>()
      .len();

   workstream_count >= COMPOSE_MONOLITH_FALLBACK_WORKSTREAM_THRESHOLD
}

fn build_large_patch_fallback_groups(
   snapshot: &ComposeSnapshot,
   planning_index: &PlanningIndex,
   max_commits: usize,
) -> Result<Vec<ComposeIntentGroup>> {
   #[derive(Debug, Clone)]
   struct WorkstreamGroup {
      label:    String,
      file_ids: HashSet<String>,
      weight:   usize,
   }

   #[derive(Debug, Clone)]
   struct FallbackBin {
      labels:       Vec<String>,
      file_ids:     HashSet<String>,
      total_weight: usize,
   }

   let mut workstreams: HashMap<String, WorkstreamGroup> = HashMap::new();
   for target in &planning_index.targets {
      let key = workstream_key_for_label(&target.label);
      let entry = workstreams
         .entry(key.clone())
         .or_insert_with(|| WorkstreamGroup {
            label:    key,
            file_ids: HashSet::new(),
            weight:   0,
         });

      for file_id in &target.file_ids {
         entry.file_ids.insert(file_id.clone());
      }
      entry.weight = entry
         .weight
         .saturating_add(target.hunk_count.max(target.file_ids.len()));
   }

   let mut workstreams: Vec<WorkstreamGroup> = workstreams.into_values().collect();
   workstreams.sort_by(|left, right| {
      right
         .weight
         .cmp(&left.weight)
         .then_with(|| left.label.cmp(&right.label))
   });

   let bin_count = max_commits.min(workstreams.len());
   let mut bins: Vec<FallbackBin> = Vec::new();
   for workstream in workstreams {
      if bins.len() < bin_count {
         bins.push(FallbackBin {
            labels:       vec![workstream.label],
            file_ids:     workstream.file_ids,
            total_weight: workstream.weight,
         });
         continue;
      }

      let Some((target_idx, _)) = bins
         .iter()
         .enumerate()
         .min_by_key(|(_, bin)| (bin.total_weight, bin.labels.len()))
      else {
         continue;
      };

      let target_bin = &mut bins[target_idx];
      target_bin.labels.push(workstream.label);
      target_bin.total_weight = target_bin.total_weight.saturating_add(workstream.weight);
      target_bin.file_ids.extend(workstream.file_ids);
   }

   let mut groups = Vec::new();
   for (idx, bin) in bins.into_iter().enumerate() {
      let ordered_ids = ordered_file_ids(snapshot, &bin.file_ids);
      let commit_type = fallback_commit_type_for_group(snapshot, &bin.labels, &ordered_ids)?;
      let scope = (bin.labels.len() == 1)
         .then(|| fallback_scope_for_label(&bin.labels[0]))
         .flatten();
      let rationale = fallback_rationale_for_labels(&bin.labels);

      groups.push(ComposeIntentGroup {
         group_id: format!("G{}", idx + 1),
         commit_type,
         scope,
         file_ids: ordered_ids,
         rationale,
         dependencies: Vec::new(),
      });
   }

   Ok(groups)
}

async fn analyze_compose_intent(
   snapshot: &ComposeSnapshot,
   observations: &[FileObservation],
   config: &CommitConfig,
   max_commits: usize,
   debug_dir: Option<&Path>,
) -> Result<ComposeIntentPlan> {
   let planning_index = build_planning_index(snapshot);
   let stat_summary = render_planning_stat(&planning_index);
   let snapshot_summary = render_planning_snapshot_summary(snapshot, observations, &planning_index);
   let planning_targets = render_planning_targets(&planning_index, snapshot);
   let planning_notes = render_planning_notes(&planning_index);
   let split_bias = render_split_bias(&planning_index);
   let schema = build_intent_schema(config);
   let parts = templates::render_compose_intent_prompt(&templates::ComposeIntentPromptParams {
      variant: "default",
      max_commits,
      stat: &stat_summary,
      snapshot_summary: &snapshot_summary,
      planning_targets: &planning_targets,
      planning_notes: &planning_notes,
      split_bias: &split_bias,
   })?;

   let response = run_oneshot::<ComposeIntentResponse>(config, &OneShotSpec {
      operation:        "compose/intent",
      model:            &config.analysis_model,
      max_tokens:       3000,
      temperature:      COMPOSE_PLANNER_TEMPERATURE,
      prompt_family:    "compose-intent",
      prompt_variant:   "default",
      system_prompt:    &parts.system,
      user_prompt:      &parts.user,
      tool_name:        "create_compose_intent_plan",
      tool_description: "Plan logical commit groups over the provided planning target IDs",
      schema:           &schema,
      debug:            debug_dir.map(|dir| OneShotDebug {
         dir:    Some(dir),
         prefix: None,
         name:   "compose_intent",
      }),
   })
   .await?;

   let (mut groups, repair_notes) =
      normalize_intent_plan(snapshot, &planning_index, response.output.groups)?;
   for note in &repair_notes {
      eprintln!("{}", style::warning(note));
   }
   if should_force_large_patch_fallback(snapshot, &planning_index, &groups, max_commits) {
      eprintln!(
         "{}",
         style::warning(
            "Compose intent collapsed into a monolithic large-patch group; falling back to \
             path-based workstream splits."
         )
      );
      groups = build_large_patch_fallback_groups(snapshot, &planning_index, max_commits)?;
   }
   let dependency_order =
      compute_dependency_order(&groups, |group| &group.group_id, |group| &group.dependencies)?;

   Ok(ComposeIntentPlan { groups, dependency_order })
}

fn should_collect_compose_observations(
   snapshot: &ComposeSnapshot,
   config: &CommitConfig,
   counter: &TokenCounter,
) -> bool {
   planning_mode_for_snapshot(snapshot) != PlanningMode::Area
      && should_use_map_reduce(&snapshot.diff, config, counter)
}

fn auto_assign_hunks(
   snapshot: &ComposeSnapshot,
   intent_plan: &ComposeIntentPlan,
) -> Result<(HunkAssignments, Vec<AmbiguousFileBinding>)> {
   let mut groups_by_file: HashMap<&str, Vec<&str>> = HashMap::new();
   for group in &intent_plan.groups {
      for file_id in &group.file_ids {
         groups_by_file
            .entry(file_id.as_str())
            .or_default()
            .push(group.group_id.as_str());
      }
   }

   let mut assigned: HashMap<String, BTreeSet<String>> = intent_plan
      .groups
      .iter()
      .map(|group| (group.group_id.clone(), BTreeSet::new()))
      .collect();
   let mut ambiguous = Vec::new();

   for file in &snapshot.files {
      let Some(candidate_group_ids) = groups_by_file.get(file.file_id.as_str()) else {
         return Err(CommitGenError::Other(format!(
            "No compose group claimed file {} ({})",
            file.file_id, file.path
         )));
      };

      if candidate_group_ids.len() == 1 {
         let group_id = candidate_group_ids[0];
         let entry = assigned
            .get_mut(group_id)
            .ok_or_else(|| CommitGenError::Other(format!("Unknown compose group {group_id}")))?;
         for hunk_id in &file.hunk_ids {
            entry.insert(hunk_id.clone());
         }
      } else {
         ambiguous.push(AmbiguousFileBinding {
            file_id:             file.file_id.clone(),
            path:                file.path.clone(),
            candidate_group_ids: candidate_group_ids
               .iter()
               .map(|group_id| (*group_id).to_string())
               .collect(),
            hunk_ids:            file.hunk_ids.clone(),
         });
      }
   }

   Ok((assigned, ambiguous))
}

fn render_binding_groups(groups: &[ComposeIntentGroup]) -> String {
   let mut out = String::new();
   for group in groups {
      let scope = group
         .scope
         .as_ref()
         .map(|scope| format!("({})", scope.as_str()))
         .unwrap_or_default();
      writeln!(
         out,
         "- {} [{}{}] {}",
         group.group_id,
         group.commit_type.as_str(),
         scope,
         group.rationale
      )
      .unwrap();
   }

   out
}

fn render_binding_ambiguous_files(
   snapshot: &ComposeSnapshot,
   ambiguous_files: &[AmbiguousFileBinding],
) -> String {
   let mut out = String::new();
   for ambiguous_file in ambiguous_files {
      writeln!(
         out,
         "- {} {} candidates: {}",
         ambiguous_file.file_id,
         ambiguous_file.path,
         ambiguous_file.candidate_group_ids.join(", ")
      )
      .unwrap();

      for hunk_id in &ambiguous_file.hunk_ids {
         if let Some(hunk) = snapshot.hunk_by_id(hunk_id) {
            if hunk.synthetic {
               writeln!(out, "  - {} :: {}", hunk.hunk_id, hunk.snippet).unwrap();
            } else {
               writeln!(
                  out,
                  "  - {} old:{} new:{} :: {}",
                  hunk.hunk_id,
                  format_line_range(hunk.old_start, hunk.old_count),
                  format_line_range(hunk.new_start, hunk.new_count),
                  hunk.snippet
               )
               .unwrap();
            }
         }
      }
   }

   out
}

async fn request_binding(
   snapshot: &ComposeSnapshot,
   groups: &[ComposeIntentGroup],
   ambiguous_files: &[AmbiguousFileBinding],
   config: &CommitConfig,
   debug_dir: Option<&Path>,
   debug_name: &str,
) -> Result<Vec<ComposeBindingAssignment>> {
   let schema = build_binding_schema();
   let groups_text = render_binding_groups(groups);
   let ambiguous_files_text = render_binding_ambiguous_files(snapshot, ambiguous_files);
   let parts = templates::render_compose_bind_prompt(&templates::ComposeBindPromptParams {
      variant:         "default",
      groups:          &groups_text,
      ambiguous_files: &ambiguous_files_text,
   })?;
   let response = run_oneshot::<ComposeBindingResponse>(config, &OneShotSpec {
      operation:        "compose/bind",
      model:            &config.analysis_model,
      max_tokens:       2500,
      temperature:      COMPOSE_PLANNER_TEMPERATURE,
      prompt_family:    "compose-bind",
      prompt_variant:   "default",
      system_prompt:    &parts.system,
      user_prompt:      &parts.user,
      tool_name:        "bind_compose_hunks",
      tool_description: "Assign hunk IDs to existing compose groups",
      schema:           &schema,
      debug:            debug_dir.map(|dir| OneShotDebug {
         dir:    Some(dir),
         prefix: None,
         name:   debug_name,
      }),
   })
   .await?;

   Ok(response.output.assignments)
}

fn ambiguous_hunk_context(
   ambiguous_files: &[AmbiguousFileBinding],
) -> HashMap<String, AmbiguousHunkContext> {
   let mut context = HashMap::new();
   for ambiguous_file in ambiguous_files {
      for hunk_id in &ambiguous_file.hunk_ids {
         context.insert(hunk_id.clone(), AmbiguousHunkContext {
            candidate_group_ids: ambiguous_file.candidate_group_ids.clone(),
         });
      }
   }
   context
}

fn evaluate_binding(
   assignments: &[ComposeBindingAssignment],
   hunk_context: &HashMap<String, AmbiguousHunkContext>,
   valid_group_ids: &HashSet<&str>,
   snapshot: &ComposeSnapshot,
) -> BindingEvaluation {
   let mut assigned_hunk_to_group: HashMap<String, String> = HashMap::new();

   for assignment in assignments {
      if !valid_group_ids.contains(assignment.group_id.as_str()) {
         continue;
      }

      let mut seen_in_group = HashSet::new();
      for hunk_id in &assignment.hunk_ids {
         if !seen_in_group.insert(hunk_id.as_str()) {
            continue;
         }

         let Some(context) = hunk_context.get(hunk_id) else {
            continue;
         };

         if !context
            .candidate_group_ids
            .iter()
            .any(|candidate| candidate == &assignment.group_id)
         {
            continue;
         }

         match assigned_hunk_to_group.get(hunk_id) {
            None => {
               assigned_hunk_to_group.insert(hunk_id.clone(), assignment.group_id.clone());
            },
            Some(existing_group) if existing_group == &assignment.group_id => {},
            Some(_) => {
               assigned_hunk_to_group.remove(hunk_id);
            },
         }
      }
   }

   let mut assigned_by_group: HashMap<String, Vec<String>> = HashMap::new();
   for (hunk_id, group_id) in assigned_hunk_to_group {
      assigned_by_group.entry(group_id).or_default().push(hunk_id);
   }

   for hunk_ids in assigned_by_group.values_mut() {
      let ordered: Vec<String> = snapshot
         .hunks
         .iter()
         .filter(|hunk| hunk_ids.iter().any(|selected| selected == &hunk.hunk_id))
         .map(|hunk| hunk.hunk_id.clone())
         .collect();
      *hunk_ids = ordered;
   }

   let unresolved = snapshot
      .hunks
      .iter()
      .filter(|hunk| hunk_context.contains_key(&hunk.hunk_id))
      .filter(|hunk| {
         !assigned_by_group.values().any(|assigned_hunks| {
            assigned_hunks
               .iter()
               .any(|assigned| assigned == &hunk.hunk_id)
         })
      })
      .map(|hunk| hunk.hunk_id.clone())
      .collect();

   BindingEvaluation { assigned: assigned_by_group, unresolved }
}

fn filter_ambiguous_files(
   ambiguous_files: &[AmbiguousFileBinding],
   hunk_ids: &[String],
) -> Vec<AmbiguousFileBinding> {
   let hunk_ids: HashSet<&str> = hunk_ids.iter().map(String::as_str).collect();

   ambiguous_files
      .iter()
      .filter_map(|file| {
         let matching_hunks: Vec<String> = file
            .hunk_ids
            .iter()
            .filter(|hunk_id| hunk_ids.contains(hunk_id.as_str()))
            .cloned()
            .collect();

         (!matching_hunks.is_empty()).then(|| AmbiguousFileBinding {
            file_id:             file.file_id.clone(),
            path:                file.path.clone(),
            candidate_group_ids: file.candidate_group_ids.clone(),
            hunk_ids:            matching_hunks,
         })
      })
      .collect()
}

fn chunk_ambiguous_files(
   ambiguous_files: &[AmbiguousFileBinding],
) -> Vec<Vec<AmbiguousFileBinding>> {
   if ambiguous_files.is_empty() {
      return Vec::new();
   }

   let mut batches = Vec::new();
   let mut current_batch = Vec::new();
   let mut current_hunk_count = 0_usize;

   for file in ambiguous_files {
      let file_hunk_count = file.hunk_ids.len();
      let should_split = !current_batch.is_empty()
         && (current_batch.len() >= MAX_BIND_FILES_PER_REQUEST
            || current_hunk_count.saturating_add(file_hunk_count) > MAX_BIND_HUNKS_PER_REQUEST);

      if should_split {
         batches.push(current_batch);
         current_batch = Vec::new();
         current_hunk_count = 0;
      }

      current_hunk_count = current_hunk_count.saturating_add(file_hunk_count);
      current_batch.push(file.clone());
   }

   if !current_batch.is_empty() {
      batches.push(current_batch);
   }

   batches
}

fn order_hunk_ids(snapshot: &ComposeSnapshot, hunk_ids: &[String]) -> Vec<String> {
   let hunk_ids: HashSet<&str> = hunk_ids.iter().map(String::as_str).collect();

   snapshot
      .hunks
      .iter()
      .filter(|hunk| hunk_ids.contains(hunk.hunk_id.as_str()))
      .map(|hunk| hunk.hunk_id.clone())
      .collect()
}

fn fallback_group_for_hunk(
   hunk_id: &str,
   ambiguous_files: &[AmbiguousFileBinding],
   group_rank: &HashMap<&str, usize>,
) -> Option<String> {
   ambiguous_files.iter().find_map(|file| {
      file
         .hunk_ids
         .iter()
         .any(|candidate| candidate == hunk_id)
         .then(|| {
            file
               .candidate_group_ids
               .iter()
               .min_by_key(|group_id| {
                  group_rank
                     .get(group_id.as_str())
                     .copied()
                     .unwrap_or(usize::MAX)
               })
               .cloned()
         })
   })?
}

fn assign_unresolved_hunks(
   unresolved_hunks: &[String],
   assigned_by_group: &mut HashMap<String, BTreeSet<String>>,
   ambiguous_files: &[AmbiguousFileBinding],
   group_rank: &HashMap<&str, usize>,
) {
   for hunk_id in unresolved_hunks {
      if let Some(group_id) = fallback_group_for_hunk(hunk_id, ambiguous_files, group_rank)
         && let Some(group_hunks) = assigned_by_group.get_mut(&group_id)
      {
         group_hunks.insert(hunk_id.clone());
      }
   }
}

fn normalize_group_type(
   snapshot: &ComposeSnapshot,
   file_ids: &[String],
   original_type: &CommitType,
) -> Result<CommitType> {
   let dependency_only = !file_ids.is_empty()
      && file_ids.iter().all(|file_id| {
         snapshot
            .file_by_id(file_id)
            .is_some_and(|file| is_dependency_manifest(&file.path))
      });

   if dependency_only && original_type.as_str() != "build" {
      CommitType::new("build")
   } else {
      Ok(original_type.clone())
   }
}

fn derive_file_ids_for_hunks(snapshot: &ComposeSnapshot, hunk_ids: &[String]) -> Vec<String> {
   snapshot
      .files
      .iter()
      .filter(|file| {
         hunk_ids
            .iter()
            .any(|hunk_id| file.hunk_ids.contains(hunk_id))
      })
      .map(|file| file.file_id.clone())
      .collect()
}

fn build_redirects(
   intent_plan: &ComposeIntentPlan,
   executable_groups: &[ComposeExecutableGroup],
   group_rank: &HashMap<&str, usize>,
) -> HashMap<String, String> {
   let surviving_groups: HashMap<&str, &ComposeExecutableGroup> = executable_groups
      .iter()
      .filter(|group| !group.hunk_ids.is_empty())
      .map(|group| (group.group_id.as_str(), group))
      .collect();

   let mut redirects = HashMap::new();
   for group in &intent_plan.groups {
      if surviving_groups.contains_key(group.group_id.as_str()) {
         continue;
      }

      let redirect = executable_groups
         .iter()
         .filter(|candidate| candidate.group_id != group.group_id)
         .filter(|candidate| {
            candidate.file_ids.iter().any(|file_id| {
               group
                  .file_ids
                  .iter()
                  .any(|candidate_id| candidate_id == file_id)
            })
         })
         .min_by_key(|candidate| {
            group_rank
               .get(candidate.group_id.as_str())
               .copied()
               .unwrap_or(usize::MAX)
         })
         .map(|candidate| candidate.group_id.clone());

      if let Some(redirect) = redirect {
         redirects.insert(group.group_id.clone(), redirect);
      }
   }

   redirects
}

fn resolve_redirect(group_id: &str, redirects: &HashMap<String, String>) -> String {
   let mut current = group_id.to_string();
   let mut seen = HashSet::new();

   while let Some(next) = redirects.get(&current) {
      if !seen.insert(current.clone()) {
         break;
      }
      current.clone_from(next);
   }

   current
}

fn prune_empty_groups(
   groups: Vec<ComposeExecutableGroup>,
   redirects: &HashMap<String, String>,
) -> Result<ComposeExecutablePlan> {
   let surviving_ids: HashSet<String> = groups
      .iter()
      .filter(|group| !group.hunk_ids.is_empty())
      .map(|group| group.group_id.clone())
      .collect();

   let mut surviving_groups = Vec::new();
   for mut group in groups {
      if group.hunk_ids.is_empty() {
         continue;
      }

      let mut rewritten_dependencies = Vec::new();
      for dependency in &group.dependencies {
         let rewritten = resolve_redirect(dependency, redirects);
         if rewritten != group.group_id
            && surviving_ids.contains(&rewritten)
            && !rewritten_dependencies
               .iter()
               .any(|existing| existing == &rewritten)
         {
            rewritten_dependencies.push(rewritten);
         }
      }

      group.dependencies = rewritten_dependencies;
      surviving_groups.push(group);
   }

   let dependency_order = compute_dependency_order(
      &surviving_groups,
      |group| &group.group_id,
      |group| &group.dependencies,
   )?;
   Ok(ComposeExecutablePlan { groups: surviving_groups, dependency_order })
}

fn finalize_executable_plan(
   snapshot: &ComposeSnapshot,
   intent_plan: &ComposeIntentPlan,
   assigned_by_group: HashMap<String, BTreeSet<String>>,
) -> Result<ComposeExecutablePlan> {
   let group_rank: HashMap<&str, usize> = intent_plan
      .dependency_order
      .iter()
      .enumerate()
      .map(|(position, idx)| (intent_plan.groups[*idx].group_id.as_str(), position))
      .collect();

   let mut executable_groups = Vec::new();
   for group in &intent_plan.groups {
      let hunk_ids: Vec<String> = snapshot
         .hunks
         .iter()
         .filter(|hunk| {
            assigned_by_group
               .get(&group.group_id)
               .is_some_and(|assigned| assigned.contains(&hunk.hunk_id))
         })
         .map(|hunk| hunk.hunk_id.clone())
         .collect();

      let file_ids = derive_file_ids_for_hunks(snapshot, &hunk_ids);
      let commit_type = normalize_group_type(snapshot, &file_ids, &group.commit_type)?;
      executable_groups.push(ComposeExecutableGroup {
         group_id: group.group_id.clone(),
         commit_type,
         scope: group.scope.clone(),
         file_ids,
         rationale: group.rationale.clone(),
         dependencies: group.dependencies.clone(),
         hunk_ids,
      });
   }

   let redirects = build_redirects(intent_plan, &executable_groups, &group_rank);
   prune_empty_groups(executable_groups, &redirects)
}

fn validate_executable_plan(
   snapshot: &ComposeSnapshot,
   plan: &ComposeExecutablePlan,
) -> Result<()> {
   if plan.groups.is_empty() {
      return Err(CommitGenError::Other("Compose executable plan returned no groups".to_string()));
   }

   let known_hunks: HashSet<&str> = snapshot
      .hunks
      .iter()
      .map(|hunk| hunk.hunk_id.as_str())
      .collect();
   let known_files: HashSet<&str> = snapshot
      .files
      .iter()
      .map(|file| file.file_id.as_str())
      .collect();
   let mut coverage = HashMap::<String, String>::new();

   for group in &plan.groups {
      if group.hunk_ids.is_empty() {
         return Err(CommitGenError::Other(format!(
            "Compose group {} ended up empty after binding",
            group.group_id
         )));
      }

      for file_id in &group.file_ids {
         if !known_files.contains(file_id.as_str()) {
            return Err(CommitGenError::Other(format!(
               "Compose group {} references unknown file_id {}",
               group.group_id, file_id
            )));
         }
      }

      for hunk_id in &group.hunk_ids {
         if !known_hunks.contains(hunk_id.as_str()) {
            return Err(CommitGenError::Other(format!(
               "Compose group {} references unknown hunk_id {}",
               group.group_id, hunk_id
            )));
         }

         if let Some(existing_group) = coverage.insert(hunk_id.clone(), group.group_id.clone()) {
            return Err(CommitGenError::Other(format!(
               "Hunk {} was assigned to both {} and {}",
               hunk_id, existing_group, group.group_id
            )));
         }
      }
   }

   let missing_hunks: Vec<String> = snapshot
      .hunks
      .iter()
      .filter(|hunk| !coverage.contains_key(&hunk.hunk_id))
      .map(|hunk| hunk.hunk_id.clone())
      .collect();
   if !missing_hunks.is_empty() {
      return Err(CommitGenError::Other(format!(
         "Compose plan left hunks unassigned: {}",
         missing_hunks.join(", ")
      )));
   }

   let dependency_order =
      compute_dependency_order(&plan.groups, |group| &group.group_id, |group| &group.dependencies)?;
   if dependency_order != plan.dependency_order {
      return Err(CommitGenError::Other(
         "Compose dependency order does not match recomputed order".to_string(),
      ));
   }

   Ok(())
}

async fn bind_compose_plan(
   snapshot: &ComposeSnapshot,
   intent_plan: &ComposeIntentPlan,
   config: &CommitConfig,
   debug_dir: Option<&Path>,
) -> Result<ComposeExecutablePlan> {
   let (mut assigned_by_group, ambiguous_files) = auto_assign_hunks(snapshot, intent_plan)?;

   if !ambiguous_files.is_empty() {
      let valid_group_ids: HashSet<&str> = intent_plan
         .groups
         .iter()
         .map(|group| group.group_id.as_str())
         .collect();
      let binding_batches = chunk_ambiguous_files(&ambiguous_files);
      let mut unresolved = Vec::new();

      for (batch_idx, batch) in binding_batches.iter().enumerate() {
         let hunk_context = ambiguous_hunk_context(batch);
         let debug_name = if binding_batches.len() == 1 {
            "compose_bind".to_string()
         } else {
            format!("compose_bind_{:02}", batch_idx + 1)
         };
         let assignments =
            request_binding(snapshot, &intent_plan.groups, batch, config, debug_dir, &debug_name)
               .await?;
         let evaluation = evaluate_binding(&assignments, &hunk_context, &valid_group_ids, snapshot);
         for (group_id, hunk_ids) in evaluation.assigned {
            let entry = assigned_by_group.entry(group_id).or_default();
            for hunk_id in hunk_ids {
               entry.insert(hunk_id);
            }
         }
         unresolved.extend(evaluation.unresolved);
      }

      let group_rank: HashMap<&str, usize> = intent_plan
         .dependency_order
         .iter()
         .enumerate()
         .map(|(position, idx)| (intent_plan.groups[*idx].group_id.as_str(), position))
         .collect();

      let mut unresolved = order_hunk_ids(snapshot, &unresolved);
      if !unresolved.is_empty() {
         let unresolved_files = filter_ambiguous_files(&ambiguous_files, &unresolved);
         let repair_batches = chunk_ambiguous_files(&unresolved_files);
         let mut repair_unresolved = Vec::new();

         for (batch_idx, batch) in repair_batches.iter().enumerate() {
            let debug_name = if repair_batches.len() == 1 {
               "compose_bind_repair".to_string()
            } else {
               format!("compose_bind_repair_{:02}", batch_idx + 1)
            };
            let repair_assignments = request_binding(
               snapshot,
               &intent_plan.groups,
               batch,
               config,
               debug_dir,
               &debug_name,
            )
            .await?;
            let repair_context = ambiguous_hunk_context(batch);
            let repair =
               evaluate_binding(&repair_assignments, &repair_context, &valid_group_ids, snapshot);
            for (group_id, hunk_ids) in repair.assigned {
               let entry = assigned_by_group.entry(group_id).or_default();
               for hunk_id in hunk_ids {
                  entry.insert(hunk_id);
               }
            }

            repair_unresolved.extend(repair.unresolved);
         }
         unresolved = order_hunk_ids(snapshot, &repair_unresolved);

         if !unresolved.is_empty() {
            assign_unresolved_hunks(
               &unresolved,
               &mut assigned_by_group,
               &ambiguous_files,
               &group_rank,
            );
         }
      }
   }

   let plan = finalize_executable_plan(snapshot, intent_plan, assigned_by_group)?;
   validate_executable_plan(snapshot, &plan)?;
   Ok(plan)
}

fn patch_signature(path: &str, hunks: &[&ComposeHunk]) -> String {
   let mut changed_lines = Vec::new();
   let mut synthetic = Vec::new();

   for hunk in hunks {
      if hunk.synthetic {
         synthetic.push(hunk.snippet.clone());
         continue;
      }

      for line in hunk.raw_patch.lines() {
         if (line.starts_with('+') && !line.starts_with("+++"))
            || (line.starts_with('-') && !line.starts_with("---"))
         {
            changed_lines.push(line.to_string());
         }
      }
   }

   let material = if changed_lines.is_empty() {
      synthetic.join("\n")
   } else {
      changed_lines.join("\n")
   };

   format!("{path}:{material}")
}

fn expected_remaining_signatures(
   snapshot: &ComposeSnapshot,
   remaining_hunk_ids: &HashSet<String>,
) -> BTreeMap<String, String> {
   let mut hunks_by_path: BTreeMap<String, Vec<&ComposeHunk>> = BTreeMap::new();
   for hunk in &snapshot.hunks {
      if remaining_hunk_ids.contains(&hunk.hunk_id) {
         hunks_by_path
            .entry(hunk.path.clone())
            .or_default()
            .push(hunk);
      }
   }

   hunks_by_path
      .into_iter()
      .map(|(path, hunks)| {
         let signature = patch_signature(&path, &hunks);
         (path, signature)
      })
      .collect()
}

fn current_snapshot_signatures(snapshot: &ComposeSnapshot) -> BTreeMap<String, String> {
   snapshot
      .files
      .iter()
      .map(|file| {
         let hunks = snapshot.hunks_for_file(&file.file_id);
         let signature = patch_signature(&file.path, &hunks);
         (file.path.clone(), signature)
      })
      .collect()
}

fn verify_remaining_snapshot(
   dir: &str,
   original_snapshot: &ComposeSnapshot,
   remaining_hunk_ids: &HashSet<String>,
) -> Result<()> {
   if remaining_hunk_ids.is_empty() {
      match get_compose_diff(dir) {
         Err(CommitGenError::NoChanges { .. }) => return Ok(()),
         Err(err) => return Err(err),
         Ok(_) => {
            return Err(CommitGenError::Other(
               "Compose expected no remaining changes, but working tree is still dirty".to_string(),
            ));
         },
      }
   }

   let current_diff = get_compose_diff(dir)?;
   let current_stat = get_compose_stat(dir)?;
   let current_snapshot = build_compose_snapshot(&current_diff, &current_stat)?;

   let expected = expected_remaining_signatures(original_snapshot, remaining_hunk_ids);
   let actual = current_snapshot_signatures(&current_snapshot);

   if expected != actual {
      return Err(CommitGenError::Other(format!(
         "Remaining compose snapshot diverged from expectation.\nExpected: {:?}\nActual: {:?}",
         expected.keys().collect::<Vec<_>>(),
         actual.keys().collect::<Vec<_>>()
      )));
   }

   Ok(())
}

fn print_executable_plan(snapshot: &ComposeSnapshot, plan: &ComposeExecutablePlan) {
   println!("\n{}", style::section_header("Proposed Commit Groups", 80));
   for (display_idx, &group_idx) in plan.dependency_order.iter().enumerate() {
      let group = &plan.groups[group_idx];
      let scope = group
         .scope
         .as_ref()
         .map(|scope| format!("({})", style::scope(scope.as_str())))
         .unwrap_or_default();

      println!(
         "\n{}. {} [{}{}] {}",
         display_idx + 1,
         style::bold(&group.group_id),
         style::commit_type(group.commit_type.as_str()),
         scope,
         group.rationale
      );

      println!("   Files:");
      for file_id in &group.file_ids {
         if let Some(file) = snapshot.file_by_id(file_id) {
            let selected_hunk_ids: Vec<&str> = group
               .hunk_ids
               .iter()
               .filter(|hunk_id| file.hunk_ids.contains(*hunk_id))
               .map(String::as_str)
               .collect();
            let selection = if selected_hunk_ids.len() == file.hunk_ids.len() {
               "all hunks".to_string()
            } else {
               selected_hunk_ids.join(", ")
            };
            println!("     - {} {} ({selection})", file.file_id, file.path);
         }
      }

      if !group.dependencies.is_empty() {
         println!("   Depends on: {}", group.dependencies.join(", "));
      }
   }
}

pub async fn execute_compose(
   snapshot: &ComposeSnapshot,
   plan: &ComposeExecutablePlan,
   config: &CommitConfig,
   args: &Args,
) -> Result<Vec<String>> {
   let dir = &args.dir;
   let mut remaining_hunk_ids: HashSet<String> = snapshot.all_hunk_ids().into_iter().collect();
   let mut commit_hashes = Vec::new();
   let total = plan.dependency_order.len();

   println!("{}", style::info("Resetting staging area..."));
   reset_staging(dir)?;

   // Phase 1: capture per-group diff/stat sequentially. Staging is a global
   // resource so we must serialize git operations, but each group's diff is
   // independent of the others (no two groups share a hunk).
   let mut group_diff_stats: Vec<(String, String)> = Vec::with_capacity(total);
   for (idx, &group_idx) in plan.dependency_order.iter().enumerate() {
      let group = &plan.groups[group_idx];
      println!(
         "  {}",
         style::info(&format!(
            "Capturing diff for {} ({}/{})",
            group.group_id,
            idx + 1,
            total,
         ))
      );
      stage_executable_group(snapshot, group, dir)?;
      let diff = get_git_diff(&Mode::Staged, None, dir, config)?;
      let stat = get_git_stat(&Mode::Staged, None, dir, config)?;
      group_diff_stats.push((diff, stat));
      reset_staging(dir)?;
   }

   // Phase 2: generate commit messages concurrently. Both LLM calls per group
   // (analysis + summary) run inside a single async task so the slower of the
   // two does not block other groups from progressing.
   println!(
      "{}",
      style::info(&format!(
         "Generating {total} commit message(s) in parallel (up to {} at a time)...",
         COMPOSE_MESSAGE_PARALLELISM.min(total).max(1)
      ))
   );

   let prepared_messages: Vec<(Vec<String>, crate::types::CommitSummary)> =
      stream::iter(plan.dependency_order.iter().enumerate())
         .map(|(idx, &group_idx)| {
            let group = &plan.groups[group_idx];
            let (diff, stat) = &group_diff_stats[idx];
            let debug_prefix = format!("compose-{}", idx + 1);
            async move {
               let ctx = AnalysisContext {
                  user_context:    Some(&group.rationale),
                  recent_commits:  None,
                  common_scopes:   None,
                  project_context: None,
                  debug_output:    args.debug_output.as_deref(),
                  debug_prefix:    Some(&debug_prefix),
               };
               let analysis = generate_conventional_analysis(
                  stat,
                  diff,
                  &config.analysis_model,
                  "",
                  &ctx,
                  config,
               )
               .await?;
               let body = analysis.body_texts();
               let summary = generate_summary_from_analysis(
                  stat,
                  group.commit_type.as_str(),
                  group.scope.as_ref().map(|scope| scope.as_str()),
                  &body,
                  Some(&group.rationale),
                  config,
                  args.debug_output.as_deref(),
                  Some(&debug_prefix),
               )
               .await?;
               Ok::<_, CommitGenError>((body, summary))
            }
         })
         .buffered(COMPOSE_MESSAGE_PARALLELISM.min(total).max(1))
         .collect::<Vec<_>>()
         .await
         .into_iter()
         .collect::<Result<Vec<_>>>()?;

   // Phase 3: sequential commit loop. Re-stage each group (cheap git ops) and
   // commit using the message we generated in phase 2.
   for (idx, &group_idx) in plan.dependency_order.iter().enumerate() {
      let group = &plan.groups[group_idx];

      println!(
         "\n[{}/{}] Creating commit {}: {}",
         idx + 1,
         total,
         group.group_id,
         group.rationale
      );
      println!("  Type: {}", style::commit_type(group.commit_type.as_str()));
      if let Some(scope) = &group.scope {
         println!("  Scope: {}", style::scope(scope.as_str()));
      }
      let paths: Vec<String> = group
         .file_ids
         .iter()
         .filter_map(|file_id| snapshot.file_by_id(file_id).map(|file| file.path.clone()))
         .collect();
      println!("  Files: {}", paths.join(", "));

      stage_executable_group(snapshot, group, dir)?;

      let (analysis_body, summary) = prepared_messages[idx].clone();
      let mut commit = ConventionalCommit {
         commit_type: group.commit_type.clone(),
         scope: group.scope.clone(),
         summary,
         body: analysis_body,
         footers: vec![],
      };
      post_process_commit_message(&mut commit, config);

      if let Err(err) = validate_commit_message(&commit, config) {
         eprintln!(
            "  {}",
            style::warning(&format!("{} Warning: Validation failed: {err}", style::icons::WARNING))
         );
      }

      let formatted_message = format_commit_message(&commit);
      println!(
         "  Message:\n{}",
         formatted_message
            .lines()
            .take(3)
            .collect::<Vec<_>>()
            .join("\n")
      );

      if !args.compose_preview {
         let sign = args.sign || config.gpg_sign;
         let signoff = args.signoff || config.signoff;
         git_commit(&formatted_message, false, dir, sign, signoff, args.skip_hooks, false)?;
         let hash = get_head_hash(dir)?;
         commit_hashes.push(hash);

         for hunk_id in &group.hunk_ids {
            remaining_hunk_ids.remove(hunk_id);
         }
         verify_remaining_snapshot(dir, snapshot, &remaining_hunk_ids)?;

         if args.compose_test_after_each {
            println!("  {}", style::info("Running tests..."));
            let status = std::process::Command::new("cargo")
               .arg("test")
               .current_dir(dir)
               .status();

            if let Ok(status) = status {
               if !status.success() {
                  return Err(CommitGenError::Other(format!(
                     "Tests failed after commit {} ({})",
                     idx + 1,
                     group.group_id
                  )));
               }
               println!("  {}", style::success(&format!("{} Tests passed", style::icons::SUCCESS)));
            }
         }
      }
   }

   Ok(commit_hashes)
}

pub async fn run_compose_mode(args: &Args, config: &CommitConfig) -> Result<()> {
   let max_rounds = config.compose_max_rounds;

   for round in 1..=max_rounds {
      if round > 1 {
         println!(
            "\n{}",
            style::section_header(&format!("Compose Round {round}/{max_rounds}"), 80)
         );
      } else {
         println!("{}", style::section_header("Compose Mode", 80));
      }
      println!("{}\n", style::info("Analyzing all changes for intelligent splitting..."));

      run_compose_round(args, config, round).await?;

      if args.compose_preview {
         break;
      }

      match get_compose_diff(&args.dir) {
         Err(CommitGenError::NoChanges { .. }) => {
            println!(
               "\n{}",
               style::success(&format!(
                  "{} All changes committed successfully",
                  style::icons::SUCCESS
               ))
            );
            break;
         },
         Err(err) => return Err(err),
         Ok(remaining_diff) => {
            eprintln!(
               "\n{}",
               style::warning(&format!(
                  "{} Uncommitted changes remain after round {round}",
                  style::icons::WARNING
               ))
            );
            eprintln!("{remaining_diff}");
         },
      }

      if round < max_rounds {
         eprintln!("{}", style::info("Starting another compose round..."));
      } else {
         eprintln!(
            "{}",
            style::warning(&format!(
               "Reached max rounds ({max_rounds}). Remaining changes need manual commit."
            ))
         );
      }
   }

   Ok(())
}

async fn run_compose_round(args: &Args, config: &CommitConfig, round: usize) -> Result<()> {
   let diff = get_compose_diff(&args.dir)?;
   let stat = get_compose_stat(&args.dir)?;
   let snapshot = build_compose_snapshot(&diff, &stat)?;

   if let Some(debug_dir) = args.debug_output.as_deref() {
      save_debug_artifact(
         Some(debug_dir),
         &format!("compose_round_{round}_snapshot.json"),
         &snapshot,
      )?;
   }

   let token_counter = create_token_counter(config);
   let observations = if should_collect_compose_observations(&snapshot, config, &token_counter) {
      println!("{}", style::info("Summarizing compose snapshot with map-reduce..."));
      observe_diff_files(&snapshot.diff, &config.analysis_model, config, &token_counter).await?
   } else {
      if planning_mode_for_snapshot(&snapshot) == PlanningMode::Area
         && should_use_map_reduce(&snapshot.diff, config, &token_counter)
      {
         println!(
            "{}",
            style::info(
               "Skipping per-file observations for very large compose snapshot; using area-level \
                planning instead."
            )
         );
      }
      Vec::new()
   };

   if let Some(debug_dir) = args.debug_output.as_deref()
      && !observations.is_empty()
   {
      save_debug_artifact(
         Some(debug_dir),
         &format!("compose_round_{round}_observations.json"),
         &observations,
      )?;
   }

   let max_commits = args.compose_max_commits.unwrap_or(20);
   let executable_plan = if let Some(cached_plan) =
      load_cached_plan(&args.dir, &snapshot, max_commits, &config.analysis_model)?
   {
      println!("{}", style::info("Reusing cached compose plan for identical snapshot..."));
      cached_plan
   } else {
      println!("{}", style::info(&format!("Planning changes (max {max_commits} commits)...")));
      let intent_plan = analyze_compose_intent(
         &snapshot,
         &observations,
         config,
         max_commits,
         args.debug_output.as_deref(),
      )
      .await?;

      if let Some(debug_dir) = args.debug_output.as_deref() {
         save_debug_artifact(
            Some(debug_dir),
            &format!("compose_round_{round}_intent_plan.json"),
            &intent_plan,
         )?;
      }

      println!("{}", style::info("Binding hunks to groups..."));
      let plan =
         bind_compose_plan(&snapshot, &intent_plan, config, args.debug_output.as_deref()).await?;
      save_cached_plan(&args.dir, &snapshot, max_commits, &config.analysis_model, &plan)?;
      plan
   };

   if let Some(debug_dir) = args.debug_output.as_deref() {
      save_debug_artifact(
         Some(debug_dir),
         &format!("compose_round_{round}_executable_plan.json"),
         &executable_plan,
      )?;
   }

   print_executable_plan(&snapshot, &executable_plan);

   if args.compose_preview {
      println!(
         "\n{}",
         style::success(&format!(
            "{} Preview complete (use --compose without --compose-preview to execute)",
            style::icons::SUCCESS
         ))
      );
      return Ok(());
   }

   println!("\n{}", style::info(&format!("Executing compose (round {round})...")));
   let hashes = execute_compose(&snapshot, &executable_plan, config, args).await?;
   println!(
      "{}",
      style::success(&format!(
         "{} Round {round}: Created {} commit(s)",
         style::icons::SUCCESS,
         hashes.len()
      ))
   );
   Ok(())
}

#[cfg(test)]
mod tests {
   use std::fmt::Write;

   use super::*;
   use crate::{config::CommitConfig, patch::build_compose_snapshot, types::CommitType};

   fn shared_file_diff() -> (&'static str, &'static str) {
      (
         r#"diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
-fn alpha() {
+fn alpha_changed() {
     println!("alpha");
 }
@@ -12,3 +12,3 @@
-fn beta() {
+fn beta_changed() {
     println!("beta");
 }
diff --git a/tests/lib.rs b/tests/lib.rs
index 3333333..4444444 100644
--- a/tests/lib.rs
+++ b/tests/lib.rs
@@ -1,3 +1,4 @@
 fn test_it() {
+    assert!(true);
 }
"#,
         " src/lib.rs | 4 ++--\n tests/lib.rs | 1 +\n",
      )
   }

   fn build_test_snapshot() -> ComposeSnapshot {
      let (diff, stat) = shared_file_diff();
      build_compose_snapshot(diff, stat).unwrap()
   }

   fn build_large_snapshot(file_count: usize, hunks_per_file: usize) -> ComposeSnapshot {
      let mut diff = String::new();

      for file_idx in 0..file_count {
         let path = format!("src/module_{file_idx:03}.rs");
         writeln!(diff, "diff --git a/{path} b/{path}").unwrap();
         diff.push_str("index 1111111..2222222 100644\n");
         writeln!(diff, "--- a/{path}").unwrap();
         writeln!(diff, "+++ b/{path}").unwrap();

         for hunk_idx in 0..hunks_per_file {
            let line_no = (hunk_idx * 4) + 1;
            writeln!(diff, "@@ -{line_no},1 +{line_no},1 @@").unwrap();
            writeln!(diff, "-old_{file_idx}_{hunk_idx}").unwrap();
            writeln!(diff, "+new_{file_idx}_{hunk_idx}").unwrap();
         }
      }

      build_compose_snapshot(&diff, "").unwrap()
   }

   fn build_multi_area_snapshot() -> ComposeSnapshot {
      let mut diff = String::new();
      let areas = [
         ("apps/frontend/src/server", 72),
         ("packages/model/src/models", 54),
         ("apps/daemon/src/worker", 43),
         (".github/workflows", 16),
      ];

      for (prefix, count) in areas {
         for file_idx in 0..count {
            let path = format!("{prefix}/file_{file_idx:03}.rs");
            writeln!(diff, "diff --git a/{path} b/{path}").unwrap();
            diff.push_str("index 1111111..2222222 100644\n");
            writeln!(diff, "--- a/{path}").unwrap();
            writeln!(diff, "+++ b/{path}").unwrap();
            diff.push_str("@@ -1,1 +1,1 @@\n");
            writeln!(diff, "-old_{file_idx}").unwrap();
            writeln!(diff, "+new_{file_idx}").unwrap();
         }
      }

      build_compose_snapshot(&diff, "").unwrap()
   }

   fn build_shared_intent_plan(snapshot: &ComposeSnapshot) -> ComposeIntentPlan {
      let source_file = snapshot.file_by_path("src/lib.rs").unwrap();
      let test_file = snapshot.file_by_path("tests/lib.rs").unwrap();
      let groups = vec![
         ComposeIntentGroup {
            group_id:     "G1".to_string(),
            commit_type:  CommitType::new("refactor").unwrap(),
            scope:        None,
            file_ids:     vec![source_file.file_id.clone(), test_file.file_id.clone()],
            rationale:    "implementation group".to_string(),
            dependencies: vec![],
         },
         ComposeIntentGroup {
            group_id:     "G2".to_string(),
            commit_type:  CommitType::new("refactor").unwrap(),
            scope:        None,
            file_ids:     vec![source_file.file_id.clone()],
            rationale:    "shared file follow-up".to_string(),
            dependencies: vec!["G1".to_string()],
         },
      ];
      let dependency_order =
         compute_dependency_order(&groups, |group| &group.group_id, |group| &group.dependencies)
            .unwrap();
      ComposeIntentPlan { groups, dependency_order }
   }

   #[test]
   fn test_auto_assign_hunks_marks_shared_file_ambiguous() {
      let snapshot = build_test_snapshot();
      let intent_plan = build_shared_intent_plan(&snapshot);
      let (assigned, ambiguous) = auto_assign_hunks(&snapshot, &intent_plan).unwrap();

      assert_eq!(ambiguous.len(), 1);
      let test_file = snapshot.file_by_path("tests/lib.rs").unwrap();
      let assigned_to_g1 = assigned.get("G1").unwrap();
      assert!(
         test_file
            .hunk_ids
            .iter()
            .all(|hunk_id| assigned_to_g1.contains(hunk_id)),
         "uniquely owned file should be auto-assigned"
      );
   }

   #[test]
   fn test_ambiguous_fallback_merges_and_prunes_empty_group() {
      let snapshot = build_test_snapshot();
      let intent_plan = build_shared_intent_plan(&snapshot);
      let (mut assigned, ambiguous_files) = auto_assign_hunks(&snapshot, &intent_plan).unwrap();
      let source_file = snapshot.file_by_path("src/lib.rs").unwrap();
      let hunk_context = ambiguous_hunk_context(&ambiguous_files);
      let valid_group_ids: HashSet<&str> = intent_plan
         .groups
         .iter()
         .map(|group| group.group_id.as_str())
         .collect();

      let evaluation = evaluate_binding(
         &[
            ComposeBindingAssignment {
               group_id: "G1".to_string(),
               hunk_ids: vec![source_file.hunk_ids[0].clone(), source_file.hunk_ids[1].clone()],
            },
            ComposeBindingAssignment {
               group_id: "G2".to_string(),
               hunk_ids: vec![source_file.hunk_ids[1].clone()],
            },
         ],
         &hunk_context,
         &valid_group_ids,
         &snapshot,
      );

      for (group_id, hunk_ids) in evaluation.assigned {
         let entry = assigned.entry(group_id).or_default();
         for hunk_id in hunk_ids {
            entry.insert(hunk_id);
         }
      }

      let group_rank: HashMap<&str, usize> = intent_plan
         .dependency_order
         .iter()
         .enumerate()
         .map(|(position, idx)| (intent_plan.groups[*idx].group_id.as_str(), position))
         .collect();
      assign_unresolved_hunks(&evaluation.unresolved, &mut assigned, &ambiguous_files, &group_rank);

      let executable_plan = finalize_executable_plan(&snapshot, &intent_plan, assigned).unwrap();
      assert_eq!(executable_plan.groups.len(), 1);
      assert_eq!(executable_plan.groups[0].group_id, "G1");
      assert!(
         source_file
            .hunk_ids
            .iter()
            .all(|hunk_id| executable_plan.groups[0].hunk_ids.contains(hunk_id)),
         "fallback should keep every hunk from the shared file in the surviving group"
      );
   }

   #[test]
   fn test_validate_executable_plan_rejects_overlap() {
      let snapshot = build_test_snapshot();
      let source_file = snapshot.file_by_path("src/lib.rs").unwrap();
      let executable_plan = ComposeExecutablePlan {
         groups:           vec![
            ComposeExecutableGroup {
               group_id:     "G1".to_string(),
               commit_type:  CommitType::new("refactor").unwrap(),
               scope:        None,
               file_ids:     vec![source_file.file_id.clone()],
               rationale:    "group one".to_string(),
               dependencies: vec![],
               hunk_ids:     vec![source_file.hunk_ids[0].clone()],
            },
            ComposeExecutableGroup {
               group_id:     "G2".to_string(),
               commit_type:  CommitType::new("refactor").unwrap(),
               scope:        None,
               file_ids:     vec![source_file.file_id.clone()],
               rationale:    "group two".to_string(),
               dependencies: vec![],
               hunk_ids:     vec![source_file.hunk_ids[0].clone(), source_file.hunk_ids[1].clone()],
            },
         ],
         dependency_order: vec![0, 1],
      };

      let err = validate_executable_plan(&snapshot, &executable_plan).unwrap_err();
      assert!(err.to_string().contains("assigned to both"));
   }

   #[test]
   fn test_normalize_intent_plan_maps_path_references_to_file_ids() {
      let snapshot = build_test_snapshot();
      let planning_index = build_planning_index(&snapshot);
      let groups = vec![ComposeIntentGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     vec!["src/lib.rs".to_string(), "`tests/lib.rs`".to_string()],
         rationale:    "normalize file references".to_string(),
         dependencies: vec![],
      }];

      let (normalized_groups, repair_notes) =
         normalize_intent_plan(&snapshot, &planning_index, groups).unwrap();

      assert_eq!(normalized_groups.len(), 1);
      assert_eq!(
         normalized_groups[0].file_ids,
         snapshot
            .files
            .iter()
            .map(|file| file.file_id.clone())
            .collect::<Vec<_>>()
      );
      assert_eq!(repair_notes.len(), 2);
   }

   #[test]
   fn test_normalize_intent_plan_repairs_missing_files() {
      let snapshot = build_test_snapshot();
      let planning_index = build_planning_index(&snapshot);
      let source_file = snapshot.file_by_path("src/lib.rs").unwrap();
      let test_file = snapshot.file_by_path("tests/lib.rs").unwrap();
      let groups = vec![ComposeIntentGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     vec![source_file.file_id.clone()],
         rationale:    "partial coverage".to_string(),
         dependencies: vec![],
      }];

      let (normalized_groups, repair_notes) =
         normalize_intent_plan(&snapshot, &planning_index, groups).unwrap();

      assert_eq!(normalized_groups.len(), 1);
      assert!(
         normalized_groups[0].file_ids.contains(&source_file.file_id),
         "existing file assignment should be preserved"
      );
      assert!(
         normalized_groups[0].file_ids.contains(&test_file.file_id),
         "missing files should be assigned to an existing group"
      );
      assert_eq!(repair_notes.len(), 1);
      assert!(repair_notes[0].contains(&test_file.file_id));
   }

   #[test]
   fn test_normalize_intent_plan_drops_placeholder_targets_and_repairs_dependencies() {
      let snapshot = build_multi_area_snapshot();
      let planning_index = build_planning_index(&snapshot);
      let frontend_target = planning_index
         .targets
         .iter()
         .find(|target| target.label.starts_with("apps/frontend"))
         .unwrap();
      let model_target = planning_index
         .targets
         .iter()
         .find(|target| target.label.starts_with("packages/model"))
         .unwrap();
      let groups = vec![
         ComposeIntentGroup {
            group_id:     "G1".to_string(),
            commit_type:  CommitType::new("refactor").unwrap(),
            scope:        Scope::new("apps/frontend").ok(),
            file_ids:     vec!["G3_PLACEHOLDER".to_string(), frontend_target.target_id.clone()],
            rationale:    "frontend platform updates".to_string(),
            dependencies: vec!["group 2".to_string(), "G1".to_string()],
         },
         ComposeIntentGroup {
            group_id:     "G2".to_string(),
            commit_type:  CommitType::new("refactor").unwrap(),
            scope:        Scope::new("packages/model").ok(),
            file_ids:     vec!["UNKNOWN_TARGET".to_string(), model_target.target_id.clone()],
            rationale:    "model storage updates".to_string(),
            dependencies: vec!["F5".to_string()],
         },
      ];

      let (normalized_groups, repair_notes) =
         normalize_intent_plan(&snapshot, &planning_index, groups).unwrap();

      assert_eq!(normalized_groups.len(), 2);
      assert!(
         normalized_groups[0]
            .file_ids
            .iter()
            .all(|file_id| file_id.starts_with('F'))
      );
      assert_eq!(normalized_groups[0].dependencies, vec!["G2".to_string()]);
      assert!(normalized_groups[1].dependencies.is_empty());
      assert!(
         repair_notes
            .iter()
            .any(|note| note.contains("Dropped unknown planning target"))
      );
      assert!(
         repair_notes
            .iter()
            .any(|note| note.contains("Dropped self-dependency"))
      );
      assert!(
         repair_notes
            .iter()
            .any(|note| note.contains("Mapped compose planner dependency"))
      );
      assert!(
         repair_notes
            .iter()
            .any(|note| note.contains("Dropped unknown dependency"))
      );
   }

   #[test]
   fn test_render_snapshot_summary_keeps_all_hunks_for_small_snapshot() {
      let snapshot = build_test_snapshot();
      let summary = render_snapshot_summary(&snapshot, &[]);
      let source_file = snapshot.file_by_path("src/lib.rs").unwrap();

      assert!(!summary.contains("# snapshot compacted"));
      for hunk_id in &source_file.hunk_ids {
         assert!(summary.contains(hunk_id));
      }
   }

   #[test]
   fn test_render_snapshot_summary_compacts_large_snapshot() {
      let snapshot = build_large_snapshot(160, 4);
      let summary = render_snapshot_summary(&snapshot, &[]);

      assert!(summary.contains("# snapshot compacted"));
      assert!(summary.contains("- F001 src/module_000.rs (+4/-4, 4 hunks)"));
      assert!(summary.contains("F001-H001"));
      assert!(summary.contains("F001-H004"));
      assert!(!summary.contains("F001-H002"));
      assert!(!summary.contains("F001-H003"));
      assert!(summary.contains("... 2 more hunks omitted from F001"));
   }

   #[test]
   fn test_build_planning_index_uses_area_targets_for_large_snapshot() {
      let snapshot = build_multi_area_snapshot();
      let planning_index = build_planning_index(&snapshot);

      assert_eq!(planning_index.mode, PlanningMode::Area);
      assert!(planning_index.targets.len() < snapshot.files.len());
      assert!(
         planning_index
            .targets
            .iter()
            .any(|target| target.label.starts_with("apps/frontend"))
      );
      assert!(
         render_planning_stat(&planning_index).contains("planning over"),
         "planning stat should explain the area mode"
      );
   }

   #[test]
   fn test_normalize_intent_plan_expands_area_targets() {
      let snapshot = build_multi_area_snapshot();
      let planning_index = build_planning_index(&snapshot);
      let midpoint = planning_index.targets.len() / 2;
      let first_group_targets: Vec<String> = planning_index
         .targets
         .iter()
         .take(midpoint)
         .map(|target| target.label.clone())
         .collect();
      let second_group_targets: Vec<String> = planning_index
         .targets
         .iter()
         .skip(midpoint)
         .map(|target| target.label.clone())
         .collect();
      let groups = vec![
         ComposeIntentGroup {
            group_id:     "G1".to_string(),
            commit_type:  CommitType::new("refactor").unwrap(),
            scope:        None,
            file_ids:     first_group_targets,
            rationale:    "frontend and model".to_string(),
            dependencies: vec![],
         },
         ComposeIntentGroup {
            group_id:     "G2".to_string(),
            commit_type:  CommitType::new("refactor").unwrap(),
            scope:        None,
            file_ids:     second_group_targets,
            rationale:    "daemon and ci".to_string(),
            dependencies: vec![],
         },
      ];

      let (normalized_groups, repair_notes) =
         normalize_intent_plan(&snapshot, &planning_index, groups).unwrap();

      assert_eq!(normalized_groups.len(), 2);
      assert!(
         normalized_groups
            .iter()
            .flat_map(|group| group.file_ids.iter())
            .all(|file_id| file_id.starts_with('F')),
         "area targets should expand back to concrete file IDs"
      );
      assert!(!repair_notes.is_empty());
      assert_eq!(
         normalized_groups
            .iter()
            .flat_map(|group| group.file_ids.iter())
            .collect::<HashSet<_>>()
            .len(),
         snapshot.files.len()
      );
   }

   #[test]
   fn test_large_patch_fallback_splits_monolithic_area_plan() {
      let snapshot = build_multi_area_snapshot();
      let planning_index = build_planning_index(&snapshot);
      let monolithic_group = ComposeIntentGroup {
         group_id:     "G1".to_string(),
         commit_type:  CommitType::new("refactor").unwrap(),
         scope:        None,
         file_ids:     snapshot
            .files
            .iter()
            .map(|file| file.file_id.clone())
            .collect(),
         rationale:    "repo-wide refactor".to_string(),
         dependencies: vec![],
      };

      assert!(should_force_large_patch_fallback(
         &snapshot,
         &planning_index,
         &[monolithic_group],
         6
      ));

      let fallback_groups =
         build_large_patch_fallback_groups(&snapshot, &planning_index, 6).unwrap();
      assert!(fallback_groups.len() >= 3);
      assert_eq!(
         fallback_groups
            .iter()
            .flat_map(|group| group.file_ids.iter())
            .collect::<HashSet<_>>()
            .len(),
         snapshot.files.len()
      );
      assert!(
         fallback_groups
            .iter()
            .any(|group| group.rationale.contains("frontend")),
         "fallback should preserve workstream identity"
      );
   }

   #[test]
   fn test_should_collect_compose_observations_skips_area_mode() {
      let snapshot = build_large_snapshot(160, 4);
      let config = CommitConfig::default();
      let counter = create_token_counter(&config);

      assert!(should_use_map_reduce(&snapshot.diff, &config, &counter));
      assert!(!should_collect_compose_observations(&snapshot, &config, &counter));
   }

   #[test]
   fn test_chunk_ambiguous_files_splits_large_binding_request() {
      let ambiguous_files = vec![
         AmbiguousFileBinding {
            file_id:             "F001".to_string(),
            path:                "src/alpha.rs".to_string(),
            candidate_group_ids: vec!["G1".to_string(), "G2".to_string()],
            hunk_ids:            (1..=70).map(|idx| format!("F001-H{idx:03}")).collect(),
         },
         AmbiguousFileBinding {
            file_id:             "F002".to_string(),
            path:                "src/beta.rs".to_string(),
            candidate_group_ids: vec!["G1".to_string(), "G3".to_string()],
            hunk_ids:            (1..=60).map(|idx| format!("F002-H{idx:03}")).collect(),
         },
         AmbiguousFileBinding {
            file_id:             "F003".to_string(),
            path:                "src/gamma.rs".to_string(),
            candidate_group_ids: vec!["G2".to_string(), "G3".to_string()],
            hunk_ids:            (1..=10).map(|idx| format!("F003-H{idx:03}")).collect(),
         },
      ];

      let batches = chunk_ambiguous_files(&ambiguous_files);
      let total_hunks: usize = batches
         .iter()
         .flatten()
         .map(|file| file.hunk_ids.len())
         .sum();

      assert_eq!(batches.len(), 2);
      assert_eq!(batches[0].len(), 1);
      assert_eq!(batches[1].len(), 2);
      assert_eq!(total_hunks, 140);
      assert!(batches.iter().all(|batch| {
         batch.len() <= MAX_BIND_FILES_PER_REQUEST
            && batch.iter().map(|file| file.hunk_ids.len()).sum::<usize>()
               <= MAX_BIND_HUNKS_PER_REQUEST
      }));
   }
}
