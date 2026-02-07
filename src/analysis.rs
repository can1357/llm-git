use std::{
   collections::{HashMap, HashSet},
   process::Command,
};

/// Scope analysis functionality for git diff numstat parsing
use crate::config::CommitConfig;
use crate::{
   error::{CommitGenError, Result},
   types::{Mode, ScopeCandidate},
};

/// Placeholder dirs to skip when building two-segment scopes
/// These are organizational directories that rarely represent meaningful scopes
const PLACEHOLDER_DIRS: &[&str] = &[
   // Rust conventions
   "src", "lib", "bin", "crates", "benches", "examples", // Go conventions
   "internal", "pkg",     // C/C++ conventions
   "include", // Testing
   "tests", "test", // Documentation
   "docs", // Generic organizational
   "packages", "modules",
];

/// Directories to skip entirely when extracting scopes
const SKIP_DIRS: &[&str] =
   &["test", "tests", "benches", "examples", "target", "build", "node_modules", ".github"];

pub struct ScopeAnalyzer {
   component_lines: HashMap<String, usize>,
   total_lines:     usize,
}

impl Default for ScopeAnalyzer {
   fn default() -> Self {
      Self::new()
   }
}

impl ScopeAnalyzer {
   pub fn new() -> Self {
      Self { component_lines: HashMap::new(), total_lines: 0 }
   }

   /// Process single numstat line: "added\tdeleted\tpath"
   pub fn process_numstat_line(&mut self, line: &str, config: &CommitConfig) {
      let parts: Vec<&str> = line.split('\t').collect();
      if parts.len() < 3 {
         return;
      }

      let (added_str, deleted_str, path_part) = (parts[0], parts[1], parts[2]);

      // Parse line counts (skip binary files marked with "-")
      let added = added_str.parse::<usize>().unwrap_or(0);
      let deleted = deleted_str.parse::<usize>().unwrap_or(0);
      let lines_changed = added + deleted;

      if lines_changed == 0 {
         return;
      }

      // Extract actual path from rename syntax
      let path = Self::extract_path_from_rename(path_part);

      // Skip excluded files
      if config.excluded_files.iter().any(|ex| path.ends_with(ex)) {
         return;
      }

      self.total_lines += lines_changed;

      // Extract component candidates from path
      let component_candidates = Self::extract_components_from_path(&path);

      for comp in component_candidates {
         // Final sanity check: no segments should contain dots
         if comp.split('/').any(|s| s.contains('.')) {
            continue;
         }

         *self.component_lines.entry(comp).or_insert(0) += lines_changed;
      }
   }

   /// Extract new path from rename syntax (handles both brace and arrow forms)
   fn extract_path_from_rename(path_part: &str) -> String {
      // Handle renames with brace syntax: "lib/wal/{io_worker.rs => io.rs}"
      if let Some(brace_start) = path_part.find('{') {
         if let Some(arrow_pos) = path_part[brace_start..].find(" => ") {
            let arrow_abs = brace_start + arrow_pos;
            if let Some(brace_end) = path_part[arrow_abs..].find('}') {
               let brace_end_abs = arrow_abs + brace_end;
               let prefix = &path_part[..brace_start];
               let new_name = path_part[arrow_abs + 4..brace_end_abs].trim();
               return format!("{prefix}{new_name}");
            }
         }
      } else if path_part.contains(" => ") {
         // Simple arrow syntax: "old/path => new/path"
         return path_part
            .split(" => ")
            .nth(1)
            .unwrap_or(path_part)
            .trim()
            .to_string();
      }

      path_part.trim().to_string()
   }

