//! Markdown format parsers for structured LLM outputs
//!
//! Provides parsers for markdown-formatted responses as an alternative to JSON tool calls.

use std::collections::HashMap;

use crate::{
   error::{CommitGenError, Result},
   types::CommitType,
};

// ===== Leniency helpers =====
// Models wrap the same content many ways: code fences, quotes, mismatched or
// missing tags, bullet glyph variations. These helpers normalize all of that
// before structured parsing so the parsers stay tolerant.

/// Convert literal escape sequences (`\n`, `\r`, `\t`) into real whitespace.
///
/// Some models emit a single physical line containing literal backslash-n
/// instead of real newlines. Only triggers when literal `\n` appears at least
/// as often as real newlines, so text that legitimately contains a stray
/// backslash isn't mangled.
fn normalize_escaped_whitespace(text: &str) -> String {
   let real = text.matches('\n').count();
   let literal = text.matches("\\n").count();
   if literal == 0 || literal < real {
      return text.to_string();
   }
   text.replace("\\r\\n", "\n")
      .replace("\\n", "\n")
      .replace("\\r", "\n")
      .replace("\\t", "\t")
}

/// Strip surrounding Markdown code fences (```lang ... ```), if present.
/// Also normalizes literal `\n`/`\t` escapes first, so every parser that
/// routes through here inherits both behaviors.
fn strip_fences(text: &str) -> String {
   let normalized = normalize_escaped_whitespace(text);
   let t = normalized.trim();
   // Whole-block fence: starts with ``` and ends with ```
   if let Some(after_fence) = t.strip_prefix("```") {
      // Drop the opening fence line (may carry a language tag like ```md).
      let after_open = after_fence.split_once('\n').map_or("", |x| x.1);
      let body = match after_open.rfind("```") {
         Some(end) => &after_open[..end],
         None => after_open,
      };
      return body.trim().to_string();
   }
   // No leading fence: just remove any stray ``` lines.
   t.lines()
      .filter(|l| l.trim_start().trim_end() != "```" && !l.trim_start().starts_with("```"))
      .collect::<Vec<_>>()
      .join("\n")
      .trim()
      .to_string()
}

/// Remove matching wrapping quotes (straight or smart, single/double/backtick).
fn strip_wrapping_quotes(s: &str) -> String {
   let s = s.trim();
   let pairs = [('"', '"'), ('\'', '\''), ('`', '`'), ('“', '”'), ('‘', '’')];
   let chars: Vec<char> = s.chars().collect();
   if chars.len() >= 2 {
      let first = chars[0];
      let last = chars[chars.len() - 1];
      for (open, close) in pairs {
         if first == open && last == close {
            let inner: String = chars[1..chars.len() - 1].iter().collect();
            return inner.trim().to_string();
         }
      }
   }
   s.to_string()
}

/// Strip a leading `Label:` prefix (e.g. "Title:", "Summary:") if present.
fn strip_label_prefix(s: &str) -> String {
   if let Some(colon) = s.find(':') {
      let label = s[..colon].trim().to_lowercase();
      if matches!(label.as_str(), "title" | "summary" | "description" | "result") {
         return s[colon + 1..].trim().to_string();
      }
   }
   s.to_string()
}

/// Strip leading Markdown heading hashes and bold/italic emphasis markers.
fn strip_heading_markers(s: &str) -> String {
   let mut t = s.trim();
   // leading #'s
   t = t.trim_start_matches('#').trim_start();
   // surrounding ** or * emphasis on the whole line
   for marker in ["**", "*", "__", "_"] {
      if t.starts_with(marker) && t.ends_with(marker) && t.len() > 2 * marker.len() {
         t = t[marker.len()..t.len() - marker.len()].trim();
      }
   }
   t.to_string()
}

