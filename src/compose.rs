use std::{
   collections::{BTreeMap, BTreeSet, HashMap, HashSet},
   fmt::Write,
   fs,
   path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
   api::{
      AnalysisContext, OneShotDebug, OneShotSpec, generate_conventional_analysis,
      generate_summary_from_analysis, run_oneshot, strict_json_schema,
   },
   compose_types::{
      ComposeBindingAssignment, ComposeExecutableGroup, ComposeExecutablePlan, ComposeHunk,
      ComposeIntentGroup, ComposeIntentPlan, ComposeSnapshot,
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
   style,
   tokens::create_token_counter,
   types::{Args, CommitType, ConventionalCommit, Mode},
   validation::validate_commit_message,
};

const COMPOSE_INTENT_SYSTEM_PROMPT: &str = r"You plan atomic git commits from a pre-parsed snapshot of changes.

Rules:
1. Return 1-{MAX_COMMITS} logical groups.
2. Use file IDs only. Do not emit hunk IDs in this phase.
3. Every file ID must appear in at least one group.
4. If one file contains changes for multiple logical commits, repeat that file ID across the relevant groups.
5. Prefer fewer groups over speculative splits.
6. Dependencies must use group IDs, not numeric indices.
7. Keep groups buildable in dependency order.
";

const COMPOSE_BIND_SYSTEM_PROMPT: &str = r"You bind pre-parsed hunk IDs to existing commit groups.

Rules:
1. Use only the provided group IDs and hunk IDs.
2. Every hunk ID must be assigned to exactly one group.
3. Only assign a hunk to one of its candidate groups.
4. Prefer keeping related hunks together.
5. Prefer fewer splits when uncertain.
";

const MAX_OBSERVATIONS_PER_FILE: usize = 3;
const COMPOSE_PLAN_SCHEMA_VERSION: &str = "v2";
const COMPOSE_PLANNER_TEMPERATURE: f32 = 0.0;

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

fn render_snapshot_summary(snapshot: &ComposeSnapshot, observations: &[FileObservation]) -> String {
   let observations_by_file: HashMap<&str, Vec<&str>> = observations
      .iter()
      .map(|observation| {
         (
            observation.file.as_str(),
            observation
               .observations
               .iter()
               .map(String::as_str)
               .take(MAX_OBSERVATIONS_PER_FILE)
               .collect(),
         )
      })
      .collect();

   let mut out = String::new();
   for file in &snapshot.files {
      writeln!(out, "- {} {}", file.file_id, file.summary).unwrap();
      if let Some(file_observations) = observations_by_file.get(file.path.as_str()) {
         for observation in file_observations {
            writeln!(out, "  observation: {observation}").unwrap();
         }
      }

      for hunk_id in &file.hunk_ids {
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
                     "description": "File IDs that belong to this logical commit. Repeat file IDs across groups when a file is shared.",
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

fn validate_intent_plan(snapshot: &ComposeSnapshot, groups: &[ComposeIntentGroup]) -> Result<()> {
   if groups.is_empty() {
      return Err(CommitGenError::Other("Compose intent plan returned no groups".to_string()));
   }

   let known_file_ids: HashSet<&str> = snapshot
      .files
      .iter()
      .map(|file| file.file_id.as_str())
      .collect();
   let mut covered_file_ids = HashSet::new();

   for group in groups {
      if group.file_ids.is_empty() {
         return Err(CommitGenError::Other(format!(
            "Compose group {} does not reference any files",
            group.group_id
         )));
      }

      for file_id in &group.file_ids {
         if !known_file_ids.contains(file_id.as_str()) {
            return Err(CommitGenError::Other(format!(
               "Compose group {} references unknown file_id {}",
               group.group_id, file_id
            )));
         }
         covered_file_ids.insert(file_id.as_str());
      }
   }

   let missing: Vec<&str> = snapshot
      .files
      .iter()
      .filter_map(|file| {
         (!covered_file_ids.contains(file.file_id.as_str())).then_some(file.file_id.as_str())
      })
      .collect();

   if !missing.is_empty() {
      return Err(CommitGenError::Other(format!(
         "Compose intent plan did not cover all files: {}",
         missing.join(", ")
      )));
   }

   Ok(())
}

async fn analyze_compose_intent(
   snapshot: &ComposeSnapshot,
   observations: &[FileObservation],
   config: &CommitConfig,
   max_commits: usize,
   debug_dir: Option<&Path>,
) -> Result<ComposeIntentPlan> {
   let snapshot_summary = render_snapshot_summary(snapshot, observations);
   let schema = build_intent_schema(config);
   let user_prompt = format!(
      "Plan 1-{max_commits} logical commits.\n\n## Git Stat\n{}\n\n## Snapshot\n{}",
      snapshot.stat, snapshot_summary
   );

   let response = run_oneshot::<ComposeIntentResponse>(config, &OneShotSpec {
      operation:        "compose/intent",
      model:            &config.analysis_model,
      max_tokens:       3000,
      temperature:      COMPOSE_PLANNER_TEMPERATURE,
      prompt_family:    "compose-intent",
      prompt_variant:   "default",
      system_prompt:    &COMPOSE_INTENT_SYSTEM_PROMPT
         .replace("{MAX_COMMITS}", &max_commits.to_string()),
      user_prompt:      &user_prompt,
      tool_name:        "create_compose_intent_plan",
      tool_description: "Plan logical commit groups over file IDs",
      schema:           &schema,
      debug:            debug_dir.map(|dir| OneShotDebug {
         dir:    Some(dir),
         prefix: None,
         name:   "compose_intent",
      }),
   })
   .await?;

   validate_intent_plan(snapshot, &response.output.groups)?;
   let dependency_order = compute_dependency_order(
      &response.output.groups,
      |group| &group.group_id,
      |group| &group.dependencies,
   )?;

   Ok(ComposeIntentPlan { groups: response.output.groups, dependency_order })
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

fn render_binding_prompt(
   snapshot: &ComposeSnapshot,
   groups: &[ComposeIntentGroup],
   ambiguous_files: &[AmbiguousFileBinding],
) -> String {
   let mut out = String::new();
   out.push_str("Assign each hunk to one of its candidate groups.\n\n");
   out.push_str("## Groups\n");
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

   out.push_str("\n## Ambiguous Files\n");
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
   debug_name: &'static str,
) -> Result<Vec<ComposeBindingAssignment>> {
   let schema = build_binding_schema();
   let user_prompt = render_binding_prompt(snapshot, groups, ambiguous_files);
   let response = run_oneshot::<ComposeBindingResponse>(config, &OneShotSpec {
      operation:        "compose/bind",
      model:            &config.analysis_model,
      max_tokens:       2500,
      temperature:      COMPOSE_PLANNER_TEMPERATURE,
      prompt_family:    "compose-bind",
      prompt_variant:   "default",
      system_prompt:    COMPOSE_BIND_SYSTEM_PROMPT,
      user_prompt:      &user_prompt,
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
      let hunk_context = ambiguous_hunk_context(&ambiguous_files);

      let assignments = request_binding(
         snapshot,
         &intent_plan.groups,
         &ambiguous_files,
         config,
         debug_dir,
         "compose_bind",
      )
      .await?;
      let evaluation = evaluate_binding(&assignments, &hunk_context, &valid_group_ids, snapshot);
      for (group_id, hunk_ids) in evaluation.assigned {
         let entry = assigned_by_group.entry(group_id).or_default();
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

      let mut unresolved = evaluation.unresolved;
      if !unresolved.is_empty() {
         let unresolved_files = filter_ambiguous_files(&ambiguous_files, &unresolved);
         let repair_assignments = request_binding(
            snapshot,
            &intent_plan.groups,
            &unresolved_files,
            config,
            debug_dir,
            "compose_bind_repair",
         )
         .await?;
         let repair_context = ambiguous_hunk_context(&unresolved_files);
         let repair =
            evaluate_binding(&repair_assignments, &repair_context, &valid_group_ids, snapshot);
         for (group_id, hunk_ids) in repair.assigned {
            let entry = assigned_by_group.entry(group_id).or_default();
            for hunk_id in hunk_ids {
               entry.insert(hunk_id);
            }
         }
         unresolved = repair.unresolved;

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

   println!("{}", style::info("Resetting staging area..."));
   reset_staging(dir)?;

   for (idx, &group_idx) in plan.dependency_order.iter().enumerate() {
      let group = &plan.groups[group_idx];

      println!(
         "\n[{}/{}] Creating commit {}: {}",
         idx + 1,
         plan.dependency_order.len(),
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

      let diff = get_git_diff(&Mode::Staged, None, dir, config)?;
      let stat = get_git_stat(&Mode::Staged, None, dir, config)?;

      println!("  {}", style::info("Generating commit message..."));
      let debug_prefix = format!("compose-{}", idx + 1);
      let ctx = AnalysisContext {
         user_context:    Some(&group.rationale),
         recent_commits:  None,
         common_scopes:   None,
         project_context: None,
         debug_output:    args.debug_output.as_deref(),
         debug_prefix:    Some(&debug_prefix),
      };
      let message_analysis =
         generate_conventional_analysis(&stat, &diff, &config.analysis_model, "", &ctx, config)
            .await?;

      let analysis_body = message_analysis.body_texts();
      let summary = generate_summary_from_analysis(
         &stat,
         group.commit_type.as_str(),
         group.scope.as_ref().map(|scope| scope.as_str()),
         &analysis_body,
         Some(&group.rationale),
         config,
         args.debug_output.as_deref(),
         Some(&debug_prefix),
      )
      .await?;

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
   let observations = if should_use_map_reduce(&snapshot.diff, config, &token_counter) {
      println!("{}", style::info("Summarizing large compose snapshot with map-reduce..."));
      observe_diff_files(&snapshot.diff, &config.analysis_model, config, &token_counter).await?
   } else {
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
   use super::*;
   use crate::{patch::build_compose_snapshot, types::CommitType};

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
}
