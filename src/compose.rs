use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{
   api::{AnalysisContext, generate_conventional_analysis, openai_prompt_cache_key},
   config::CommitConfig,
   diff::smart_truncate_diff,
   error::{CommitGenError, Result},
   git::{get_git_diff, get_git_stat, get_head_hash, git_commit},
   normalization::{format_commit_message, post_process_commit_message},
   patch::{reset_staging, stage_group_changes},
   style,
   tokens::create_token_counter,
   types::{Args, ChangeGroup, CommitType, ComposeAnalysis, ConventionalCommit, Mode},
   validation::validate_commit_message,
};

#[derive(Debug, Serialize)]
struct Message {
   role:    String,
   content: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct FunctionParameters {
   #[serde(rename = "type")]
   param_type: String,
   properties: serde_json::Value,
   required:   Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Function {
   name:        String,
   description: String,
   parameters:  FunctionParameters,
}

#[derive(Debug, Serialize, Deserialize)]
struct Tool {
   #[serde(rename = "type")]
   tool_type: String,
   function:  Function,
}

#[derive(Debug, Serialize)]
struct ApiRequest {
   model:            String,
   max_tokens:       u32,
   temperature:      f32,
   tools:            Vec<Tool>,
   #[serde(skip_serializing_if = "Option::is_none")]
   tool_choice:      Option<serde_json::Value>,
   #[serde(skip_serializing_if = "Option::is_none")]
   prompt_cache_key: Option<String>,
   messages:         Vec<Message>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ToolCall {
   function: FunctionCall,
}

#[derive(Debug, Deserialize, Serialize)]
struct FunctionCall {
   name:      String,
   arguments: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct Choice {
   message: ResponseMessage,
}

#[derive(Debug, Deserialize, Serialize)]
struct ResponseMessage {
   #[serde(default)]
   tool_calls:    Vec<ToolCall>,
   #[serde(default)]
   content:       Option<String>,
   #[serde(default)]
   function_call: Option<FunctionCall>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ApiResponse {
   choices: Vec<Choice>,
}

const COMPOSE_SYSTEM_PROMPT: &str = r#"You split git diffs into logical, atomic commit groups for compose mode.

## Rules (CRITICAL)
1. **EXHAUSTIVENESS**: You MUST account for 100% of changes. Every file and hunk in the provided diff must appear in exactly one group.
2. **Atomicity**: Each group represents ONE logical change (feat/fix/refactor/etc.) that leaves codebase working.
3. **Prefer fewer groups**: Default to 1-3 commits. Only split when changes are truly independent/separable.
4. **Group related**: Implementation + tests go together. Refactoring + usage updates go together.
5. **Dependencies**: Use indices. Group 2 depending on Group 1 means: dependencies: [0].
6. **Hunk selection** (IMPORTANT - Use line numbers, NOT hunk headers):
   - If entire file → hunks: ["ALL"]
   - If partial → specify line ranges: hunks: [{start: 10, end: 25}, {start: 50, end: 60}]
   - Line numbers are 1-indexed from the ORIGINAL file (look at "-" lines in diff)
   - You can specify multiple ranges for discontinuous changes in one file

## Good Example (2 independent changes)
groups: [
  {
    changes: [
      {path: "src/api.rs", hunks: ["ALL"]},
      {path: "tests/api_test.rs", hunks: [{start: 15, end: 23}]}
    ],
    type: "feat", scope: "api", rationale: "add user endpoint with test",
    dependencies: []
  },
  {
    changes: [
      {path: "src/utils.rs", hunks: [{start: 42, end: 48}, {start: 100, end: 105}]}
    ],
    type: "fix", scope: "utils", rationale: "fix string parsing bug in two locations",
    dependencies: []
  }
]

## Bad Example (over-splitting)
❌ DON'T create 6 commits for: function rename + call sites. That's ONE refactor group.
❌ DON'T split tests from implementation unless they test something from a prior group.

## Bad Example (incomplete)
❌ DON'T forget files. If diff shows 5 files, groups must cover all 5.

Return groups in dependency order."#;

const COMPOSE_USER_PROMPT: &str = r#"Split this git diff into 1-{MAX_COMMITS} logical, atomic commit groups.

## Git Stat
{STAT}

## Git Diff
{DIFF}"#;

#[derive(Deserialize)]
struct ComposeResult {
   groups: Vec<ChangeGroup>,
}

fn parse_compose_groups_from_content(content: &str) -> Result<Vec<ChangeGroup>> {
   fn try_parse(input: &str) -> Option<Vec<ChangeGroup>> {
      let trimmed = input.trim();
      if trimmed.is_empty() {
         return None;
      }

      serde_json::from_str::<ComposeResult>(trimmed)
         .map(|r| r.groups)
         .ok()
   }

   let trimmed = content.trim();
   if trimmed.is_empty() {
      return Err(CommitGenError::Other(
         "Model returned an empty compose analysis response".to_string(),
      ));
   }

   if let Some(groups) = try_parse(trimmed) {
      return Ok(groups);
   }

   if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}'))
      && end >= start
   {
      let candidate = &trimmed[start..=end];
      if let Some(groups) = try_parse(candidate) {
         return Ok(groups);
      }
   }

   let segments: Vec<&str> = trimmed.split("```").collect();
   for (idx, segment) in segments.iter().enumerate() {
      if idx % 2 == 1 {
         let block = segment.trim();
         let mut lines = block.lines();
         let first_line = lines.next().unwrap_or_default();

         let rest_owned: String;
         let json_candidate = if first_line.trim_start().starts_with('{') {
            block
         } else {
            let rest: String = lines.collect::<Vec<_>>().join("\n");
            let trimmed_rest = rest.trim();
            if trimmed_rest.is_empty() {
               block
            } else {
               rest_owned = trimmed_rest.to_string();
               &rest_owned
            }
         };

         if let Some(groups) = try_parse(json_candidate) {
            return Ok(groups);
         }
      }
   }

   Err(CommitGenError::Other("Failed to parse compose analysis from model response".to_string()))
}

fn parse_compose_groups_from_json(
   raw: &str,
) -> std::result::Result<Vec<ChangeGroup>, serde_json::Error> {
   let trimmed = raw.trim();
   if trimmed.starts_with('[') {
      serde_json::from_str::<Vec<ChangeGroup>>(trimmed)
   } else {
      serde_json::from_str::<ComposeResult>(trimmed).map(|r| r.groups)
   }
}

fn debug_failed_payload(source: &str, payload: &str, err: &serde_json::Error) {
   let preview = payload.trim();
   let preview = if preview.len() > 2000 {
      format!("{}…", &preview[..2000])
   } else {
      preview.to_string()
   };
   eprintln!("Compose debug: failed to parse {source} payload ({err}); preview: {preview}");
}

fn group_affects_only_dependency_files(group: &ChangeGroup) -> bool {
   group
      .changes
      .iter()
      .all(|change| is_dependency_manifest(&change.path))
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

/// Call AI to analyze and group changes for compose mode
pub async fn analyze_for_compose(
   diff: &str,
   stat: &str,
   config: &CommitConfig,
   max_commits: usize,
) -> Result<ComposeAnalysis> {
   let client = crate::api::get_client(config);

   let tool = Tool {
      tool_type: "function".to_string(),
      function:  Function {
         name:        "create_compose_analysis".to_string(),
         description: "Split changes into logical commit groups with dependencies".to_string(),
         parameters:  FunctionParameters {
            param_type: "object".to_string(),
            properties: serde_json::json!({
               "groups": {
                  "type": "array",
                  "description": "Array of change groups in dependency order",
                  "items": {
                     "type": "object",
                     "properties": {
                        "changes": {
                           "type": "array",
                           "description": "File changes with specific hunks",
                           "items": {
                              "type": "object",
                              "properties": {
                                 "path": {
                                    "type": "string",
                                    "description": "File path"
                                 },
                                 "hunks": {
                                    "type": "array",
                                    "description": "Either ['ALL'] for entire file, or line range objects: [{start: 10, end: 25}]. Line numbers are 1-indexed from ORIGINAL file.",
                                    "items": {
                                       "oneOf": [
                                          { "type": "string", "const": "ALL" },
                                          {
                                             "type": "object",
                                             "properties": {
                                                "start": { "type": "integer", "minimum": 1 },
                                                "end": { "type": "integer", "minimum": 1 }
                                             },
                                             "required": ["start", "end"]
                                          }
                                       ]
                                    }
                                 }
                              },
                              "required": ["path", "hunks"]
                           }
                        },
                        "type": {
                           "type": "string",
                           "enum": ["feat", "fix", "refactor", "docs", "test", "chore", "style", "perf", "build", "ci", "revert"],
                           "description": "Commit type for this group"
                        },
                        "scope": {
                           "type": "string",
                           "description": "Optional scope (module/component). Omit if broad."
                        },
                        "rationale": {
                           "type": "string",
                           "description": "Brief explanation of why these changes belong together"
                        },
                        "dependencies": {
                           "type": "array",
                           "description": "Indices of groups this depends on (e.g., [0, 1])",
                           "items": { "type": "integer" }
                        }
                     },
                     "required": ["changes", "type", "rationale", "dependencies"]
                  }
               }
            }),
            required:   vec!["groups".to_string()],
         },
      },
   };

   let user_prompt = COMPOSE_USER_PROMPT
      .replace("{STAT}", stat)
      .replace("{DIFF}", diff)
      .replace("{MAX_COMMITS}", &max_commits.to_string());
   let prompt_cache_key =
      openai_prompt_cache_key(config, &config.model, "compose", "default", COMPOSE_SYSTEM_PROMPT);

   let request = ApiRequest {
      model: config.model.clone(),
      max_tokens: 8000,
      temperature: config.temperature,
      tools: vec![tool],
      tool_choice: Some(
         serde_json::json!({ "type": "function", "function": { "name": "create_compose_analysis" } }),
      ),
      prompt_cache_key,
      messages: vec![
         Message { role: "system".to_string(), content: COMPOSE_SYSTEM_PROMPT.to_string() },
         Message { role: "user".to_string(), content: user_prompt },
      ],
   };

   let mut request_builder = client
      .post(format!("{}/chat/completions", config.api_base_url))
      .header("content-type", "application/json");

   if let Some(api_key) = &config.api_key {
      request_builder = request_builder.header("Authorization", format!("Bearer {api_key}"));
   }

   let (status, response_text) =
      crate::api::timed_send(request_builder.json(&request), "compose", &config.model).await?;
   if !status.is_success() {
      return Err(CommitGenError::ApiError { status: status.as_u16(), body: response_text });
   }

   let api_response: ApiResponse =
      serde_json::from_str(&response_text).map_err(CommitGenError::JsonError)?;

   if api_response.choices.is_empty() {
      return Err(CommitGenError::Other(
         "API returned empty response for compose analysis".to_string(),
      ));
   }

   let mut last_parse_error: Option<CommitGenError> = None;

   for choice in &api_response.choices {
      let message = &choice.message;

      if let Some(tool_call) = message.tool_calls.first()
         && tool_call.function.name.ends_with("create_compose_analysis")
      {
         let args = &tool_call.function.arguments;
         match parse_compose_groups_from_json(args) {
            Ok(groups) => {
               let dependency_order = compute_dependency_order(&groups)?;
               return Ok(ComposeAnalysis { groups, dependency_order });
            },
            Err(err) => {
               debug_failed_payload("tool_call", args, &err);
               last_parse_error =
                  Some(CommitGenError::Other(format!("Failed to parse compose analysis: {err}")));
            },
         }
      }

      if let Some(function_call) = &message.function_call
         && function_call.name == "create_compose_analysis"
      {
         let args = &function_call.arguments;
         match parse_compose_groups_from_json(args) {
            Ok(groups) => {
               let dependency_order = compute_dependency_order(&groups)?;
               return Ok(ComposeAnalysis { groups, dependency_order });
            },
            Err(err) => {
               debug_failed_payload("function_call", args, &err);
               last_parse_error =
                  Some(CommitGenError::Other(format!("Failed to parse compose analysis: {err}")));
            },
         }
      }

      if let Some(content) = &message.content {
         match parse_compose_groups_from_content(content) {
            Ok(groups) => {
               let dependency_order = compute_dependency_order(&groups)?;
               return Ok(ComposeAnalysis { groups, dependency_order });
            },
            Err(err) => last_parse_error = Some(err),
         }
      }
   }

   if let Some(err) = last_parse_error {
      debug_compose_response(&api_response);
      return Err(err);
   }

   debug_compose_response(&api_response);
   Err(CommitGenError::Other("No compose analysis found in API response".to_string()))
}

fn debug_compose_response(response: &ApiResponse) {
   let raw_preview = serde_json::to_string(response).map_or_else(
      |_| "<failed to serialize response>".to_string(),
      |json| {
         if json.len() > 4000 {
            format!("{}…", &json[..4000])
         } else {
            json
         }
      },
   );

   eprintln!(
      "Compose debug: received {} choice(s) from analysis model\n  raw: {}",
      response.choices.len(),
      raw_preview
   );

   for (idx, choice) in response.choices.iter().enumerate() {
      let message = &choice.message;
      let tool_call = message.tool_calls.first();
      let tool_name = tool_call.map_or("<none>", |tc| tc.function.name.as_str());
      let tool_args_len = tool_call.map_or(0, |tc| tc.function.arguments.len());

      let function_call_name = message
         .function_call
         .as_ref()
         .map_or("<none>", |fc| fc.name.as_str());
      let function_call_args_len = message
         .function_call
         .as_ref()
         .map_or(0, |fc| fc.arguments.len());

      let content_preview = message.content.as_deref().map_or_else(
         || "<none>".to_string(),
         |c| {
            let trimmed = c.trim();
            if trimmed.len() > 200 {
               format!("{}…", &trimmed[..200])
            } else {
               trimmed.to_string()
            }
         },
      );

      eprintln!(
         "Choice #{idx}: tool_call={tool_name} (args {tool_args_len} chars), \
          function_call={function_call_name} (args {function_call_args_len} chars), \
          content_preview={content_preview}"
      );
   }
}

/// Compute topological order for commit groups based on dependencies
fn compute_dependency_order(groups: &[ChangeGroup]) -> Result<Vec<usize>> {
   let n = groups.len();
   let mut in_degree = vec![0; n];
   let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];

   // Build graph
   for (i, group) in groups.iter().enumerate() {
      for &dep in &group.dependencies {
         if dep >= n {
            return Err(CommitGenError::Other(format!(
               "Invalid dependency index {dep} (max: {n})"
            )));
         }
         adjacency[dep].push(i);
         in_degree[i] += 1;
      }
   }

   // Kahn's algorithm for topological sort
   let mut queue: Vec<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
   let mut order = Vec::new();

   while let Some(node) = queue.pop() {
      order.push(node);
      for &neighbor in &adjacency[node] {
         in_degree[neighbor] -= 1;
         if in_degree[neighbor] == 0 {
            queue.push(neighbor);
         }
      }
   }

   if order.len() != n {
      return Err(CommitGenError::Other(
         "Circular dependency detected in commit groups".to_string(),
      ));
   }

   Ok(order)
}

/// Validate groups for exhaustiveness and correctness
fn validate_compose_groups(groups: &[ChangeGroup], full_diff: &str) -> Result<()> {
   use std::collections::{HashMap, HashSet};

   // Extract all files from diff
   let mut diff_files: HashSet<String> = HashSet::new();
   for line in full_diff.lines() {
      if line.starts_with("diff --git")
         && let Some(b_part) = line.split_whitespace().nth(3)
         && let Some(path) = b_part.strip_prefix("b/")
      {
         diff_files.insert(path.to_string());
      }
   }

   // Track which files are covered by groups
   let mut covered_files: HashSet<String> = HashSet::new();
   let mut file_coverage: HashMap<String, usize> = HashMap::new();

   for (idx, group) in groups.iter().enumerate() {
      for change in &group.changes {
         covered_files.insert(change.path.clone());
         *file_coverage.entry(change.path.clone()).or_insert(0) += 1;

         // Validate hunk selectors
         for selector in &change.hunks {
            match selector {
               crate::types::HunkSelector::All => {},
               crate::types::HunkSelector::Lines { start, end } => {
                  if start > end {
                     eprintln!(
                        "{}",
                        style::warning(&format!(
                           "{} Warning: Group {idx} has invalid line range {start}-{end} in {}",
                           style::icons::WARNING,
                           change.path
                        ))
                     );
                  }
                  if *start == 0 {
                     eprintln!(
                        "{}",
                        style::warning(&format!(
                           "{} Warning: Group {idx} has line range starting at 0 (should be \
                            1-indexed) in {}",
                           style::icons::WARNING,
                           change.path
                        ))
                     );
                  }
               },
               crate::types::HunkSelector::Search { pattern } => {
                  if pattern.is_empty() {
                     eprintln!(
                        "{}",
                        style::warning(&format!(
                           "{} Warning: Group {idx} has empty search pattern in {}",
                           style::icons::WARNING,
                           change.path
                        ))
                     );
                  }
               },
            }
         }
      }

      // Check for invalid dependency indices
      for &dep in &group.dependencies {
         if dep >= groups.len() {
            return Err(CommitGenError::Other(format!(
               "Group {idx} has invalid dependency {dep} (only {} groups total)",
               groups.len()
            )));
         }
         if dep == idx {
            return Err(CommitGenError::Other(format!("Group {idx} depends on itself (circular)")));
         }
      }
   }

   // Check for missing files
   let missing_files: Vec<&String> = diff_files.difference(&covered_files).collect();
   if !missing_files.is_empty() {
      eprintln!(
         "{}",
         style::warning(&format!(
            "{} Warning: Groups don't cover all files. Missing:",
            style::icons::WARNING
         ))
      );
      for file in &missing_files {
         eprintln!("   - {file}");
      }
      return Err(CommitGenError::Other(format!(
         "Non-exhaustive groups: {} file(s) not covered",
         missing_files.len()
      )));
   }

   // Check for duplicate file coverage
   let duplicates: Vec<_> = file_coverage
      .iter()
      .filter(|&(_, count)| *count > 1)
      .collect();

   if !duplicates.is_empty() {
      eprintln!(
         "{}",
         style::warning(&format!(
            "{} Warning: Some files appear in multiple groups:",
            style::icons::WARNING
         ))
      );
      for (file, count) in duplicates {
         eprintln!("   - {file} ({count} times)");
      }
   }

   // Warn if empty groups
   for (idx, group) in groups.iter().enumerate() {
      if group.changes.is_empty() {
         return Err(CommitGenError::Other(format!("Group {idx} has no changes")));
      }
   }

   Ok(())
}

/// Execute compose: stage groups, generate messages, create commits
pub async fn execute_compose(
   analysis: &ComposeAnalysis,
   config: &CommitConfig,
   args: &Args,
) -> Result<Vec<String>> {
   let dir = &args.dir;
   let token_counter = create_token_counter(config);

   // Reset staging area
   println!("{}", style::info("Resetting staging area..."));
   reset_staging(dir)?;

   // Capture the full diff against the original HEAD once so we can reuse the same
   // hunk metadata even after earlier groups move HEAD forward.
   let baseline_diff_output = std::process::Command::new("git")
      .args(["diff", "HEAD"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to get baseline diff: {e}")))?;

   if !baseline_diff_output.status.success() {
      let stderr = String::from_utf8_lossy(&baseline_diff_output.stderr);
      return Err(CommitGenError::git(format!("git diff HEAD failed: {stderr}")));
   }

   let baseline_diff = String::from_utf8_lossy(&baseline_diff_output.stdout).to_string();

   let mut commit_hashes = Vec::new();

   for (idx, &group_idx) in analysis.dependency_order.iter().enumerate() {
      let mut group = analysis.groups[group_idx].clone();
      let dependency_only = group_affects_only_dependency_files(&group);

      if dependency_only && group.commit_type.as_str() != "build" {
         group.commit_type = CommitType::new("build")?;
      }

      println!(
         "\n[{}/{}] Creating commit for group: {}",
         idx + 1,
         analysis.dependency_order.len(),
         group.rationale
      );
      println!("  Type: {}", style::commit_type(&group.commit_type.to_string()));
      if let Some(scope) = &group.scope {
         println!("  Scope: {}", style::scope(&scope.to_string()));
      }
      let files: Vec<String> = group.changes.iter().map(|c| c.path.clone()).collect();
      println!("  Files: {}", files.join(", "));

      // Stage changes for this group (with hunk awareness)
      stage_group_changes(&group, dir, &baseline_diff)?;

      // Get diff and stat for this specific group
      let diff = get_git_diff(&Mode::Staged, None, dir, config)?;
      let stat = get_git_stat(&Mode::Staged, None, dir, config)?;

      // Truncate if needed
      let diff = if diff.len() > config.max_diff_length {
         smart_truncate_diff(&diff, config.max_diff_length, config, &token_counter)
      } else {
         diff
      };

      // Generate commit message using existing infrastructure
      println!("  {}", style::info("Generating commit message..."));
      let debug_prefix = format!("compose-{}", idx + 1);
      let ctx = AnalysisContext {
         user_context:    Some(&group.rationale),
         recent_commits:  None, // No recent commits for compose mode
         common_scopes:   None, // No common scopes for compose mode
         project_context: None, // No project context for compose mode
         debug_output:    args.debug_output.as_deref(),
         debug_prefix:    Some(&debug_prefix),
      };
      let message_analysis =
         generate_conventional_analysis(&stat, &diff, &config.model, "", &ctx, config).await?;

      let analysis_body = message_analysis.body_texts();

      let summary = crate::api::generate_summary_from_analysis(
         &stat,
         group.commit_type.as_str(),
         group.scope.as_ref().map(|s| s.as_str()),
         &analysis_body,
         Some(&group.rationale),
         config,
         args.debug_output.as_deref(),
         Some(&debug_prefix),
      )
      .await?;

      let final_commit_type = if dependency_only {
         CommitType::new("build")?
      } else {
         message_analysis.commit_type
      };

      let mut commit = ConventionalCommit {
         commit_type: final_commit_type,
         scope: message_analysis.scope,
         summary,
         body: analysis_body,
         footers: vec![],
      };

      post_process_commit_message(&mut commit, config);

      if let Err(e) = validate_commit_message(&commit, config) {
         eprintln!(
            "  {}",
            style::warning(&format!("{} Warning: Validation failed: {e}", style::icons::WARNING))
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

      // Create commit (unless preview mode)
      if !args.compose_preview {
         let sign = args.sign || config.gpg_sign;
         let signoff = args.signoff || config.signoff;
         git_commit(&formatted_message, false, dir, sign, signoff, args.skip_hooks, false)?;
         let hash = get_head_hash(dir)?;
         commit_hashes.push(hash);

         // Run tests if requested
         if args.compose_test_after_each {
            println!("  {}", style::info("Running tests..."));
            let test_result = std::process::Command::new("cargo")
               .arg("test")
               .current_dir(dir)
               .status();

            if let Ok(status) = test_result {
               if !status.success() {
                  return Err(CommitGenError::Other(format!(
                     "Tests failed after commit {idx}. Aborting."
                  )));
               }
               println!("  {}", style::success(&format!("{} Tests passed", style::icons::SUCCESS)));
            }
         }
      }
   }

   Ok(commit_hashes)
}

/// Main entry point for compose mode
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

      // Check if there are remaining changes
      if args.compose_preview {
         break;
      }

      let remaining_diff_output = std::process::Command::new("git")
         .args(["diff", "HEAD"])
         .current_dir(&args.dir)
         .output()
         .map_err(|e| CommitGenError::git(format!("Failed to check remaining diff: {e}")))?;

      if !remaining_diff_output.status.success() {
         continue;
      }

      let remaining_diff = String::from_utf8_lossy(&remaining_diff_output.stdout);
      if remaining_diff.trim().is_empty() {
         println!(
            "\n{}",
            style::success(&format!(
               "{} All changes committed successfully",
               style::icons::SUCCESS
            ))
         );
         break;
      }

      eprintln!(
         "\n{}",
         style::warning(&format!(
            "{} Uncommitted changes remain after round {round}",
            style::icons::WARNING
         ))
      );

      let stat_output = std::process::Command::new("git")
         .args(["diff", "HEAD", "--stat"])
         .current_dir(&args.dir)
         .output()
         .ok();

      if let Some(output) = stat_output
         && output.status.success()
      {
         let stat = String::from_utf8_lossy(&output.stdout);
         eprintln!("{stat}");
      }

      if round < max_rounds {
         eprintln!("{}", style::info("Starting another compose round..."));
         continue;
      }
      eprintln!(
         "{}",
         style::warning(&format!(
            "Reached max rounds ({max_rounds}). Remaining changes need manual commit."
         ))
      );
   }

   Ok(())
}

/// Run a single round of compose
async fn run_compose_round(args: &Args, config: &CommitConfig, round: usize) -> Result<()> {
   let token_counter = create_token_counter(config);

   // Get combined diff (staged + unstaged)
   let diff_staged = get_git_diff(&Mode::Staged, None, &args.dir, config).unwrap_or_default();
   let diff_unstaged = get_git_diff(&Mode::Unstaged, None, &args.dir, config).unwrap_or_default();

   let combined_diff = if diff_staged.is_empty() {
      diff_unstaged
   } else if diff_unstaged.is_empty() {
      diff_staged
   } else {
      format!("{diff_staged}\n{diff_unstaged}")
   };

   if combined_diff.is_empty() {
      return Err(CommitGenError::NoChanges { mode: "working directory".to_string() });
   }

   let stat_staged = get_git_stat(&Mode::Staged, None, &args.dir, config).unwrap_or_default();
   let stat_unstaged = get_git_stat(&Mode::Unstaged, None, &args.dir, config).unwrap_or_default();

   let combined_stat = if stat_staged.is_empty() {
      stat_unstaged
   } else if stat_unstaged.is_empty() {
      stat_staged
   } else {
      format!("{stat_staged}\n{stat_unstaged}")
   };

   // Save original diff for validation (before possible truncation)
   let original_diff = combined_diff.clone();

   // Truncate if needed
   let diff = if combined_diff.len() > config.max_diff_length {
      println!(
         "{}",
         style::warning(&format!(
            "{} Applying smart truncation (diff size: {} characters)",
            style::icons::WARNING,
            combined_diff.len()
         ))
      );
      smart_truncate_diff(&combined_diff, config.max_diff_length, config, &token_counter)
   } else {
      combined_diff
   };

   let max_commits = args.compose_max_commits.unwrap_or(3);

   println!("{}", style::info(&format!("Analyzing changes (max {max_commits} commits)...")));
   let analysis = analyze_for_compose(&diff, &combined_stat, config, max_commits).await?;

   // Validate groups for exhaustiveness and correctness
   println!("{}", style::info("Validating groups..."));
   validate_compose_groups(&analysis.groups, &original_diff)?;

   println!("\n{}", style::section_header("Proposed Commit Groups", 80));
   for (idx, &group_idx) in analysis.dependency_order.iter().enumerate() {
      let mut group = analysis.groups[group_idx].clone();
      if group_affects_only_dependency_files(&group) && group.commit_type.as_str() != "build" {
         group.commit_type = CommitType::new("build")?;
      }
      println!(
         "\n{}. [{}{}] {}",
         idx + 1,
         style::commit_type(&group.commit_type.to_string()),
         group
            .scope
            .as_ref()
            .map(|s| format!("({})", style::scope(&s.to_string())))
            .unwrap_or_default(),
         group.rationale
      );
      println!("   Changes:");
      for change in &group.changes {
         let is_all =
            change.hunks.len() == 1 && matches!(&change.hunks[0], crate::types::HunkSelector::All);

         if is_all {
            println!("     - {} (all changes)", change.path);
         } else {
            // Display summary of selectors
            let summary: Vec<String> = change
               .hunks
               .iter()
               .map(|s| match s {
                  crate::types::HunkSelector::All => "all".to_string(),
                  crate::types::HunkSelector::Lines { start, end } => {
                     format!("lines {start}-{end}")
                  },
                  crate::types::HunkSelector::Search { pattern } => {
                     if pattern.len() > 20 {
                        format!("search '{}'...", &pattern[..20])
                     } else {
                        format!("search '{pattern}'")
                     }
                  },
               })
               .collect();
            println!("     - {} ({})", change.path, summary.join(", "));
         }
      }
      if !group.dependencies.is_empty() {
         println!("   Depends on: {:?}", group.dependencies);
      }
   }

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
   let hashes = execute_compose(&analysis, config, args).await?;

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