/// Return the content of a bullet line (`-`, `*`, `•`, `–`) or None.
fn bullet_content(line: &str) -> Option<&str> {
   let t = line.trim_start();
   for glyph in ["- ", "* ", "• ", "– ", "+ "] {
      if let Some(rest) = t.strip_prefix(glyph) {
         return Some(rest.trim());
      }
   }
   None
}

/// Extract content between the first `<tag>` and the next closing `</...>`,
/// tolerating a mismatched closing tag (e.g. `<summary>X</title>`) or a missing
/// close (takes the remainder). Case-insensitive on the opening tag name.
fn extract_tag_lenient(text: &str, tag: &str) -> Option<String> {
   let lower = text.to_lowercase();
   let open = format!("<{tag}");
   let open_pos = lower.find(&open)?;
   // advance to end of the opening tag '>'
   let after_open_rel = text[open_pos..].find('>')? + 1;
   let content_start = open_pos + after_open_rel;
   let rest = &text[content_start..];
   // Find next closing tag of ANY name: "</"
   let end = rest.find("</").unwrap_or(rest.len());
   Some(rest[..end].trim().to_string())
}

/// Shared core: parse `# type(scope): summary` + detail bullets + issue footer.
/// Returns the raw pieces so callers can shape them for their target struct.
struct AnalysisParts {
   commit_type: String,
   scope:       Option<String>,
   summary:     String,
   details:     Vec<String>,
   issue_refs:  Vec<String>,
}

fn parse_analysis_parts(text: &str) -> Result<AnalysisParts> {
   let unfenced = strip_fences(text);
   let lines: Vec<&str> = unfenced.lines().collect();

   // Find the heading line: the first line that parses as `type(scope)?: summary`.
   let mut heading_idx = None;
   let mut parsed_heading = None;
   for (i, line) in lines.iter().enumerate() {
      let candidate = strip_heading_markers(line);
      if let Some(h) = parse_heading(&candidate) {
         heading_idx = Some(i);
         parsed_heading = Some(h);
         break;
      }
      // Only scan the first few lines for the heading.
      if i >= 5 {
         break;
      }
   }

   let (commit_type, scope, summary) = parsed_heading.ok_or_else(|| {
      CommitGenError::Other(
         "markdown analysis: no `type(scope): summary` heading found".to_string(),
      )
   })?;
   let start = heading_idx.unwrap_or(0) + 1;

   let mut details = Vec::new();
   let mut issue_refs = Vec::new();

   for line in &lines[start..] {
      let trimmed_line = line.trim();
      let lower = trimmed_line.to_lowercase();

      if let Some(detail) = bullet_content(trimmed_line) {
         if !detail.is_empty() {
            details.push(detail.to_string());
         }
      } else if let Some(rest) = lower
         .strip_prefix("fixes:")
         .or_else(|| lower.strip_prefix("closes:"))
         .or_else(|| lower.strip_prefix("resolves:"))
      {
         // Use the original-case slice for the refs themselves.
         let orig = &trimmed_line[trimmed_line.len() - rest.len()..];
         for ref_str in orig.split(',') {
            let r = ref_str.trim();
            if !r.is_empty() {
               issue_refs.push(r.to_string());
            }
         }
      }
   }

   Ok(AnalysisParts { commit_type, scope, summary, details, issue_refs })
}

/// Parse markdown conventional analysis format (details as `{text}` objects,
/// matching `ConventionalAnalysis`).
///
/// Lenient: tolerates code fences, headings with/without `#`, bold emphasis,
/// the `type(scope): summary` line appearing on any of the first lines, bullet
/// glyph variations, and `Fixes:`/`Closes:`/`Resolves:` footers.
pub fn parse_conventional_analysis(text: &str) -> Result<serde_json::Value> {
   let p = parse_analysis_parts(text)?;
   let details: Vec<serde_json::Value> =
      p.details.into_iter().map(|t| serde_json::json!({ "text": t })).collect();
   Ok(serde_json::json!({
      "type": p.commit_type,
      "scope": p.scope,
      "summary": p.summary,
      "details": details,
      "issue_refs": p.issue_refs
   }))
}

