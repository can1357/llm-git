/// Diff parsing and smart truncation logic
use crate::{config::CommitConfig, tokens::TokenCounter};

#[derive(Debug, Clone)]
pub struct FileDiff {
   pub filename:  String,
   pub header:    String, // The diff header (@@, index, etc)
   pub content:   String, // The actual diff content
   pub additions: usize,
   pub deletions: usize,
   pub is_binary: bool,
}

impl FileDiff {
   pub const fn size(&self) -> usize {
      self.header.len() + self.content.len()
   }

   /// Estimate token count for this file diff.
   pub fn token_estimate(&self, counter: &TokenCounter) -> usize {
      // Use combined header + content for token estimate
      counter.count_sync(&self.header) + counter.count_sync(&self.content)
   }

   pub fn priority(&self, config: &CommitConfig) -> i32 {
      // Higher number = higher priority
      if self.is_binary {
         return -100; // Lowest priority
      }

      // Critical dependency manifests get medium-high priority despite extension
      let filename_lower = self.filename.to_lowercase();
      if filename_lower.ends_with("cargo.toml")
         || filename_lower.ends_with("package.json")
         || filename_lower.ends_with("go.mod")
         || filename_lower.ends_with("requirements.txt")
         || filename_lower.ends_with("pyproject.toml")
      {
         return 70; // Medium-high priority for dependency manifests (below source/SQL, above default)
      }

      // Check if it's a test file (lower priority)
      if self.filename.contains("/test")
         || self.filename.contains("test_")
         || self.filename.contains("_test.")
         || self.filename.contains(".test.")
      {
         return 10;
      }

      // Check file extension
      let ext = self.filename.rsplit('.').next().unwrap_or("");
      if config
         .low_priority_extensions
         .iter()
         .any(|e| e.trim_start_matches('.') == ext)
      {
         return 20;
      }

      // Source code files get highest priority
      match ext {
         "rs" | "go" | "py" | "js" | "ts" | "java" | "c" | "cpp" | "h" | "hpp" => 100,
         "sql" | "sh" | "bash" => 80,
         _ => 50,
      }
   }

   pub fn truncate(&mut self, max_size: usize) {
      if self.size() <= max_size {
         return;
      }

      // Keep the header, truncate content
      let available = max_size.saturating_sub(self.header.len() + 50); // Reserve space for truncation message

      if available < 50 {
         // Too small, just keep header
         self.content = "... (truncated)".to_string();
      } else {
         // Try to keep beginning and end of the diff
         let lines: Vec<&str> = self.content.lines().collect();
         if lines.len() > 30 {
            // Keep first 15 and last 10 lines to show both what was added/removed
            let keep_start = 15;
            let keep_end = 10;
            let omitted = lines.len() - keep_start - keep_end;
            // Pre-allocate capacity
            let est_size = keep_start * 60 + keep_end * 60 + 50;
            let mut truncated = String::with_capacity(est_size);
            for (i, line) in lines[..keep_start].iter().enumerate() {
               if i > 0 {
                  truncated.push('\n');
               }
               truncated.push_str(line);
            }
            use std::fmt::Write;
            write!(&mut truncated, "\n... (truncated {omitted} lines) ...\n").unwrap();
            for (i, line) in lines[lines.len() - keep_end..].iter().enumerate() {
               if i > 0 {
                  truncated.push('\n');
               }
               truncated.push_str(line);
            }
            self.content = truncated;
         } else {
            // Just truncate the content
            self.content.truncate(available);
            self.content.push_str("\n... (truncated)");
         }
      }
   }
}