   /// Extract meaningful component paths from file path
   fn extract_components_from_path(path: &str) -> Vec<String> {
      let segments: Vec<&str> = path.split('/').collect();
      let mut component_candidates = Vec::new();
      let mut meaningful_segments = Vec::new();

      // Helper: strip extension from segment
      let strip_ext = |s: &str| -> String {
         if let Some(pos) = s.rfind('.') {
            s[..pos].to_string()
         } else {
            s.to_string()
         }
      };

      // Helper: is this segment a file (contains extension)?
      let is_file = |s: &str| -> bool {
         s.contains('.') && !s.starts_with('.') && s.rfind('.').is_some_and(|p| p > 0)
      };

      // Build candidates by walking path and extracting meaningful directory segments
      for (seg_idx, seg) in segments.iter().enumerate() {
         // Skip placeholder dirs when any deeper segments exist
         if PLACEHOLDER_DIRS.contains(seg) {
            // If this is a placeholder and we have more segments after it, skip it
            if segments.len() > seg_idx + 1 {
               continue;
            }
         }
         // Skip if it's a file (has extension)
         if is_file(seg) {
            continue;
         }
         // Skip common non-scope dirs
         if SKIP_DIRS.contains(seg) {
            continue;
         }

         let stripped = strip_ext(seg);
         // Filter out empty segments or dotfiles
         if !stripped.is_empty() && !stripped.starts_with('.') {
            meaningful_segments.push(stripped);
         }
      }

      // Generate candidates: single-level and two-level
      if !meaningful_segments.is_empty() {
         component_candidates.push(meaningful_segments[0].clone());

         if meaningful_segments.len() >= 2 {
            component_candidates
               .push(format!("{}/{}", meaningful_segments[0], meaningful_segments[1]));
         }
      }

      component_candidates
   }