/// Parse markdown fast-commit format (details as plain strings, matching
/// `FastCommitOutput`). Same heading/bullet grammar as the analysis parser.
pub fn parse_fast_commit(text: &str) -> Result<serde_json::Value> {
   let p = parse_analysis_parts(text)?;
   Ok(serde_json::json!({
      "type": p.commit_type,
      "scope": p.scope,
      "summary": p.summary,
      "details": p.details
   }))
}

/// Parse a `type(scope): summary` or `type: summary` heading line.
/// Returns (type, optional scope, summary). None if it doesn't look like one.
fn parse_heading(line: &str) -> Option<(String, Option<String>, String)> {
   let colon = line.find(':')?;
   let type_scope = line[..colon].trim();
   let summary = line[colon + 1..].trim().to_string();
   if type_scope.is_empty() || summary.is_empty() {
      return None;
   }

   let (ty, scope) = if let Some(p_start) = type_scope.find('(') {
      let p_end = type_scope.find(')')?;
      if p_end < p_start {
         return None;
      }
      let ty = type_scope[..p_start].trim().to_string();
      let sc = type_scope[p_start + 1..p_end].trim();
      (ty, if sc.is_empty() { None } else { Some(sc.to_string()) })
   } else {
      (type_scope.to_string(), None)
   };

   // The type token must be a *valid conventional commit type*. This is what
   // prevents stray `key: value` lines (e.g. `type: "refactor",` from a JSON
   // blob, or `summary: ...`) from being misread as a heading — which would
   // otherwise produce garbage like {"type":"type"} that caches and then fails
   // downstream validation. Only a real type makes a real heading.
   if CommitType::new(&ty).is_err() {
      return None;
   }
   Some((ty, scope, summary))
}

/// Parse markdown summary format.
///
/// Lenient: accepts `<summary>X</summary>`, mismatched/missing close tags,
/// bare text, quoted text, `Title:`-labeled text, and code fences. Collapses
/// internal whitespace so multiline tag bodies become a single line.
pub fn parse_summary_output(text: &str) -> Result<serde_json::Value> {
   let unfenced = strip_fences(text);

   // Prefer an explicit <summary> tag if present (tolerating bad/missing close).
   let raw = extract_tag_lenient(&unfenced, "summary").unwrap_or_else(|| unfenced.clone());

   // Normalize: drop heading markers, label prefixes, quotes; collapse whitespace.
   let stripped = strip_heading_markers(&raw);
   let stripped = strip_label_prefix(&stripped);
   let stripped = strip_wrapping_quotes(&stripped);
   let summary_text = stripped.split_whitespace().collect::<Vec<_>>().join(" ");

   if summary_text.is_empty() {
      return Err(CommitGenError::Other("markdown summary: empty summary text".to_string()));
   }

   Ok(serde_json::json!({ "summary": summary_text }))
}