/// Parse a git diff into individual file diffs
pub fn parse_diff(diff: &str) -> Vec<FileDiff> {
   let mut file_diffs = Vec::new();
   let mut current_file: Option<FileDiff> = None;
   let mut in_diff_header = false;

   for line in diff.lines() {
      if line.starts_with("diff --git") {
         // Save previous file if exists
         if let Some(file) = current_file.take() {
            file_diffs.push(file);
         }

         // Extract filename from diff line - avoid allocation until we know we need it
         let filename = line
            .split_whitespace()
            .nth(3)
            .map_or("unknown", |s| s.trim_start_matches("b/"))
            .to_string();

         current_file = Some(FileDiff {
            filename,
            header: String::from(line),
            content: String::new(),
            additions: 0,
            deletions: 0,
            is_binary: false,
         });
         in_diff_header = true;
      } else if let Some(file) = &mut current_file {
         if line.starts_with("Binary files") {
            file.is_binary = true;
            file.header.reserve(line.len() + 1);
            file.header.push('\n');
            file.header.push_str(line);
         } else if line.starts_with("index ")
            || line.starts_with("new file")
            || line.starts_with("deleted file")
            || line.starts_with("rename ")
            || line.starts_with("similarity index")
            || line.starts_with("+++")
            || line.starts_with("---")
         {
            // Part of the header
            file.header.reserve(line.len() + 1);
            file.header.push('\n');
            file.header.push_str(line);
         } else if line.starts_with("@@") {
            // Hunk header - marks end of file header, start of content
            in_diff_header = false;
            file.header.reserve(line.len() + 1);
            file.header.push('\n');
            file.header.push_str(line);
         } else if !in_diff_header {
            // Actual diff content
            if !file.content.is_empty() {
               file.content.push('\n');
            }
            file.content.push_str(line);

            if line.starts_with('+') && !line.starts_with("+++") {
               file.additions += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
               file.deletions += 1;
            }
         } else {
            // Still in header
            file.header.reserve(line.len() + 1);
            file.header.push('\n');
            file.header.push_str(line);
         }
      }
   }

   // Don't forget the last file
   if let Some(file) = current_file {
      file_diffs.push(file);
   }

   file_diffs
}

/// Smart truncation of git diff with token-aware budgeting
pub fn smart_truncate_diff(
   diff: &str,
   max_length: usize,
   config: &CommitConfig,
   counter: &TokenCounter,
) -> String {
   let mut file_diffs = parse_diff(diff);

   // Filter out excluded files
   file_diffs.retain(|f| {
      !config
         .excluded_files
         .iter()
         .any(|excluded| f.filename.ends_with(excluded))
   });

   if file_diffs.is_empty() {
      return "No relevant files to analyze (only lock files or excluded files were changed)"
         .to_string();
   }

   // Sort by priority (highest first)
   file_diffs.sort_by_key(|f| -f.priority(config));

   // Calculate total size and token estimate
   let total_size: usize = file_diffs.iter().map(|f| f.size()).sum();
   let total_tokens: usize = file_diffs.iter().map(|f| f.token_estimate(counter)).sum();

   // Use token budget if it's more restrictive than character budget
   // Estimate 4 chars per token for the size conversion
   let effective_max = if total_tokens > config.max_diff_tokens {
      // Convert token budget to approximate character budget
      config.max_diff_tokens * 4
   } else {
      max_length
   };

   if total_size <= effective_max {
      // Everything fits, reconstruct the diff
      return reconstruct_diff(&file_diffs);
   }

   // Strategy: Prioritize showing ALL file headers, even if we must truncate
   // content aggressively This ensures the LLM sees the full scope of changes
   let mut included_files = Vec::new();
   let mut current_size = 0;

   // First pass: include all files with minimal content to show the scope
   let header_only_size: usize = file_diffs.iter().map(|f| f.header.len() + 20).sum();
   let total_files = file_diffs.len();

   if header_only_size <= effective_max {
      // We can fit all headers, now distribute remaining space for content
      let remaining_space = effective_max - header_only_size;
      let space_per_file = if file_diffs.is_empty() {
         0
      } else {
         remaining_space / file_diffs.len()
      };

      included_files.reserve(file_diffs.len());
      for file in file_diffs {
         if file.is_binary {
            // Include binary files with just header
            included_files.push(FileDiff {
               filename:  file.filename,
               header:    file.header,
               content:   String::new(),
               additions: file.additions,
               deletions: file.deletions,
               is_binary: true,
            });
         } else {
            let mut truncated = file;
            let target_size = truncated.header.len() + space_per_file;
            if truncated.size() > target_size {
               truncated.truncate(target_size);
            }
            included_files.push(truncated);
         }
      }
   } else {
      // Even headers don't fit, fall back to including top priority files
      for mut file in file_diffs {
         if file.is_binary {
            continue; // Skip binary files when severely constrained
         }

         let file_size = file.size();
         if current_size + file_size <= effective_max {
            current_size += file_size;
            included_files.push(file);
         } else if current_size < effective_max / 2 && file.priority(config) >= 50 {
            // If we haven't used half the space and this is important, truncate and include
            // it
            let remaining = effective_max - current_size;
            file.truncate(remaining.saturating_sub(100)); // Leave some space
            included_files.push(file);
            break;
         }
      }
   }

   if included_files.is_empty() {
      return "Error: Could not include any files in the diff".to_string();
   }

   let mut result = reconstruct_diff(&included_files);

   // Add a note about excluded files if any
   let excluded_count = total_files - included_files.len();
   if excluded_count > 0 {
      use std::fmt::Write;
      write!(result, "\n\n... ({excluded_count} files omitted) ...").unwrap();
   }

   result
}

