use serde::{Deserialize, Serialize};

use crate::types::{CommitType, Scope};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeHunk {
   pub hunk_id:      String,
   pub file_id:      String,
   pub path:         String,
   pub old_start:    usize,
   pub old_count:    usize,
   pub new_start:    usize,
   pub new_count:    usize,
   pub header:       String,
   pub raw_patch:    String,
   pub snippet:      String,
   pub semantic_key: String,
   pub synthetic:    bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeFile {
   pub file_id:        String,
   pub path:           String,
   pub patch_header:   String,
   pub full_patch:     String,
   pub summary:        String,
   pub hunk_ids:       Vec<String>,
   pub additions:      usize,
   pub deletions:      usize,
   pub is_binary:      bool,
   pub synthetic_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeSnapshot {
   pub diff:  String,
   pub stat:  String,
   pub files: Vec<ComposeFile>,
   pub hunks: Vec<ComposeHunk>,
}

impl ComposeSnapshot {
   pub fn file_by_id(&self, file_id: &str) -> Option<&ComposeFile> {
      self.files.iter().find(|file| file.file_id == file_id)
   }

   pub fn file_by_path(&self, path: &str) -> Option<&ComposeFile> {
      self.files.iter().find(|file| file.path == path)
   }

   pub fn hunk_by_id(&self, hunk_id: &str) -> Option<&ComposeHunk> {
      self.hunks.iter().find(|hunk| hunk.hunk_id == hunk_id)
   }

   pub fn hunks_for_file(&self, file_id: &str) -> Vec<&ComposeHunk> {
      self
         .hunks
         .iter()
         .filter(|hunk| hunk.file_id == file_id)
         .collect()
   }

   pub fn all_hunk_ids(&self) -> Vec<String> {
      self.hunks.iter().map(|hunk| hunk.hunk_id.clone()).collect()
   }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeIntentGroup {
   pub group_id:     String,
   #[serde(rename = "type")]
   pub commit_type:  CommitType,
   #[serde(default, deserialize_with = "deserialize_optional_scope_lossy")]
   pub scope:        Option<Scope>,
   #[serde(default)]
   pub file_ids:     Vec<String>,
   pub rationale:    String,
   #[serde(default)]
   pub dependencies: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeIntentPlan {
   pub groups:           Vec<ComposeIntentGroup>,
   pub dependency_order: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeBindingAssignment {
   pub group_id: String,
   #[serde(default)]
   pub hunk_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeExecutableGroup {
   pub group_id:     String,
   #[serde(rename = "type")]
   pub commit_type:  CommitType,
   #[serde(default, deserialize_with = "deserialize_optional_scope_lossy")]
   pub scope:        Option<Scope>,
   #[serde(default)]
   pub file_ids:     Vec<String>,
   pub rationale:    String,
   #[serde(default)]
   pub dependencies: Vec<String>,
   #[serde(default)]
   pub hunk_ids:     Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeExecutablePlan {
   pub groups:           Vec<ComposeExecutableGroup>,
   pub dependency_order: Vec<usize>,
}

fn deserialize_optional_scope_lossy<'de, D>(
   deserializer: D,
) -> std::result::Result<Option<Scope>, D::Error>
where
   D: serde::Deserializer<'de>,
{
   let value = Option::<String>::deserialize(deserializer)?;
   Ok(value.as_deref().and_then(coerce_scope))
}

fn coerce_scope(raw: &str) -> Option<Scope> {
   let normalized = raw.trim().replace('\\', "/").to_lowercase();

   let segments: Vec<String> = normalized
      .split('/')
      .filter_map(sanitize_scope_segment)
      .take(2)
      .collect();

   if segments.is_empty() {
      return None;
   }

   Scope::new(segments.join("/")).ok()
}

fn sanitize_scope_segment(segment: &str) -> Option<String> {
   let mut out = String::new();
   let mut last_was_separator = false;

   for ch in segment.trim().chars() {
      if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
         out.push(ch);
         last_was_separator = false;
      } else if ch == '-' || ch == '_' {
         if !out.is_empty() && !last_was_separator {
            out.push(ch);
            last_was_separator = true;
         }
      } else if (ch.is_ascii_whitespace() || ch == '.') && !out.is_empty() && !last_was_separator {
         out.push('-');
         last_was_separator = true;
      }
   }

   let trimmed = out.trim_matches(['-', '_']).to_string();
   (!trimmed.is_empty()).then_some(trimmed)
}