/// Parse markdown changelog format.
///
/// Lenient: tolerates code fences, headers as `#`/`##`/`###` or bare
/// `Category:` lines, and bullet glyph variations. Recognized categories are
/// matched case-insensitively; unknown `#` headers are still accepted verbatim.
pub fn parse_changelog_response(text: &str) -> Result<serde_json::Value> {
   const KNOWN: [&str; 7] =
      ["Added", "Changed", "Fixed", "Deprecated", "Removed", "Security", "Breaking"];

   let unfenced = strip_fences(text);

   let mut entries: HashMap<String, Vec<String>> = HashMap::new();
   let mut current_category: Option<String> = None;

   let canonical = |name: &str| -> Option<String> {
      let n = name.trim().trim_end_matches(':').trim();
      KNOWN
         .iter()
         .find(|k| k.eq_ignore_ascii_case(n))
         .map(|k| (*k).to_string())
   };

   for line in unfenced.lines() {
      let trimmed_line = line.trim();
      if trimmed_line.is_empty() {
         continue; // tolerate any number of blank/whitespace lines
      }

      // Header detection:
      //  - `#`/`##`/`###` prefixed line (any text), or
      //  - a bare line that *exactly* equals a known category (with optional
      //    trailing `:`), e.g. `Added`, `Added:` — but NOT `Added rate limiting`.
      let header = if trimmed_line.starts_with('#') {
         let h = trimmed_line.trim_start_matches('#').trim().trim_end_matches(':').trim();
         Some(canonical(h).unwrap_or_else(|| h.to_string()))
      } else { canonical(trimmed_line) };

      if let Some(h) = header {
         current_category = Some(h);
         continue;
      }

      // Otherwise it's an entry. Accept bulleted (`-`, `*`, `•`, …) or bare lines.
      let entry = bullet_content(trimmed_line).unwrap_or(trimmed_line).trim();
      if let Some(cat) = &current_category
         && !entry.is_empty() {
            entries.entry(cat.clone()).or_default().push(entry.to_string());
         }
   }

   if entries.is_empty() {
      return Err(CommitGenError::Other(
         "markdown changelog: no entries found (format: ## Category\\n- entry)".to_string(),
      ));
   }

   Ok(serde_json::json!({ "entries": entries }))
}

/// Parse markdown compose intent format.
///
/// Lenient: strips code fences before parsing the `G1 := type(scope): rationale`,
/// `G2 <- G1`, and `Files:` sections; bullet glyphs in the files section vary.
pub fn parse_compose_intent(text: &str) -> Result<serde_json::Value> {
   let trimmed = strip_fences(text);

   let mut groups = Vec::new();
   let mut group_map: HashMap<String, usize> = HashMap::new();

   // First pass: collect group definitions (G1 := type(scope): rationale)
   for line in trimmed.lines() {
      let trimmed_line = line.trim();
      if let Some(assign_pos) = trimmed_line.find(":=") {
         let gid = trimmed_line[..assign_pos].trim().to_string();
         let rest = &trimmed_line[assign_pos + 2..].trim();

         if let Some(colon_pos) = rest.find(':') {
            let type_scope = &rest[..colon_pos].trim();
            let rationale = rest[colon_pos + 1..].trim().to_string();

            let (gtype, scope) = if let Some(paren_start) = type_scope.find('(') {
               if let Some(paren_end) = type_scope.find(')') {
                  let t = type_scope[..paren_start].trim();
                  let s = type_scope[paren_start + 1..paren_end].trim();
                  (t.to_string(), Some(s.to_string()))
               } else {
                  (type_scope.to_string(), None)
               }
            } else {
               (type_scope.to_string(), None)
            };

            group_map.insert(gid.clone(), groups.len());

            let group_obj = serde_json::json!({
               "group_id": gid,
               "type": normalize_commit_type(&gtype),
               "scope": scope,
               "rationale": rationale,
               "file_ids": Vec::<String>::new(),
               "dependencies": Vec::<String>::new()
            });
            groups.push(group_obj);
         }
      }
   }

   // Second pass: parse dependencies (G2 <- G1)
   for line in trimmed.lines() {
      let trimmed_line = line.trim();
      if let Some(dep_pos) = trimmed_line.find("<-") {
         let gid = trimmed_line[..dep_pos].trim().to_string();
         let deps_str = trimmed_line[dep_pos + 2..].trim();

         if let Some(idx) = group_map.get(&gid) {
            let mut dependencies = Vec::new();
            for dep_id in deps_str.split(',') {
               let trimmed_dep = dep_id.trim();
               if !trimmed_dep.is_empty() {
                  dependencies.push(trimmed_dep.to_string());
               }
            }
            if let Some(group_obj) = groups.get_mut(*idx) {
               group_obj["dependencies"] = serde_json::Value::Array(
                  dependencies.into_iter().map(serde_json::Value::String).collect(),
               );
            }
         }
      }
   }

   // Third pass: parse file assignments (- G1: file1, file2)
   let mut in_files_section = false;
   for line in trimmed.lines() {
      let trimmed_line = line.trim();

      if trimmed_line.to_lowercase().starts_with("files:") {
         in_files_section = true;
         continue;
      }

      if in_files_section && let Some(bullet) = bullet_content(trimmed_line)
         && let Some(colon_pos) = bullet.find(':') {
            let gid = bullet[..colon_pos].trim().to_string();
            let files_str = bullet[colon_pos + 1..].trim();

            if let Some(idx) = group_map.get(&gid)
               && let Some(group_obj) = groups.get_mut(*idx) {
               group_obj["file_ids"] = serde_json::Value::Array(
                  files_str.split(',').map(|f| serde_json::Value::String(f.trim().to_string())).collect(),
               );
            }
         }
   }

   if groups.is_empty() {
      return Err(CommitGenError::Other(
         "markdown compose intent: no groups found (format: G1 := type(scope): rationale)".to_string(),
      ));
   }

   Ok(serde_json::json!({
      "groups": groups
   }))
}