   /// Build sorted `ScopeCandidate` list from accumulated data
   pub fn build_scope_candidates(&self) -> Vec<ScopeCandidate> {
      let mut candidates: Vec<ScopeCandidate> = self
         .component_lines
         .iter()
         .filter(|(path, _)| {
            // Filter out pure placeholder single-segment scopes
            if !path.contains('/') && PLACEHOLDER_DIRS.contains(&path.as_str()) {
               return false;
            }
            // Filter out scopes starting with placeholder dirs
            if let Some(root) = path.split('/').next()
               && PLACEHOLDER_DIRS.contains(&root)
            {
               return false;
            }
            true
         })
         .map(|(path, &lines)| {
            let percentage = (lines as f32 / self.total_lines as f32) * 100.0;
            let is_two_segment = path.contains('/');

            // Confidence calculation:
            // - Single-segment: percentage as-is
            // - Two-segment: percentage * 1.2 if >60%, else * 0.8
            let confidence = if is_two_segment {
               if percentage > 60.0 {
                  percentage * 1.2
               } else {
                  percentage * 0.8
               }
            } else {
               percentage
            };

            ScopeCandidate { percentage, path: path.clone(), confidence }
         })
         .collect();

      candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap());
      candidates
   }

   /// Check if change spans multiple components (wide change)
   pub fn is_wide_change(candidates: &[ScopeCandidate], config: &CommitConfig) -> bool {
      // Check if top component is below threshold
      let is_wide = if let Some(top) = candidates.first() {
         top.percentage / 100.0 < config.wide_change_threshold
      } else {
         false
      };

      // Check if ≥3 distinct roots
      let distinct_roots: HashSet<&str> = candidates
         .iter()
         .map(|c| c.path.split('/').next().unwrap_or(&c.path))
         .collect();

      is_wide || distinct_roots.len() >= 3
   }

   /// Public API: extract scope candidates from git numstat output
   pub fn extract_scope(numstat: &str, config: &CommitConfig) -> (Vec<ScopeCandidate>, usize) {
      let mut analyzer = Self::new();

      for line in numstat.lines() {
         analyzer.process_numstat_line(line, config);
      }

      let candidates = analyzer.build_scope_candidates();
      (candidates, analyzer.total_lines)
   }

   /// Analyze wide changes to detect cross-cutting patterns
   pub fn analyze_wide_change(numstat: &str) -> Option<String> {
      let lines: Vec<&str> = numstat.lines().collect();
      if lines.is_empty() {
         return None;
      }

      // Extract file paths from numstat
      let paths: Vec<&str> = lines
         .iter()
         .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 3 {
               Some(parts[2])
            } else {
               None
            }
         })
         .collect();

      if paths.is_empty() {
         return None;
      }

      // Count file types
      let total = paths.len();
      let mut md_count = 0;
      let mut test_count = 0;
      let mut config_count = 0;
      let mut has_cargo_toml = false;
      let mut has_package_json = false;

      // Track patterns
      let mut error_keywords = 0;
      let mut type_keywords = 0;

      for path in &paths {
         // File extension analysis
         if std::path::Path::new(path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
         {
            md_count += 1;
         }
         if path.contains("/test") || path.contains("_test.") || path.ends_with("_test.rs") {
            test_count += 1;
         }
         if std::path::Path::new(path).extension().is_some_and(|ext| {
            ext.eq_ignore_ascii_case("toml")
               || ext.eq_ignore_ascii_case("yaml")
               || ext.eq_ignore_ascii_case("yml")
               || ext.eq_ignore_ascii_case("json")
         }) {
            config_count += 1;
         }

         // Dependency files
         if path.contains("Cargo.toml") {
            has_cargo_toml = true;
         }
         if path.contains("package.json") {
            has_package_json = true;
         }

         // Pattern keywords in paths
         let lower_path = path.to_lowercase();
         if lower_path.contains("error")
            || lower_path.contains("result")
            || lower_path.contains("err")
         {
            error_keywords += 1;
         }
         if lower_path.contains("type")
            || lower_path.contains("struct")
            || lower_path.contains("enum")
         {
            type_keywords += 1;
         }
      }

      // Detection heuristics (ordered by specificity)

      // 1. Dependency updates (high confidence)
      if has_cargo_toml || has_package_json {
         return Some("deps".to_string());
      }

      // 2. Documentation updates (>70% .md files)
      if md_count * 100 / total > 70 {
         return Some("docs".to_string());
      }

      // 3. Test updates (>60% test files)
      if test_count * 100 / total > 60 {
         return Some("tests".to_string());
      }

      // 4. Error handling migration (>40% files with error keywords)
      if error_keywords * 100 / total > 40 {
         return Some("error-handling".to_string());
      }

      // 5. Type migration (>40% files with type keywords)
      if type_keywords * 100 / total > 40 {
         return Some("type-refactor".to_string());
      }

      // 6. Config/tooling updates (>50% config files)
      if config_count * 100 / total > 50 {
         return Some("config".to_string());
      }

      // No clear pattern detected
      None
   }
}