/// Reconstruct a diff from `FileDiff` objects
pub fn reconstruct_diff(files: &[FileDiff]) -> String {
   // Pre-allocate capacity based on file sizes
   let capacity: usize = files.iter().map(|f| f.size() + 1).sum();
   let mut result = String::with_capacity(capacity);

   for (i, file) in files.iter().enumerate() {
      if i > 0 {
         result.push('\n');
      }
      result.push_str(&file.header);
      if !file.content.is_empty() {
         result.push('\n');
         result.push_str(&file.content);
      }
   }

   result
}

/// Truncate a diff to fit within a line budget, distributing lines across files
/// by priority.
///
/// Unlike `smart_truncate_diff` which works on byte budgets, this operates on
/// line counts for simpler/faster context window management in fast mode.
pub fn truncate_diff_by_lines(diff: &str, max_lines: usize, config: &CommitConfig) -> String {
   let files = parse_diff(diff);

   // Count total content lines across all files
   let total_lines: usize = files
      .iter()
      .map(|f| f.header.lines().count() + f.content.lines().count())
      .sum();

   if total_lines <= max_lines {
      return diff.to_string();
   }

   // Calculate priority-weighted allocation
   let total_priority: i32 = files.iter().map(|f| f.priority(config).max(1)).sum();

   let mut result = String::with_capacity(diff.len());

   for file in &files {
      // Always include the header
      result.push_str(&file.header);
      if !file.header.ends_with('\n') {
         result.push('\n');
      }

      let content_lines: Vec<&str> = file.content.lines().collect();
      let priority = file.priority(config).max(1);

      // Allocate lines proportionally by priority
      #[allow(clippy::cast_sign_loss, reason = "priority and total are positive")]
      #[allow(clippy::cast_possible_truncation, reason = "line count fits in usize")]
      let allocated = ((max_lines as f64) * (priority as f64) / (total_priority as f64)) as usize;
      let allocated = allocated.max(5); // minimum 5 lines per file

      if content_lines.len() <= allocated {
         result.push_str(&file.content);
         if !file.content.ends_with('\n') {
            result.push('\n');
         }
      } else {
         // Keep first half and last half of allocation
         let keep_start = allocated / 2;
         let keep_end = allocated - keep_start;
         let omitted = content_lines.len() - keep_start - keep_end;

         for line in &content_lines[..keep_start] {
            result.push_str(line);
            result.push('\n');
         }

         use std::fmt::Write;
         writeln!(&mut result, "[... {omitted} lines omitted ...]")
            .expect("writing to String is infallible");

         for line in &content_lines[content_lines.len() - keep_end..] {
            result.push_str(line);
            result.push('\n');
         }
      }
   }

   result
}