/// Parse markdown compose binding format.
///
/// Lenient: strips code fences; group headers accept `#`/`##` (with or without
/// trailing colon); hunk bullets accept varied glyphs.
pub fn parse_compose_binding(text: &str) -> Result<serde_json::Value> {
   let trimmed = strip_fences(text);

   let mut assignments = Vec::new();
   let mut current_group: Option<String> = None;
   let mut current_hunks: Vec<String> = Vec::new();

   for line in trimmed.lines() {
      let trimmed_line = line.trim();

      if trimmed_line.starts_with('#') {
         // Save previous group if any
         if let Some(gid) = current_group.take() {
            assignments.push(serde_json::json!({
               "group_id": gid,
               "hunk_ids": std::mem::take(&mut current_hunks)
            }));
         }
         // Start new group (strip hashes and any trailing colon)
         let new_gid = trimmed_line
            .trim_start_matches('#')
            .trim()
            .trim_end_matches(':')
            .trim()
            .to_string();
         current_group = Some(new_gid);
      } else if let Some(hunk_id) = bullet_content(trimmed_line) {
         current_hunks.push(hunk_id.to_string());
      }
   }

   // Save final group
   if let Some(gid) = current_group.take() {
      assignments.push(serde_json::json!({
         "group_id": gid,
         "hunk_ids": std::mem::take(&mut current_hunks)
      }));
   }

   if assignments.is_empty() {
      return Err(CommitGenError::Other(
         "markdown compose binding: no assignments found (format: # group_id\\n- hunk_id)".to_string(),
      ));
   }

   Ok(serde_json::json!({
      "assignments": assignments
   }))
}

/// Parse markdown map-phase batch observations.
///
/// Format: each file is a `## path` (or `# path`) header, followed by bullet or
/// bare-line observations. Produces `{ "files": [{ "path", "observations" }] }`.
/// Files with no observations are kept with an empty array. Lenient: strips
/// fences, accepts varied bullet glyphs and bare-line observations.
pub fn parse_batch_observations(text: &str) -> Result<serde_json::Value> {
   let unfenced = strip_fences(text);

   let mut files: Vec<serde_json::Value> = Vec::new();
   let mut current_path: Option<String> = None;
   let mut current_obs: Vec<String> = Vec::new();

   for line in unfenced.lines() {
      let t = line.trim();
      if t.is_empty() {
         continue;
      }

      if t.starts_with('#') {
         // New file header — flush the previous one.
         if let Some(path) = current_path.take() {
            files.push(serde_json::json!({
               "path": path,
               "observations": std::mem::take(&mut current_obs),
            }));
         }
         current_path = Some(t.trim_start_matches('#').trim().to_string());
      } else if current_path.is_some() {
         // Observation: bullet or bare line.
         let obs = bullet_content(t).unwrap_or(t).trim();
         if !obs.is_empty() {
            current_obs.push(obs.to_string());
         }
      }
   }

   if let Some(path) = current_path.take() {
      files.push(serde_json::json!({
         "path": path,
         "observations": current_obs,
      }));
   }

   if files.is_empty() {
      return Err(CommitGenError::Other(
         "markdown observations: no file sections found (format: ## path\\n- observation)"
            .to_string(),
      ));
   }

   Ok(serde_json::json!({ "files": files }))
}