/// Extract candidate scopes from git diff --numstat output
/// Returns (`scope_string`, `is_wide_change`)
pub fn extract_scope_candidates(
   mode: &Mode,
   target: Option<&str>,
   dir: &str,
   config: &CommitConfig,
) -> Result<(String, bool)> {
   // Get numstat output
   let output = match mode {
      Mode::Staged => Command::new("git")
         .args(["diff", "--cached", "--numstat"])
         .current_dir(dir)
         .output()
         .map_err(|e| {
            CommitGenError::git(format!("Failed to run git diff --cached --numstat: {e}"))
         })?,
      Mode::Commit => {
         let target = target.ok_or_else(|| {
            CommitGenError::ValidationError("--target required for commit mode".to_string())
         })?;
         Command::new("git")
            .args(["show", "--numstat", target])
            .current_dir(dir)
            .output()
            .map_err(|e| {
               CommitGenError::git(format!("Failed to run git show --numstat: {e}"))
            })?
      },
      Mode::Unstaged => Command::new("git")
         .args(["diff", "--numstat"])
         .current_dir(dir)
         .output()
         .map_err(|e| CommitGenError::git(format!("Failed to run git diff --numstat: {e}")))?,
      Mode::Compose => unreachable!("compose mode handled separately"),
   };

   if !output.status.success() {
      return Err(CommitGenError::git("git diff --numstat failed".to_string()));
   }

   let numstat = String::from_utf8_lossy(&output.stdout);

   let (candidates, total_lines) = ScopeAnalyzer::extract_scope(&numstat, config);

   if total_lines == 0 {
      return Ok(("(none - no measurable changes)".to_string(), false));
   }

   let is_wide = ScopeAnalyzer::is_wide_change(&candidates, config);

   if is_wide {
      // Try to detect a pattern if wide_change_abstract is enabled
      let scope_str = if config.wide_change_abstract {
         if let Some(pattern) = ScopeAnalyzer::analyze_wide_change(&numstat) {
            format!("(cross-cutting: {pattern})")
         } else {
            "(none - multi-component change)".to_string()
         }
      } else {
         "(none - multi-component change)".to_string()
      };

      return Ok((scope_str, true));
   }

   // Format suggested scopes with weights for prompt (keep top 5, prefer 2-segment
   // when >60%)
   let mut suggestion_parts = Vec::new();
   for cand in candidates.iter().take(5) {
      // Only suggest if ≥10% to avoid noise
      if cand.percentage >= 10.0 {
         let confidence_label = if cand.path.contains('/') {
            if cand.percentage > 60.0 {
               "high confidence"
            } else {
               "moderate confidence"
            }
         } else {
            "high confidence"
         };

         suggestion_parts
            .push(format!("{} ({:.0}%, {})", cand.path, cand.percentage, confidence_label));
      }
   }

   let scope_str = if suggestion_parts.is_empty() {
      "(none - unclear component)".to_string()
   } else {
      format!("{}\nPrefer 2-segment scopes marked 'high confidence'", suggestion_parts.join(", "))
   };

   Ok((scope_str, is_wide))
}

#[cfg(test)]
mod tests {
   use super::*;

   fn default_config() -> CommitConfig {
      CommitConfig {
         excluded_files: vec![
            "Cargo.lock".to_string(),
            "package-lock.json".to_string(),
            "yarn.lock".to_string(),
         ],
         wide_change_threshold: 0.5,
         ..Default::default()
      }
   }

   // Tests for extract_path_from_rename()
   #[test]
   fn test_extract_path_from_rename_brace() {
      // Brace syntax replaces only the content within braces (suffix is not
      // preserved)
      assert_eq!(ScopeAnalyzer::extract_path_from_rename("lib/{old => new}/file.rs"), "lib/new");
   }

   #[test]
   fn test_extract_path_from_rename_brace_complex() {
      assert_eq!(
         ScopeAnalyzer::extract_path_from_rename("src/api/{client.rs => http_client.rs}"),
         "src/api/http_client.rs"
      );
   }

   #[test]
   fn test_extract_path_from_rename_arrow() {
      assert_eq!(
         ScopeAnalyzer::extract_path_from_rename("old/file.rs => new/file.rs"),
         "new/file.rs"
      );
   }

   #[test]
   fn test_extract_path_from_rename_arrow_with_spaces() {
      assert_eq!(
         ScopeAnalyzer::extract_path_from_rename("  old/path.rs => new/path.rs  "),
         "new/path.rs"
      );
   }

   #[test]
   fn test_extract_path_from_rename_no_rename() {
      assert_eq!(ScopeAnalyzer::extract_path_from_rename("lib/file.rs"), "lib/file.rs");
   }

   #[test]
   fn test_extract_path_from_rename_malformed_brace() {
      // Missing closing brace - falls back to original
      assert_eq!(
         ScopeAnalyzer::extract_path_from_rename("lib/{old => new/file.rs"),
         "lib/{old => new/file.rs"
      );
   }

   // Tests for extract_components_from_path()
   #[test]
   fn test_extract_components_simple() {
      // "src" is placeholder and skipped, only "api" remains
      let comps = ScopeAnalyzer::extract_components_from_path("src/api/client.rs");
      assert_eq!(comps, vec!["api"]);
   }