#[cfg(test)]
mod tests {
   use super::*;

   fn test_config() -> CommitConfig {
      CommitConfig::default()
   }

   fn test_counter() -> TokenCounter {
      TokenCounter::new("http://localhost:4000", None, "claude-sonnet-4.5")
   }

   #[test]
   fn test_parse_diff_simple() {
      let diff = r#"diff --git a/src/main.rs b/src/main.rs
index 123..456 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
+use std::collections::HashMap;
 fn main() {
     println!("hello");
 }"#;
      let files = parse_diff(diff);
      assert_eq!(files.len(), 1);
      assert_eq!(files[0].filename, "src/main.rs");
      assert_eq!(files[0].additions, 1);
      assert_eq!(files[0].deletions, 0);
      assert!(!files[0].is_binary);
      assert!(files[0].header.contains("diff --git"));
      assert!(files[0].content.contains("use std::collections::HashMap"));
   }

   #[test]
   fn test_parse_diff_multi_file() {
      let diff = r"diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,3 @@
+pub mod utils;
 pub fn test() {}
diff --git a/src/main.rs b/src/main.rs
index 333..444 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,1 +1,2 @@
 fn main() {}
+fn helper() {}";
      let files = parse_diff(diff);
      assert_eq!(files.len(), 2);
      assert_eq!(files[0].filename, "src/lib.rs");
      assert_eq!(files[1].filename, "src/main.rs");
      assert_eq!(files[0].additions, 1);
      assert_eq!(files[1].additions, 1);
   }

   #[test]
   fn test_parse_diff_rename() {
      let diff = r"diff --git a/old.rs b/new.rs
similarity index 95%
rename from old.rs
rename to new.rs
index 123..456 100644
--- a/old.rs
+++ b/new.rs
@@ -1,2 +1,3 @@
 fn test() {}
+fn helper() {}";
      let files = parse_diff(diff);
      assert_eq!(files.len(), 1);
      assert_eq!(files[0].filename, "new.rs");
      assert!(files[0].header.contains("rename from"));
      assert!(files[0].header.contains("rename to"));
      assert_eq!(files[0].additions, 1);
   }

   #[test]
   fn test_parse_diff_binary() {
      let diff = r"diff --git a/image.png b/image.png
index 123..456 100644
Binary files a/image.png and b/image.png differ";
      let files = parse_diff(diff);
      assert_eq!(files.len(), 1);
      assert_eq!(files[0].filename, "image.png");
      assert!(files[0].is_binary);
      assert!(files[0].header.contains("Binary files"));
   }

   #[test]
   fn test_parse_diff_empty() {
      let diff = "";
      let files = parse_diff(diff);
      assert_eq!(files.len(), 0);
   }

   #[test]
   fn test_parse_diff_malformed_missing_hunks() {
      let diff = r"diff --git a/src/main.rs b/src/main.rs
index 123..456 100644
--- a/src/main.rs
+++ b/src/main.rs";
      let files = parse_diff(diff);
      assert_eq!(files.len(), 1);
      assert_eq!(files[0].filename, "src/main.rs");
      assert!(files[0].content.is_empty());
   }

   #[test]
   fn test_parse_diff_new_file() {
      let diff = r"diff --git a/new.rs b/new.rs
new file mode 100644
index 000..123 100644
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,2 @@
+fn test() {}
+fn main() {}";
      let files = parse_diff(diff);
      assert_eq!(files.len(), 1);
      assert_eq!(files[0].filename, "new.rs");
      assert!(files[0].header.contains("new file mode"));
      assert_eq!(files[0].additions, 2);
   }