/// Normalize commit type string
fn normalize_commit_type(s: &str) -> String {
   match s.to_lowercase().as_str() {
      "feat" | "feature" => "feat".to_string(),
      "fix" | "bugfix" => "fix".to_string(),
      "docs" | "documentation" => "docs".to_string(),
      "style" | "formatting" => "style".to_string(),
      "refactor" | "refactoring" => "refactor".to_string(),
      "perf" | "performance" => "perf".to_string(),
      "test" | "tests" => "test".to_string(),
      "build" | "builder" => "build".to_string(),
      "ci" | "cicd" => "ci".to_string(),
      "chore" | "maintenance" => "chore".to_string(),
      "revert" | "reversion" => "revert".to_string(),
      "deps" | "dependencies" | "dependency" => "deps".to_string(),
      "security" | "sec" => "security".to_string(),
      "config" | "configuration" => "config".to_string(),
      "ux" | "ergonomics" => "ux".to_string(),
      "release" | "version" => "release".to_string(),
      "hotfix" => "hotfix".to_string(),
      "infra" | "infrastructure" => "infra".to_string(),
      "init" | "initialization" => "init".to_string(),
      "merge" | "merging" => "merge".to_string(),
      "hack" | "hacky" => "hack".to_string(),
      "wip" | "work-in-progress" => "wip".to_string(),
      other => other.to_string(),
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   // ===== conventional analysis =====

   #[test]
   fn test_conventional_analysis() {
      let md = "# feat(api): add user authentication endpoint\n\n- Added POST \
                /auth/login endpoint\n- Implemented bcrypt password hashing\n\nFixes: #123";
      let r = parse_conventional_analysis(md).unwrap();
      assert_eq!(r["type"], "feat");
      assert_eq!(r["scope"], "api");
      assert_eq!(r["details"].as_array().unwrap().len(), 2);
      assert_eq!(r["issue_refs"][0], "#123");
   }

   #[test]
   fn test_analysis_lenient_variations() {
      // fenced, no `#`, bold heading, `*` bullets, Closes: footer
      let md = "```md\n**fix(core): corrected null deref**\n\n* fixed a crash\n* \
                guarded the pointer\n\nCloses: #7, #8\n```";
      let r = parse_conventional_analysis(md).unwrap();
      assert_eq!(r["type"], "fix");
      assert_eq!(r["scope"], "core");
      assert_eq!(r["details"].as_array().unwrap().len(), 2);
      assert_eq!(r["issue_refs"].as_array().unwrap().len(), 2);
   }

   #[test]
   fn test_analysis_no_scope_and_leading_blank_lines() {
      let md = "\n\n\n# chore: bumped version\n";
      let r = parse_conventional_analysis(md).unwrap();
      assert_eq!(r["type"], "chore");
      assert!(r["scope"].is_null());
   }

   #[test]
   fn test_heading_requires_known_type_not_json_key() {
      // A stray JSON/YAML `type:` key must NOT be misread as a heading.
      // (This used to yield {"type":"type"} which cached and then blew up.)
      let json_ish = "{\n  \"type\": \"refactor\",\n  \"summary\": \"did things\"\n}";
      assert!(parse_conventional_analysis(json_ish).is_err());
      // And `summary:`/`scope:` key lines are likewise not headings.
      assert!(parse_conventional_analysis("summary: did a thing\nscope: core").is_err());
   }

   #[test]
   fn test_fast_commit_details_are_plain_strings() {
      // FastCommitOutput.details is Vec<String>, so the fast parser must emit
      // string details (not {text} objects like the analysis parser).
      let md = "# refactor(web): derive provider order from options\n\n- Derived the \
                metadata dynamically.\n- Reprioritized the default sequence.";
      let r = parse_fast_commit(md).unwrap();
      assert_eq!(r["type"], "refactor");
      assert_eq!(r["scope"], "web");
      let details = r["details"].as_array().unwrap();
      assert_eq!(details.len(), 2);
      assert!(details[0].is_string(), "fast details must be strings");
      // It must deserialize into the real FastCommitOutput shape.
      #[derive(serde::Deserialize)]
      struct FastShape {
         #[serde(rename = "type")]
         _t:      String,
         details: Vec<String>,
      }
      let parsed: FastShape = serde_json::from_value(r).unwrap();
      assert_eq!(parsed.details.len(), 2);
   }

   // ===== summary: all the wrapping variations =====

   #[test]
   fn test_summary_variations() {
      let cases = [
         "<summary>Added JWT auth</summary>",
         "Added JWT auth",                      // bare
         "\"Added JWT auth\"",                  // quoted
         "<summary>\"Added JWT auth\"</title>", // quoted + mismatched close tag
         "```md\n<summary>\nAdded JWT auth\n</summary>\n```", // fenced + multiline
         "Title: Added JWT auth",               // labeled
         "# Added JWT auth",                    // heading marker
         "\n\n  Added JWT auth  \n\n",          // stray whitespace
      ];
      for c in cases {
         let r = parse_summary_output(c).unwrap();
         assert_eq!(r["summary"], "Added JWT auth", "input was: {c:?}");
      }
   }

   // ===== changelog: header + item variations =====

   #[test]
   fn test_changelog_hash_and_dash() {
      let md = "# Added\n- POST /auth/login endpoint\n\n# Fixed\n- Race condition";
      let r = parse_changelog_response(md).unwrap();
      let e = r["entries"].as_object().unwrap();
      assert_eq!(e["Added"].as_array().unwrap().len(), 1);
      assert_eq!(e["Fixed"].as_array().unwrap().len(), 1);
   }

   #[test]
   fn test_changelog_lenient_mixed() {
      // `##` and `#` and bare `Category:` headers; `-`, `*`, and bare items;
      // random blank lines.
      let md = "## Added\n- one\n* two\n\n\nFixed:\nthree\n- four\n\n# Security\n\n  five  ";
      let r = parse_changelog_response(md).unwrap();
      let e = r["entries"].as_object().unwrap();
      assert_eq!(e["Added"].as_array().unwrap().len(), 2, "Added");
      assert_eq!(e["Fixed"].as_array().unwrap().len(), 2, "Fixed (bare + dash)");
      assert_eq!(e["Security"].as_array().unwrap().len(), 1, "Security (bare item)");
   }

   #[test]
   fn test_changelog_bare_category_not_confused_with_item() {
      // "Added rate limiting" must be an ITEM, not a header.
      let md = "# Security\n- Added rate limiting on auth endpoints";
      let r = parse_changelog_response(md).unwrap();
      let e = r["entries"].as_object().unwrap();
      assert!(e.contains_key("Security"));
      assert!(!e.contains_key("Added"));
      assert_eq!(e["Security"][0], "Added rate limiting on auth endpoints");
   }

   #[test]
   fn test_changelog_fenced() {
      let md = "```\n# Added\n- thing\n```";
      let r = parse_changelog_response(md).unwrap();
      assert_eq!(r["entries"]["Added"][0], "thing");
   }

   // ===== literal \n escapes =====

   #[test]
   fn test_literal_backslash_n_analysis() {
      // A model emitted the whole thing on one physical line with literal \n.
      let md = "# feat(api): add auth\\n\\n- did a thing\\n- did another\\n\\nFixes: #1";
      let r = parse_conventional_analysis(md).unwrap();
      assert_eq!(r["type"], "feat");
      assert_eq!(r["scope"], "api");
      assert_eq!(r["details"].as_array().unwrap().len(), 2);
      assert_eq!(r["issue_refs"][0], "#1");
   }

   #[test]
   fn test_literal_backslash_n_changelog() {
      let md = "# Added\\n- one\\n- two\\n# Fixed\\n- three";
      let r = parse_changelog_response(md).unwrap();
      let e = r["entries"].as_object().unwrap();
      assert_eq!(e["Added"].as_array().unwrap().len(), 2);
      assert_eq!(e["Fixed"].as_array().unwrap().len(), 1);
   }

   #[test]
   fn test_real_newlines_with_stray_backslash_preserved() {
      // Real newlines dominate → don't touch a legitimate backslash in content.
      let md = "# docs: explain C:\\\\path usage\n- noted the path C:\\nope is literal";
      let r = parse_conventional_analysis(md).unwrap();
      assert_eq!(r["type"], "docs");
      // The single detail line is preserved (not split on the literal \n).
      assert_eq!(r["details"].as_array().unwrap().len(), 1);
   }

   // ===== compose =====

   #[test]
   fn test_compose_intent_fenced() {
      let md = "```\nG1 := feat(api): add endpoints\nG2 := test(api): add tests\n\nG2 \
                <- G1\n\nFiles:\n- G1: a.rs, b.rs\n* G2: c.test.ts\n```";
      let r = parse_compose_intent(md).unwrap();
      let g = r["groups"].as_array().unwrap();
      assert_eq!(g.len(), 2);
      assert_eq!(g[0]["file_ids"].as_array().unwrap().len(), 2);
      assert_eq!(g[1]["dependencies"][0], "G1");
      assert_eq!(g[1]["file_ids"][0], "c.test.ts"); // `*` bullet handled
   }

   #[test]
   fn test_compose_binding_lenient() {
      let md = "```\n## G1:\n- h1\n* h2\n# G2\n- h3\n```";
      let r = parse_compose_binding(md).unwrap();
      let a = r["assignments"].as_array().unwrap();
      assert_eq!(a.len(), 2);
      assert_eq!(a[0]["group_id"], "G1"); // trailing colon + `##` stripped
      assert_eq!(a[0]["hunk_ids"].as_array().unwrap().len(), 2);
   }

   // ===== map-phase batch observations =====

   #[test]
   fn test_batch_observations() {
      let md = "## src/config.rs\n- added TOML loading\n- changed timeout\n\n## \
                src/main.rs\n- wired CLI flag\n\n## src/empty.rs";
      let r = parse_batch_observations(md).unwrap();
      let files = r["files"].as_array().unwrap();
      assert_eq!(files.len(), 3);
      assert_eq!(files[0]["path"], "src/config.rs");
      assert_eq!(files[0]["observations"].as_array().unwrap().len(), 2);
      assert_eq!(files[1]["observations"].as_array().unwrap().len(), 1);
      assert_eq!(files[2]["observations"].as_array().unwrap().len(), 0); // header only
   }

   #[test]
   fn test_batch_observations_fenced_and_literal_newlines() {
      let md = "```\\n## a.rs\\n- did x\\n* did y\\n## b.rs\\n- did z\\n```";
      let r = parse_batch_observations(md).unwrap();
      let files = r["files"].as_array().unwrap();
      assert_eq!(files.len(), 2);
      assert_eq!(files[0]["path"], "a.rs");
      assert_eq!(files[0]["observations"].as_array().unwrap().len(), 2);
   }
}