   #[test]
   fn test_extract_components_with_placeholder() {
      // "lib" is placeholder and skipped, "foo" and "bar" remain
      let comps = ScopeAnalyzer::extract_components_from_path("lib/foo/bar/baz.tsx");
      assert_eq!(comps, vec!["foo", "foo/bar"]);
   }

   #[test]
   fn test_extract_components_skip_tests() {
      // "tests" is in SKIP_DIRS, so skipped, only "api" remains
      let comps = ScopeAnalyzer::extract_components_from_path("tests/api/client_test.rs");
      assert_eq!(comps, vec!["api"]);
   }

   #[test]
   fn test_extract_components_skip_node_modules() {
      // "node_modules" is in SKIP_DIRS, only "foo" remains
      let comps = ScopeAnalyzer::extract_components_from_path("node_modules/foo/bar.js");
      assert_eq!(comps, vec!["foo"]);
   }

   #[test]
   fn test_extract_components_single_segment() {
      let comps = ScopeAnalyzer::extract_components_from_path("src/main.rs");
      // "src" is a placeholder and is stripped, leaving no components
      assert_eq!(comps, Vec::<String>::new());
   }

   #[test]
   fn test_extract_components_dotfile_skipped() {
      // ".git" gets stripped to "" and filtered out, "config" is kept
      let comps = ScopeAnalyzer::extract_components_from_path("lib/.git/config");
      assert_eq!(comps, vec!["config"]);
   }

   #[test]
   fn test_extract_components_strips_extension() {
      let comps = ScopeAnalyzer::extract_components_from_path("src/api/client.rs");
      // "client.rs" is a file, so skipped; "api" and "src" are dirs
      assert!(comps.contains(&"api".to_string()));
   }

   #[test]
   fn test_extract_components_go_internal() {
      // Go projects: internal/ is placeholder, extract actual module
      let comps = ScopeAnalyzer::extract_components_from_path("internal/agent/worker.go");
      assert_eq!(comps, vec!["agent"]);
   }

   #[test]
   fn test_extract_components_go_internal_nested() {
      // Go projects: internal/foo/bar/baz.go → extract "foo" and "foo/bar"
      let comps = ScopeAnalyzer::extract_components_from_path("internal/config/parser/json.go");
      assert_eq!(comps, vec!["config", "config/parser"]);
   }

   #[test]
   fn test_extract_components_go_pkg() {
      // Go projects: pkg/ is also a placeholder
      let comps = ScopeAnalyzer::extract_components_from_path("pkg/util/strings.go");
      assert_eq!(comps, vec!["util"]);
   }

   #[test]
   fn test_extract_components_monorepo_packages() {
      // Monorepos: packages/ is placeholder
      let comps = ScopeAnalyzer::extract_components_from_path("packages/core/index.ts");
      assert_eq!(comps, vec!["core"]);
   }

   // Tests for process_numstat_line()
   #[test]
   fn test_process_numstat_line_normal() {
      let mut analyzer = ScopeAnalyzer::new();
      let config = default_config();
      analyzer.process_numstat_line("10\t5\tlib/foo/bar.rs", &config);

      assert_eq!(analyzer.total_lines, 15);
      assert_eq!(analyzer.component_lines.get("foo"), Some(&15));
   }

   #[test]
   fn test_process_numstat_line_excluded_file() {
      let mut analyzer = ScopeAnalyzer::new();
      let config = default_config();
      analyzer.process_numstat_line("10\t5\tCargo.lock", &config);

      assert_eq!(analyzer.total_lines, 0);
      assert!(analyzer.component_lines.is_empty());
   }

   #[test]
   fn test_process_numstat_line_binary_file() {
      let mut analyzer = ScopeAnalyzer::new();
      let config = default_config();
      analyzer.process_numstat_line("-\t-\timage.png", &config);

      assert_eq!(analyzer.total_lines, 0);
   }

   #[test]
   fn test_process_numstat_line_invalid() {
      let mut analyzer = ScopeAnalyzer::new();
      let config = default_config();
      analyzer.process_numstat_line("invalid line", &config);

      assert_eq!(analyzer.total_lines, 0);
   }