   #[test]
   fn test_parse_diff_deleted_file() {
      let diff = r"diff --git a/old.rs b/old.rs
deleted file mode 100644
index 123..000 100644
--- a/old.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-fn test() {}
-fn main() {}";
      let files = parse_diff(diff);
      assert_eq!(files.len(), 1);
      assert_eq!(files[0].filename, "old.rs");
      assert!(files[0].header.contains("deleted file mode"));
      assert_eq!(files[0].deletions, 2);
   }

   #[test]
   fn test_file_diff_size() {
      let file = FileDiff {
         filename:  "test.rs".to_string(),
         header:    "header".to_string(),
         content:   "content".to_string(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      assert_eq!(file.size(), 6 + 7); // "header" + "content"
   }

   #[test]
   fn test_file_diff_priority_source_files() {
      let config = test_config();
      let rs_file = FileDiff {
         filename:  "src/main.rs".to_string(),
         header:    String::new(),
         content:   String::new(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      assert_eq!(rs_file.priority(&config), 100);

      let py_file = FileDiff {
         filename:  "script.py".to_string(),
         header:    String::new(),
         content:   String::new(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      assert_eq!(py_file.priority(&config), 100);

      let js_file = FileDiff {
         filename:  "app.js".to_string(),
         header:    String::new(),
         content:   String::new(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      assert_eq!(js_file.priority(&config), 100);
   }

   #[test]
   fn test_file_diff_priority_binary() {
      let config = test_config();
      let binary = FileDiff {
         filename:  "image.png".to_string(),
         header:    String::new(),
         content:   String::new(),
         additions: 0,
         deletions: 0,
         is_binary: true,
      };
      assert_eq!(binary.priority(&config), -100);
   }

   #[test]
   fn test_file_diff_priority_test_files() {
      let config = test_config();
      let test_file = FileDiff {
         filename:  "src/test_utils.rs".to_string(),
         header:    String::new(),
         content:   String::new(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      assert_eq!(test_file.priority(&config), 10);

      let test_dir = FileDiff {
         filename:  "tests/integration_test.rs".to_string(),
         header:    String::new(),
         content:   String::new(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      assert_eq!(test_dir.priority(&config), 10);
   }

   #[test]
   fn test_file_diff_priority_low_priority_extensions() {
      let config = test_config();
      let md_file = FileDiff {
         filename:  "README.md".to_string(),
         header:    String::new(),
         content:   String::new(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      assert_eq!(md_file.priority(&config), 20);

      let toml_file = FileDiff {
         filename:  "config.toml".to_string(),
         header:    String::new(),
         content:   String::new(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      assert_eq!(toml_file.priority(&config), 20);
   }

   #[test]
   fn test_file_diff_priority_dependency_manifests() {
      let config = test_config();

      let cargo_toml = FileDiff {
         filename:  "Cargo.toml".to_string(),
         header:    String::new(),
         content:   String::new(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      assert_eq!(cargo_toml.priority(&config), 70);

      let package_json = FileDiff {
         filename:  "package.json".to_string(),
         header:    String::new(),
         content:   String::new(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      assert_eq!(package_json.priority(&config), 70);

      let go_mod = FileDiff {
         filename:  "go.mod".to_string(),
         header:    String::new(),
         content:   String::new(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      assert_eq!(go_mod.priority(&config), 70);
   }

   #[test]
   fn test_file_diff_priority_default() {
      let config = test_config();
      let other = FileDiff {
         filename:  "data.csv".to_string(),
         header:    String::new(),
         content:   String::new(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      assert_eq!(other.priority(&config), 50);
   }

   #[test]
   fn test_file_diff_truncate_small() {
      let mut file = FileDiff {
         filename:  "test.rs".to_string(),
         header:    "header".to_string(),
         content:   "short content".to_string(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      let original_size = file.size();
      file.truncate(1000);
      assert_eq!(file.size(), original_size);
      assert_eq!(file.content, "short content");
   }

   #[test]
   fn test_file_diff_truncate_large() {
      let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
      let content = lines.join("\n");
      let mut file = FileDiff {
         filename: "test.rs".to_string(),
         header: "header".to_string(),
         content,
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      file.truncate(500);
      assert!(file.content.contains("... (truncated"));
      assert!(file.content.contains("line 0")); // First line preserved
      assert!(file.content.contains("line 99")); // Last line preserved
   }

   #[test]
   fn test_file_diff_truncate_preserves_context() {
      let lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
      let content = lines.join("\n");
      let original_lines = content.lines().count();
      let mut file = FileDiff {
         filename: "test.rs".to_string(),
         header: "header".to_string(),
         content,
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      // Use a size that will definitely trigger truncation
      file.truncate(300);
      // Should keep first 15 and last 10 lines
      assert!(file.content.contains("line 0"));
      assert!(file.content.contains("line 14"));
      assert!(file.content.contains("line 40"));
      assert!(file.content.contains("line 49"));
      // Check that truncation occurred and message is present
      let truncated_lines = file.content.lines().count();
      assert!(truncated_lines < original_lines, "Content should be truncated");
      assert!(file.content.contains("truncated"), "Should have truncation message");
   }

   #[test]
   fn test_file_diff_truncate_very_small_space() {
      let mut file = FileDiff {
         filename:  "test.rs".to_string(),
         header:    "long header content here".to_string(),
         content:   "lots of content that needs to be truncated".to_string(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      };
      file.truncate(30);
      assert_eq!(file.content, "... (truncated)");
   }

   #[test]
   fn test_smart_truncate_diff_under_limit() {
      let config = test_config();
      let counter = test_counter();
      let diff = r"diff --git a/src/main.rs b/src/main.rs
index 123..456 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,2 +1,3 @@
+use std::io;
 fn main() {}";
      let result = smart_truncate_diff(diff, 10000, &config, &counter);
      assert!(result.contains("use std::io"));
      assert!(result.contains("src/main.rs"));
   }

   #[test]
   fn test_smart_truncate_diff_over_limit() {
      let config = test_config();
      let counter = test_counter();
      let lines: Vec<String> = (0..200).map(|i| format!("+line {i}")).collect();
      let content = lines.join("\n");
      let diff = format!(
         "diff --git a/src/main.rs b/src/main.rs\nindex 123..456 100644\n--- a/src/main.rs\n+++ \
          b/src/main.rs\n@@ -1,1 +1,200 @@\n{content}"
      );
      let result = smart_truncate_diff(&diff, 500, &config, &counter);
      assert!(result.len() <= 600); // Allow some overhead
      assert!(result.contains("src/main.rs"));
   }

   #[test]
   fn test_smart_truncate_diff_priority_allocation() {
      let config = test_config();
      let counter = test_counter();
      // High priority source file and low priority markdown
      let diff = r"diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,1 +1,50 @@
+pub fn important_function() {}
+pub fn another_function() {}
+pub fn yet_another() {}
diff --git a/README.md b/README.md
index 333..444 100644
--- a/README.md
+++ b/README.md
@@ -1,1 +1,50 @@
+# Documentation
+More docs here";
      let result = smart_truncate_diff(diff, 300, &config, &counter);
      // Should prioritize lib.rs over README.md
      assert!(result.contains("src/lib.rs"));
      assert!(result.contains("important_function") || result.contains("truncated"));
   }

   #[test]
   fn test_smart_truncate_diff_binary_excluded() {
      let config = test_config();
      let counter = test_counter();
      let diff = r"diff --git a/image.png b/image.png
index 123..456 100644
Binary files a/image.png and b/image.png differ
diff --git a/src/main.rs b/src/main.rs
index 789..abc 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,1 +1,2 @@
 fn main() {}
+fn helper() {}";
      let result = smart_truncate_diff(diff, 10000, &config, &counter);
      assert!(result.contains("src/main.rs"));
      assert!(result.contains("image.png"));
      assert!(result.contains("Binary files"));
   }

   #[test]
   fn test_smart_truncate_diff_excluded_files() {
      let config = test_config();
      let counter = test_counter();
      let diff = r"diff --git a/Cargo.lock b/Cargo.lock
index 123..456 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1,1 +1,100 @@
+lots of lock file content
diff --git a/src/main.rs b/src/main.rs
index 789..abc 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,1 +1,2 @@
 fn main() {}
+fn helper() {}";
      let result = smart_truncate_diff(diff, 10000, &config, &counter);
      assert!(!result.contains("Cargo.lock"));
      assert!(result.contains("src/main.rs"));
   }

   #[test]
   fn test_smart_truncate_diff_all_files_excluded() {
      let config = test_config();
      let counter = test_counter();
      let diff = r"diff --git a/Cargo.lock b/Cargo.lock
index 123..456 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1,1 +1,2 @@
+dependency update";
      let result = smart_truncate_diff(diff, 10000, &config, &counter);
      assert!(result.contains("No relevant files"));
   }

   #[test]
   fn test_smart_truncate_diff_header_preservation() {
      let config = test_config();
      let counter = test_counter();
      let lines: Vec<String> = (0..100).map(|i| format!("+line {i}")).collect();
      let content = lines.join("\n");
      let diff = format!(
         "diff --git a/src/a.rs b/src/a.rs\nindex 111..222 100644\n--- a/src/a.rs\n+++ \
          b/src/a.rs\n@@ -1,1 +1,100 @@\n{content}\ndiff --git a/src/b.rs b/src/b.rs\nindex \
          333..444 100644\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -1,1 +1,100 @@\n{content}"
      );
      let result = smart_truncate_diff(&diff, 600, &config, &counter);
      // Both file headers should be present
      assert!(result.contains("src/a.rs"));
      assert!(result.contains("src/b.rs"));
   }

   #[test]
   fn test_reconstruct_diff_single_file() {
      let files = vec![FileDiff {
         filename:  "test.rs".to_string(),
         header:    "diff --git a/test.rs b/test.rs".to_string(),
         content:   "+new line".to_string(),
         additions: 1,
         deletions: 0,
         is_binary: false,
      }];
      let result = reconstruct_diff(&files);
      assert_eq!(result, "diff --git a/test.rs b/test.rs\n+new line");
   }

   #[test]
   fn test_reconstruct_diff_multiple_files() {
      let files = vec![
         FileDiff {
            filename:  "a.rs".to_string(),
            header:    "diff --git a/a.rs b/a.rs".to_string(),
            content:   "+line a".to_string(),
            additions: 1,
            deletions: 0,
            is_binary: false,
         },
         FileDiff {
            filename:  "b.rs".to_string(),
            header:    "diff --git a/b.rs b/b.rs".to_string(),
            content:   "+line b".to_string(),
            additions: 1,
            deletions: 0,
            is_binary: false,
         },
      ];
      let result = reconstruct_diff(&files);
      assert!(result.contains("a.rs"));
      assert!(result.contains("b.rs"));
      assert!(result.contains("+line a"));
      assert!(result.contains("+line b"));
   }

   #[test]
   fn test_reconstruct_diff_empty_content() {
      let files = vec![FileDiff {
         filename:  "test.rs".to_string(),
         header:    "diff --git a/test.rs b/test.rs".to_string(),
         content:   String::new(),
         additions: 0,
         deletions: 0,
         is_binary: false,
      }];
      let result = reconstruct_diff(&files);
      assert_eq!(result, "diff --git a/test.rs b/test.rs");
   }

   #[test]
   fn test_reconstruct_diff_empty_vec() {
      let files: Vec<FileDiff> = vec![];
      let result = reconstruct_diff(&files);
      assert_eq!(result, "");
   }
}