   #[test]
   fn test_process_numstat_line_rename_brace() {
      let mut analyzer = ScopeAnalyzer::new();
      let config = default_config();
      // Brace syntax gives "lib/new" path
      analyzer.process_numstat_line("20\t10\tlib/{old => new}/file.rs", &config);

      assert_eq!(analyzer.total_lines, 30);
      // Path "lib/new/file.rs" -> extracts "new" (lib is stripped as placeholder)
      assert_eq!(analyzer.component_lines.get("new"), Some(&30));
   }

   #[test]
   fn test_process_numstat_line_multiple_files() {
      let mut analyzer = ScopeAnalyzer::new();
      let config = default_config();
      analyzer.process_numstat_line("10\t5\tsrc/api/client.rs", &config);
      analyzer.process_numstat_line("20\t10\tsrc/api/server.rs", &config);

      assert_eq!(analyzer.total_lines, 45);
      assert_eq!(analyzer.component_lines.get("api"), Some(&45));
   }

   // Tests for is_wide_change()
   #[test]
   fn test_is_wide_change_focused() {
      let config = default_config();
      let candidates = vec![
         ScopeCandidate { path: "api".to_string(), percentage: 80.0, confidence: 80.0 },
         ScopeCandidate { path: "db".to_string(), percentage: 20.0, confidence: 20.0 },
      ];

      assert!(!ScopeAnalyzer::is_wide_change(&candidates, &config));
   }

   #[test]
   fn test_is_wide_change_dispersed() {
      let config = default_config();
      let candidates = vec![
         ScopeCandidate { path: "api".to_string(), percentage: 30.0, confidence: 30.0 },
         ScopeCandidate { path: "db".to_string(), percentage: 30.0, confidence: 30.0 },
         ScopeCandidate { path: "ui".to_string(), percentage: 40.0, confidence: 40.0 },
      ];

      assert!(ScopeAnalyzer::is_wide_change(&candidates, &config));
   }

   #[test]
   fn test_is_wide_change_three_roots() {
      let config = default_config();
      let candidates = vec![
         ScopeCandidate { path: "api".to_string(), percentage: 60.0, confidence: 60.0 },
         ScopeCandidate { path: "db".to_string(), percentage: 20.0, confidence: 20.0 },
         ScopeCandidate { path: "ui".to_string(), percentage: 20.0, confidence: 20.0 },
      ];

      assert!(ScopeAnalyzer::is_wide_change(&candidates, &config));
   }

   #[test]
   fn test_is_wide_change_nested_same_root() {
      let config = default_config();
      let candidates = vec![
         ScopeCandidate {
            path:       "api/client".to_string(),
            percentage: 60.0,
            confidence: 72.0,
         },
         ScopeCandidate {
            path:       "api/server".to_string(),
            percentage: 40.0,
            confidence: 32.0,
         },
      ];

      assert!(!ScopeAnalyzer::is_wide_change(&candidates, &config));
   }

   #[test]
   fn test_is_wide_change_empty() {
      let config = default_config();
      let candidates = vec![];

      assert!(!ScopeAnalyzer::is_wide_change(&candidates, &config));
   }

   // Integration tests for extract_scope()
   #[test]
   fn test_extract_scope_single_file() {
      let config = default_config();
      let numstat = "10\t5\tsrc/api/client.rs";
      let (candidates, total_lines) = ScopeAnalyzer::extract_scope(numstat, &config);

      assert_eq!(total_lines, 15);
      // "src" is filtered out, only "api" remains
      assert_eq!(candidates.len(), 1);
      assert_eq!(candidates[0].path, "api");
      assert_eq!(candidates[0].percentage, 100.0);
   }

   #[test]
   fn test_extract_scope_placeholder_only() {
      let config = default_config();
      let numstat = "10\t5\tsrc/main.rs";
      let (candidates, total_lines) = ScopeAnalyzer::extract_scope(numstat, &config);

      assert_eq!(total_lines, 15);
      // "src" is placeholder and filtered out, no candidates
      assert_eq!(candidates.len(), 0);
   }

   #[test]
   fn test_extract_scope_multiple_files() {
      let config = default_config();
      let numstat = "10\t5\tsrc/api/client.rs\n20\t10\tsrc/db/models.rs";
      let (candidates, total_lines) = ScopeAnalyzer::extract_scope(numstat, &config);

      assert_eq!(total_lines, 45);
      assert!(candidates.len() >= 2);

      // Check that both components are present
      let api_cand = candidates.iter().find(|c| c.path == "api");
      let db_cand = candidates.iter().find(|c| c.path == "db");

      assert!(api_cand.is_some());
      assert!(db_cand.is_some());

      // DB should have higher percentage (30 lines vs 15)
      assert!(db_cand.unwrap().percentage > api_cand.unwrap().percentage);
   }

   #[test]
   fn test_extract_scope_excluded_files() {
      let config = default_config();
      let numstat = "100\t50\tCargo.lock\n10\t5\tsrc/api/client.rs";
      let (candidates, total_lines) = ScopeAnalyzer::extract_scope(numstat, &config);

      // Cargo.lock should be excluded
      assert_eq!(total_lines, 15);
      assert_eq!(candidates[0].path, "api");
   }

   #[test]
   fn test_extract_scope_no_changes() {
      let config = default_config();
      let numstat = "";
      let (candidates, total_lines) = ScopeAnalyzer::extract_scope(numstat, &config);

      assert_eq!(total_lines, 0);
      assert!(candidates.is_empty());
   }

   #[test]
   fn test_extract_scope_sorted_by_percentage() {
      let config = default_config();
      let numstat = "5\t0\tsrc/api/client.rs\n50\t0\tsrc/db/models.rs\n10\t0\tsrc/ui/component.tsx";
      let (candidates, _) = ScopeAnalyzer::extract_scope(numstat, &config);

      // Should be sorted descending by percentage
      assert!(candidates[0].percentage >= candidates[1].percentage);
      assert!(candidates[1].percentage >= candidates[2].percentage);
   }

   #[test]
   fn test_build_scope_candidates_percentages() {
      let mut analyzer = ScopeAnalyzer::new();
      analyzer.component_lines.insert("api".to_string(), 30);
      analyzer.component_lines.insert("db".to_string(), 70);
      analyzer.total_lines = 100;

      let candidates = analyzer.build_scope_candidates();

      assert_eq!(candidates.len(), 2);
      assert_eq!(candidates[0].path, "db");
      assert!((candidates[0].percentage - 70.0).abs() < 0.001);
      assert_eq!(candidates[1].path, "api");
      assert!((candidates[1].percentage - 30.0).abs() < 0.001);
   }

   // Confidence heuristic tests: 70% in two-segment should prefer specific scope
   #[test]
   fn test_confidence_70_percent_in_two_segment_prefers_specific() {
      let mut analyzer = ScopeAnalyzer::new();
      analyzer.component_lines.insert("api".to_string(), 70);
      analyzer
         .component_lines
         .insert("api/client".to_string(), 70);
      analyzer.component_lines.insert("other".to_string(), 30);
      analyzer.total_lines = 100;

      let candidates = analyzer.build_scope_candidates();

      // api/client at 70% gets confidence = 70 * 1.2 = 84
      // api at 70% gets confidence = 70
      // other at 30% gets confidence = 30
      // So api/client should be first
      assert_eq!(candidates[0].path, "api/client");
      assert!((candidates[0].percentage - 70.0).abs() < 0.001);
      assert!((candidates[0].confidence - 84.0).abs() < 0.001);
   }

   // Confidence heuristic tests: 45% in two-segment should prefer single-segment
   #[test]
   fn test_confidence_45_percent_in_two_segment_prefers_single() {
      let mut analyzer = ScopeAnalyzer::new();
      analyzer.component_lines.insert("api".to_string(), 45);
      analyzer
         .component_lines
         .insert("api/client".to_string(), 45);
      analyzer.component_lines.insert("other".to_string(), 55);
      analyzer.total_lines = 100;

      let candidates = analyzer.build_scope_candidates();

      // other at 55% gets confidence = 55
      // api at 45% gets confidence = 45
      // api/client at 45% gets confidence = 45 * 0.8 = 36
      // So order should be: other, api, api/client
      assert_eq!(candidates[0].path, "other");
      assert_eq!(candidates[1].path, "api");
      assert_eq!(candidates[2].path, "api/client");
      assert!((candidates[2].confidence - 36.0).abs() < 0.001);
   }

   // Tests for analyze_wide_change()
   #[test]
   fn test_analyze_wide_change_dependency_updates() {
      let numstat = "10\t5\tCargo.toml\n20\t10\tsrc/lib.rs\n5\t3\tsrc/api.rs";
      let result = ScopeAnalyzer::analyze_wide_change(numstat);
      assert_eq!(result, Some("deps".to_string()));
   }

   #[test]
   fn test_analyze_wide_change_documentation() {
      let numstat =
         "50\t20\tREADME.md\n30\t10\tdocs/guide.md\n20\t5\tdocs/api.md\n5\t2\tsrc/lib.rs";
      let result = ScopeAnalyzer::analyze_wide_change(numstat);
      assert_eq!(result, Some("docs".to_string()));
   }

   #[test]
   fn test_analyze_wide_change_tests() {
      let numstat = "10\t5\tsrc/api_test.rs\n15\t8\tsrc/client_test.rs\n20\t10\ttests/\
                     integration_test.rs\n5\t2\tsrc/lib.rs";
      let result = ScopeAnalyzer::analyze_wide_change(numstat);
      assert_eq!(result, Some("tests".to_string()));
   }

   #[test]
   fn test_analyze_wide_change_error_handling() {
      let numstat =
         "10\t5\tsrc/error.rs\n15\t8\tsrc/result.rs\n20\t10\tsrc/error_types.rs\n5\t2\tsrc/lib.rs";
      let result = ScopeAnalyzer::analyze_wide_change(numstat);
      assert_eq!(result, Some("error-handling".to_string()));
   }

   #[test]
   fn test_analyze_wide_change_type_refactor() {
      let numstat =
         "10\t5\tsrc/types.rs\n15\t8\tsrc/structs.rs\n20\t10\tsrc/enums.rs\n5\t2\tsrc/lib.rs";
      let result = ScopeAnalyzer::analyze_wide_change(numstat);
      assert_eq!(result, Some("type-refactor".to_string()));
   }

   #[test]
   fn test_analyze_wide_change_config() {
      let numstat =
         "10\t5\tconfig.toml\n15\t8\tsettings.yaml\n20\t10\tconfig.json\n5\t2\tsrc/lib.rs";
      let result = ScopeAnalyzer::analyze_wide_change(numstat);
      assert_eq!(result, Some("config".to_string()));
   }

   #[test]
   fn test_analyze_wide_change_no_pattern() {
      let numstat = "10\t5\tsrc/foo.rs\n15\t8\tsrc/bar.rs\n20\t10\tsrc/baz.rs";
      let result = ScopeAnalyzer::analyze_wide_change(numstat);
      assert_eq!(result, None);
   }

   #[test]
   fn test_analyze_wide_change_empty() {
      let numstat = "";
      let result = ScopeAnalyzer::analyze_wide_change(numstat);
      assert_eq!(result, None);
   }

   #[test]
   fn test_analyze_wide_change_package_json() {
      let numstat = "10\t5\tpackage.json\n20\t10\tsrc/index.js\n5\t3\tsrc/utils.js";
      let result = ScopeAnalyzer::analyze_wide_change(numstat);
      assert_eq!(result, Some("deps".to_string()));
   }
}
