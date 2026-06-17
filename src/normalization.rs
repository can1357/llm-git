/// Normalization utilities for commit messages
use unicode_normalization::UnicodeNormalization;

use crate::{config::CommitConfig, types::ConventionalCommit, validation::is_past_tense_verb};

/// Normalize Unicode characters to ASCII (remove AI-style formatting)
/// Normalize Unicode characters to ASCII (remove AI-style formatting)
pub fn normalize_unicode(text: &str) -> String {
   // Pre-NFKD replacements for chars that decompose badly
   // (≠ → = + combining, ½ → 1⁄2, ² → 2)
   let pre_normalized = text
      // Math symbols that decompose badly
      .replace('≠', "!=") // not equal to (decomposes to = + \u{338})
      // Fractions (NFKD decomposes ½ to 1⁄2 with fraction slash, not regular /)
      .replace('½', "1/2")
      .replace('¼', "1/4")
      .replace('¾', "3/4")
      .replace('⅓', "1/3")
      .replace('⅔', "2/3")
      .replace('⅕', "1/5")
      .replace('⅖', "2/5")
      .replace('⅗', "3/5")
      .replace('⅘', "4/5")
      .replace('⅙', "1/6")
      .replace('⅚', "5/6")
      .replace('⅛', "1/8")
      .replace('⅜', "3/8")
      .replace('⅝', "5/8")
      .replace('⅞', "7/8")
      // Superscripts (NFKD decomposes ² to just "2", losing the superscript meaning)
      .replace('⁰', "^0")
      .replace('¹', "^1")
      .replace('²', "^2")
      .replace('³', "^3")
      .replace('⁴', "^4")
      .replace('⁵', "^5")
      .replace('⁶', "^6")
      .replace('⁷', "^7")
      .replace('⁸', "^8")
      .replace('⁹', "^9")
      // Subscripts
      .replace('₀', "_0")
      .replace('₁', "_1")
      .replace('₂', "_2")
      .replace('₃', "_3")
      .replace('₄', "_4")
      .replace('₅', "_5")
      .replace('₆', "_6")
      .replace('₇', "_7")
      .replace('₈', "_8")
      .replace('₉', "_9");

   // Apply NFKD normalization for canonical decomposition
   let normalized: String = pre_normalized.nfkd().collect();

   normalized
      // Smart quotes to straight quotes
      .replace(['\u{2018}', '\u{2019}'], "'") // ' right single quote / apostrophe
      .replace(['\u{201C}', '\u{201D}'], "\"") // " right double quote
      .replace('\u{201A}', "'") // ‚ single low-9 quote
      .replace(['\u{201E}', '\u{00AB}', '\u{00BB}'], "\"") // » right-pointing double angle quote
      .replace(['\u{2039}', '\u{203A}'], "'") // › single right-pointing angle quote
      // Dashes and hyphens
      .replace(['\u{2010}', '\u{2011}', '\u{2012}'], "-") // ‒ figure dash
      .replace(['\u{2013}', '\u{2014}', '\u{2015}'], "--") // ― horizontal bar
      .replace('\u{2212}', "-") // − minus sign
      // Arrows
      .replace('\u{2192}', "->") // rightwards arrow
      .replace('←', "<-") // leftwards arrow
      .replace('↔', "<->") // left right arrow
      .replace('⇒', "=>") // rightwards double arrow
      .replace('⇐', "<=") // leftwards double arrow
      .replace('⇔', "<=>") // left right double arrow
      .replace('↑', "^") // upwards arrow
      .replace('↓', "v") // downwards arrow
      // Math symbols
      .replace('\u{2264}', "<=") // less than or equal to
      .replace('≥', ">=") // greater than or equal to
      .replace('≈', "~=") // approximately equal to
      .replace('≡', "==") // identical to
      .replace('\u{00D7}', "x") // multiplication sign
      .replace('÷', "/") // division sign
      // Ellipsis
      .replace(['\u{2026}', '⋯', '⋮'], "...") // vertical ellipsis
      // Bullet points (convert to hyphens for consistency)
      .replace(['•', '◦', '▪', '▫', '◆', '◇'], "-") // white diamond
      // Check marks
      .replace(['✓', '✔'], "v") // heavy check mark
      .replace(['✗', '✘'], "x") // heavy ballot x
      // Greek letters (common in programming)
      .replace('λ', "lambda")
      .replace('α', "alpha")
      .replace('β', "beta")
      .replace('γ', "gamma")
      .replace('δ', "delta")
      .replace('ε', "epsilon")
      .replace('θ', "theta")
      .replace('μ', "mu")
      .replace('π', "pi")
      .replace('σ', "sigma")
      .replace('Σ', "Sigma")
      .replace('Δ', "Delta")
      .replace('Π', "Pi")
      // Special spaces to regular space
      .replace(
         [
            '\u{00A0}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}',
            '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200A}', '\u{202F}', '\u{205F}',
            '\u{3000}',
         ],
         " ",
      ) // ideographic space
      // Zero-width characters (remove)
      .replace(['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}'], "") // zero-width no-break space (BOM)
}

/// Estimate token count for text (rough approximation: 1 token ≈ 4 chars)
const fn estimate_tokens(text: &str) -> usize {
   text.len().div_ceil(4) // Round up
}

/// Cap detail points by token budget instead of hard count
/// Keeps high-priority details until budget exhausted
pub fn cap_details(details: &mut Vec<String>, max_tokens: usize) {
   if details.is_empty() {
      return;
   }

   // Calculate total tokens
   let total_tokens: usize = details.iter().map(|d| estimate_tokens(d)).sum();

   if total_tokens <= max_tokens {
      return; // Under budget, keep all
   }

   // Score by priority keywords and length
   let mut scored: Vec<(usize, i32, usize, &String)> = details
      .iter()
      .enumerate()
      .map(|(idx, detail)| {
         let lower = detail.to_lowercase();
         let mut score = 0;

         // High priority keywords (security, crashes, critical bugs)
         if lower.contains("security")
            || lower.contains("vulnerability")
            || lower.contains("exploit")
            || lower.contains("critical")
            || (lower.contains("fix") && lower.contains("crash"))
         {
            score += 100;
         }
         if lower.contains("breaking") || lower.contains("incompatible") {
            score += 90;
         }
         if lower.contains("performance")
            || lower.contains("faster")
            || lower.contains("optimization")
         {
            score += 80;
         }
         if lower.contains("fix") || lower.contains("bug") {
            score += 70;
         }

         // Medium priority keywords
         if lower.contains("api") || lower.contains("interface") || lower.contains("public") {
            score += 50;
         }
         if lower.contains("user") || lower.contains("client") {
            score += 40;
         }
         if lower.contains("deprecated") || lower.contains("removed") {
            score += 35;
         }

         // Add length component (capped contribution to avoid favoring verbosity)
         score += (detail.len() / 20).min(10) as i32;

         let tokens = estimate_tokens(detail);
         (idx, score, tokens, detail)
      })
      .collect();

   // Sort by score descending
   scored.sort_by_key(|item| std::cmp::Reverse(item.1));

   // Keep details until budget exhausted
   let mut budget_remaining = max_tokens;
   let mut keep_indices: Vec<usize> = Vec::new();

   for (idx, _score, tokens, _detail) in scored {
      if tokens <= budget_remaining {
         keep_indices.push(idx);
         budget_remaining -= tokens;
      }
   }

   keep_indices.sort_unstable(); // Preserve original order

   // Filter details
   let kept: Vec<String> = keep_indices
      .iter()
      .filter_map(|&idx| details.get(idx).cloned())
      .collect();
   *details = kept;
}

/// Convert present-tense verbs to past-tense and handle type-specific
/// replacements. Uses the shared [`crate::validation::PAST_TENSE_MAP`] so
/// normalization and validation stay in sync.
pub fn normalize_summary_verb(summary: &mut String, commit_type: &str) {
   use crate::validation::{present_to_past, split_verb_token, verb_stem};

   if summary.trim().is_empty() {
      return;
   }

   let mut parts_iter = summary.split_whitespace();
   let first_word = match parts_iter.next() {
      Some(word) => word.to_string(),
      None => return,
   };
   let rest = parts_iter.collect::<Vec<_>>().join(" ");
   let first_word_lower = first_word.to_lowercase();

   // Check if already past tense (full token, e.g. `re-enabled`).
   if is_past_tense_verb(&first_word_lower) {
      // Special case: refactor type shouldn't use "refactored"
      if commit_type == "refactor" && first_word_lower == "refactored" {
         *summary = if rest.is_empty() {
            "restructured".to_string()
         } else {
            format!("restructured {rest}")
         };
      }
      return;
   }

   // Skip tokens that aren't convertible verbs:
   //  - all-caps acronyms (API, NFC, LSP)
   //  - numeric-led tokens (403, v1.0, 2.0.0)
   //  - tokens whose suffix isn't a simple dash/slash separator (e.g.
   //    `fix(tui):` is a leaked type prefix, not a verb to convert)
   let Some((stem_raw, suffix)) = split_verb_token(&first_word) else {
      return;
   };
   let stem = stem_raw.to_ascii_lowercase();

   // Skip all-caps acronyms: stem would be all uppercase, and verb_stem
   // already returns None for those.
   if verb_stem(&first_word).is_none() {
      return;
   }

   // Only reattach suffix for simple separators (`-`, `/`). A suffix like
   // `(tui):` means the model leaked the conventional prefix; leave it alone
   // (strip_type_prefix handles that elsewhere).
   let safe_suffix = if suffix.is_empty() || suffix.starts_with('-') || suffix.starts_with('/') {
      suffix
   } else {
      // Non-separator suffix (e.g. `(tui):`): don't convert, to avoid
      // producing `fixed(tui):`.
      return;
   };

   // Handle `re-` prefixed verbs: `split_verb_token("re-enable")` gives
   // stem="re", suffix="-enable". Detect that, parse the verb segment from
   // the suffix, convert it, and rebuild as `re-{past}{tail}`.
   if stem == "re" && safe_suffix.starts_with('-') {
      let after_dash = &safe_suffix[1..]; // skip '-'
      let next_n = after_dash
         .bytes()
         .take_while(|&b| b.is_ascii_alphabetic())
         .count();
      if next_n > 0 {
         let inner = after_dash[..next_n].to_ascii_lowercase();
         let tail = &after_dash[next_n..]; // e.g. "" or "-checking"

         let inner_past = present_to_past(&inner)
            .or_else(|| inner.strip_suffix('s').and_then(|s| present_to_past(s)))
            .or_else(|| inner.strip_suffix("es").and_then(|s| present_to_past(s)))
            .or_else(|| {
               inner.strip_suffix("ies").and_then(|s| present_to_past(&format!("{s}y")))
            })
            .map(|p| {
               if commit_type == "refactor" && p == "refactored" {
                  "restructured"
               } else {
                  p
               }
            });

         if let Some(past) = inner_past {
            *summary = if rest.is_empty() {
               format!("re-{past}{tail}")
            } else {
               format!("re-{past}{tail} {rest}")
            };
         }
      }
      return;
   }

   // Normal case: look up the stem directly.
   let past = present_to_past(&stem)
      .or_else(|| {
         // Strip trailing 's' for third-person present (adds -> add).
         stem.strip_suffix('s').and_then(|s| present_to_past(s))
      })
      .or_else(|| {
         // Strip trailing 'es' for verbs ending in s/sh/ch/x/z (fixes -> fix).
         stem.strip_suffix("es").and_then(|s| present_to_past(s))
      })
      .or_else(|| {
         // -ies -> -y (simplifies -> simplify, applies -> apply).
         stem.strip_suffix("ies").and_then(|s| {
            present_to_past(&format!("{s}y"))
         })
      })
      .map(|p| {
         // Special case: refactor type shouldn't use "refactored"
         if commit_type == "refactor" && p == "refactored" {
            "restructured"
         } else {
            p
         }
      });

   if let Some(past) = past {
      *summary = if rest.is_empty() {
         format!("{past}{safe_suffix}")
      } else {
         format!("{past}{safe_suffix} {rest}")
      };
   }
}

/// Post-process conventional commit message to fix common issues
pub fn post_process_commit_message(msg: &mut ConventionalCommit, config: &CommitConfig) {
   // CommitType and Scope are already normalized to lowercase in their
   // constructors No need to re-normalize them here

   // Extract summary string for mutations, will reconstruct at end
   let mut summary_str = normalize_unicode(msg.summary.as_str());

   // Normalize body and footers
   msg.body = msg.body.iter().map(|s| normalize_unicode(s)).collect();
   msg.footers = msg.footers.iter().map(|s| normalize_unicode(s)).collect();

   // Normalize summary formatting: single line, trimmed, enforce trailing period
   summary_str = summary_str
      .replace(['\r', '\n'], " ")
      .split_whitespace()
      .collect::<Vec<_>>()
      .join(" ")
      .trim()
      .trim_end_matches('.')
      .trim_end_matches(';')
      .trim_end_matches(':')
      .to_string();

   // Helper: check if first token is all caps (acronym/initialism)
   let is_first_token_all_caps = |s: &str| -> bool {
      s.split_whitespace().next().is_some_and(|token| {
         token
            .chars()
            .all(|c| !c.is_alphabetic() || c.is_uppercase())
      })
   };

   // Ensure summary starts with lowercase (unless first token is all caps)
   if !is_first_token_all_caps(&summary_str)
      && let Some(first_char) = summary_str.chars().next()
      && first_char.is_uppercase()
   {
      let rest = &summary_str[first_char.len_utf8()..];
      summary_str = format!("{}{}", first_char.to_lowercase(), rest);
   }

   // Normalize verb tense (present \u{2192} past, handle type-specific
   // replacements)
   normalize_summary_verb(&mut summary_str, msg.commit_type.as_str());
   summary_str = summary_str.trim().to_string();

   // Ensure lowercase after normalization (unless first token is all caps)
   if !is_first_token_all_caps(&summary_str)
      && let Some(first_char) = summary_str.chars().next()
      && first_char.is_uppercase()
   {
      let rest = &summary_str[first_char.len_utf8()..];
      summary_str = format!("{}{}", first_char.to_lowercase(), rest);
   }

   // No truncation - validation handles length checks
   // Remove any trailing period (conventional commits don't use periods)
   summary_str = summary_str.trim_end_matches('.').to_string();

   // Reconstruct CommitSummary (bypassing warnings since post-processing
   // normalizes)
   msg.summary = crate::types::CommitSummary::new_unchecked(summary_str, 128)
      .expect("post-processed summary should be valid");

   // Clean and enforce punctuation for body items
   for item in &mut msg.body {
      let mut cleaned = item
         .replace(['\r', '\n'], " ")
         .trim()
         .trim_start_matches('\u{2022}')
         .trim_start_matches('-')
         .trim_start_matches('*')
         .trim_start_matches('+')
         .trim()
         .to_string();

      cleaned = cleaned
         .split_whitespace()
         .collect::<Vec<_>>()
         .join(" ")
         .trim()
         .trim_end_matches('.')
         .trim_end_matches(';')
         .trim_end_matches(',')
         .to_string();

      if cleaned.is_empty() {
         *item = cleaned;
         continue;
      }

      // Capitalize first letter
      if let Some(first_char) = cleaned.chars().next()
         && first_char.is_lowercase()
      {
         let rest = &cleaned[first_char.len_utf8()..];
         cleaned = format!("{}{}", first_char.to_uppercase(), rest);
      }

      if !cleaned.ends_with('.') {
         cleaned.push('.');
      }

      *item = cleaned;
   }

   // Remove empty body items
   msg.body.retain(|item| !item.trim().is_empty());

   // Cap details by token budget
   cap_details(&mut msg.body, config.max_detail_tokens);
}

/// Format `ConventionalCommit` as a single string for display and commit
pub fn format_commit_message(msg: &ConventionalCommit) -> String {
   // Build first line: type(scope): summary
   let scope_part = msg
      .scope
      .as_ref()
      .map(|s| format!("({s})"))
      .unwrap_or_default();
   let first_line = format!("{}{}: {}", msg.commit_type, scope_part, msg.summary);

   // Build body with - bullets
   let body_formatted = if msg.body.is_empty() {
      String::new()
   } else {
      msg.body
         .iter()
         .map(|item| format!("- {item}"))
         .collect::<Vec<_>>()
         .join("\n")
   };

   // Build footers
   let footers_formatted = if msg.footers.is_empty() {
      String::new()
   } else {
      msg.footers.join("\n")
   };

   // Combine parts
   let mut result = first_line;
   if !body_formatted.is_empty() {
      result.push_str("\n\n");
      result.push_str(&body_formatted);
   }
   if !footers_formatted.is_empty() {
      result.push_str("\n\n");
      result.push_str(&footers_formatted);
   }
   result
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::types::{CommitSummary, CommitType, ConventionalCommit, Scope};

   // normalize_unicode tests
   #[test]
   fn test_normalize_unicode_smart_quotes() {
      assert_eq!(normalize_unicode("\u{2018}smart quotes\u{2019}"), "'smart quotes'");
      assert_eq!(normalize_unicode("\u{201C}double quotes\u{201D}"), "\"double quotes\"");
      assert_eq!(normalize_unicode("\u{201A}low quote\u{2019}"), "'low quote'");
      assert_eq!(normalize_unicode("\u{201E}low double\u{201D}"), "\"low double\"");
   }

   #[test]
   fn test_normalize_unicode_dashes() {
      assert_eq!(normalize_unicode("en\u{2013}dash"), "en--dash");
      assert_eq!(normalize_unicode("em\u{2014}dash"), "em--dash");
      assert_eq!(normalize_unicode("fig\u{2012}dash"), "fig-dash");
      assert_eq!(normalize_unicode("minus\u{2212}sign"), "minus-sign");
   }

   #[test]
   fn test_normalize_unicode_arrows() {
      assert_eq!(normalize_unicode("arrow\u{2192}right"), "arrow->right");
      assert_eq!(normalize_unicode("arrow\u{2190}left"), "arrow<-left");
      assert_eq!(normalize_unicode("arrow\u{2194}both"), "arrow<->both");
      assert_eq!(normalize_unicode("double\u{21D2}arrow"), "double=>arrow");
      assert_eq!(normalize_unicode("up\u{2191}arrow"), "up^arrow");
   }

   #[test]
   fn test_normalize_unicode_math() {
      assert_eq!(normalize_unicode("a\u{00D7}b"), "axb");
      assert_eq!(normalize_unicode("a\u{00F7}b"), "a/b");
      assert_eq!(normalize_unicode("x\u{2264}y"), "x<=y");
      assert_eq!(normalize_unicode("x\u{2265}y"), "x>=y");
      assert_eq!(normalize_unicode("x\u{2260}y"), "x!=y");
      assert_eq!(normalize_unicode("x\u{2248}y"), "x~=y");
   }

   #[test]
   fn test_normalize_unicode_greek() {
      assert_eq!(normalize_unicode("\u{03BB} function"), "lambda function");
      assert_eq!(normalize_unicode("\u{03B1} beta \u{03B3}"), "alpha beta gamma");
      assert_eq!(normalize_unicode("\u{03BC} service"), "mu service");
      assert_eq!(normalize_unicode("\u{03A3} total"), "Sigma total");
   }

   #[test]
   fn test_normalize_unicode_fractions() {
      assert_eq!(normalize_unicode("\u{00BD} cup"), "1/2 cup");
      assert_eq!(normalize_unicode("\u{00BE} done"), "3/4 done");
      assert_eq!(normalize_unicode("\u{2153} left"), "1/3 left");
   }

   #[test]
   fn test_normalize_unicode_superscripts() {
      assert_eq!(normalize_unicode("x\u{00B2}"), "x^2");
      assert_eq!(normalize_unicode("10\u{00B3}"), "10^3");
   }

   #[test]
   fn test_normalize_unicode_multiple_replacements() {
      let input =
         "\u{2018}smart\u{2019}\u{2192}straight \u{201C}quotes\u{201D}\u{00D7}math\u{2264}ops";
      let expected = "'smart'->straight \"quotes\"xmath<=ops";
      assert_eq!(normalize_unicode(input), expected);
   }

   #[test]
   fn test_normalize_unicode_ellipsis() {
      assert_eq!(normalize_unicode("wait\u{2026}"), "wait...");
      assert_eq!(normalize_unicode("more\u{22EF}dots"), "more...dots");
   }

   #[test]
   fn test_normalize_unicode_bullets() {
      assert_eq!(normalize_unicode("\u{2022}item"), "-item");
      assert_eq!(normalize_unicode("\u{25E6}item"), "-item");
   }

   #[test]
   fn test_normalize_unicode_check_marks() {
      assert_eq!(normalize_unicode("\u{2713}done"), "vdone");
      assert_eq!(normalize_unicode("\u{2717}failed"), "xfailed");
   }

   // normalize_summary_verb tests
   #[test]
   fn test_normalize_summary_verb_present_to_past() {
      let mut s = "add new feature".to_string();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "added new feature");

      let mut s = "fix bug".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "fixed bug");

      let mut s = "update docs".to_string();
      normalize_summary_verb(&mut s, "docs");
      assert_eq!(s, "updated docs");
   }

   #[test]
   fn test_normalize_summary_verb_already_past() {
      let mut s = "added feature".to_string();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "added feature");

      let mut s = "fixed bug".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "fixed bug");
   }

   #[test]
   fn test_normalize_summary_verb_third_person() {
      let mut s = "adds feature".to_string();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "added feature");

      let mut s = "fixes bug".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "fixed bug");
   }

   #[test]
   fn test_normalize_summary_verb_non_verb_start() {
      let mut s = "123 files changed".to_string();
      normalize_summary_verb(&mut s, "chore");
      assert_eq!(s, "123 files changed");
   }

   #[test]
   fn test_normalize_summary_verb_refactor_special_case() {
      let mut s = "refactored code".to_string();
      normalize_summary_verb(&mut s, "refactor");
      assert_eq!(s, "restructured code");
   }

   #[test]
   fn test_normalize_summary_verb_refactor_present() {
      let mut s = "refactor code".to_string();
      normalize_summary_verb(&mut s, "refactor");
      assert_eq!(s, "restructured code");

      let mut s = "refactor logic".to_string();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "refactored logic");
   }

   #[test]
   fn test_normalize_summary_verb_empty() {
      let mut s = String::new();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "");
   }

   #[test]
   fn test_normalize_summary_verb_single_word() {
      let mut s = "add".to_string();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "added");
   }

   #[test]
   fn test_normalize_summary_verb_harden_to_hardened() {
      let mut s = "harden stealth scripts against detection".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "hardened stealth scripts against detection");
   }

   #[test]
   fn test_normalize_summary_verb_bind_to_bound() {
      let mut s = "bind native methods to local constants".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "bound native methods to local constants");
   }

   #[test]
   fn test_normalize_summary_verb_third_person_ies() {
      // -ies -> -y conversion (simplifies -> simplify -> simplified)
      let mut s = "simplifies the config loading".to_string();
      normalize_summary_verb(&mut s, "refactor");
      assert_eq!(s, "simplified the config loading");
   }

   #[test]
   fn test_normalize_summary_verb_third_person_es() {
      // -es stripping (fixes -> fix -> fixed)
      let mut s = "fixes race condition".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "fixed race condition");
   }

   #[test]
   fn test_normalize_summary_verb_suffix_reattach_dash() {
      // Dash suffix should be reattached after conversion
      let mut s = "isolate-subagent from main flow".to_string();
      normalize_summary_verb(&mut s, "refactor");
      assert_eq!(s, "isolated-subagent from main flow");
   }

   #[test]
   fn test_normalize_summary_verb_skip_type_prefix_leak() {
      // `fix(tui):` is a leaked conventional prefix, NOT a verb to convert.
      // The `(tui):` suffix is not a dash/slash separator, so we skip.
      let mut s = "fix(tui): rendering bug".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "fix(tui): rendering bug");
   }

   #[test]
   fn test_normalize_summary_verb_skip_acronym() {
      // All-caps acronyms should not be converted
      let mut s = "API response handling".to_string();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "API response handling");
   }

   #[test]
   fn test_normalize_summary_verb_skip_numeric() {
      // Numeric-led tokens should not be converted
      let mut s = "403 error handling".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "403 error handling");
   }

   #[test]
   fn test_normalize_summary_verb_already_past_hardened() {
      // Already past tense should not be re-converted
      let mut s = "hardened stealth scripts".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "hardened stealth scripts");
   }

   #[test]
   fn test_normalize_summary_verb_already_past_bound() {
      let mut s = "bound native methods".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "bound native methods");
   }

   #[test]
   fn test_normalize_summary_verb_preserves_existing_third_person() {
      // The old test had "adds" and "fixes" - ensure they still work
      let mut s = "adds feature".to_string();
      normalize_summary_verb(&mut s, "feat");
      assert_eq!(s, "added feature");

      let mut s = "fixes bug".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "fixed bug");

      let mut s = "updates docs".to_string();
      normalize_summary_verb(&mut s, "docs");
      assert_eq!(s, "updated docs");
   }

   #[test]
   fn test_normalize_summary_verb_re_prefix_enable() {
      let mut s = "re-enable formatting checks".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "re-enabled formatting checks");
   }

   #[test]
   fn test_normalize_summary_verb_re_prefix_run() {
      let mut s = "re-run the test suite".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "re-ran the test suite");
   }

   #[test]
   fn test_normalize_summary_verb_re_prefix_with_tail() {
      // re-format-checking -> re-formatted-checking
      let mut s = "re-format-checking pipeline".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "re-formatted-checking pipeline");
   }

   #[test]
   fn test_normalize_summary_verb_re_prefix_already_past() {
      // re-enabled is already past tense, should not be re-converted
      let mut s = "re-enabled linting".to_string();
      normalize_summary_verb(&mut s, "fix");
      assert_eq!(s, "re-enabled linting");
   }

   // cap_details tests (budget-based)
   #[test]
   fn test_cap_details_under_budget() {
      let mut details = vec!["first".to_string(), "second".to_string(), "third".to_string()];
      let tokens: usize = details.iter().map(|d| estimate_tokens(d)).sum();
      cap_details(&mut details, tokens + 100);
      assert_eq!(details.len(), 3);
   }

   #[test]
   fn test_cap_details_at_budget() {
      let mut details = vec![
         "one".to_string(),
         "two".to_string(),
         "three".to_string(),
         "four".to_string(),
         "five".to_string(),
         "six".to_string(),
      ];
      let tokens: usize = details.iter().map(|d| estimate_tokens(d)).sum();
      cap_details(&mut details, tokens);
      assert_eq!(details.len(), 6);
   }

   #[test]
   fn test_cap_details_security_priority() {
      let mut details = vec![
         "normal change".to_string(),
         "security vulnerability fixed".to_string(),
         "another change".to_string(),
         "third change".to_string(),
         "fourth change".to_string(),
         "fifth change".to_string(),
         "sixth change".to_string(),
      ];
      // Budget for ~4 typical items (15 chars each = ~4 tokens, 4*4 = 16 tokens)
      cap_details(&mut details, 60);
      assert!(details.iter().any(|d| d.contains("security")));
   }

   #[test]
   fn test_cap_details_performance_priority() {
      let mut details = vec![
         "normal change".to_string(),
         "performance optimization added".to_string(),
         "another change".to_string(),
         "third change".to_string(),
         "fourth change".to_string(),
         "fifth change".to_string(),
      ];
      // Budget for ~3 typical items
      cap_details(&mut details, 40);
      assert!(details.iter().any(|d| d.contains("performance")));
   }

   #[test]
   fn test_cap_details_api_priority() {
      let mut details = vec![
         "normal change".to_string(),
         "API interface updated".to_string(),
         "internal change".to_string(),
         "another internal change".to_string(),
         "yet another change".to_string(),
      ];
      // Budget for ~3 items
      cap_details(&mut details, 50);
      assert!(details.iter().any(|d| d.contains("API")));
   }

   #[test]
   fn test_cap_details_preserves_order() {
      let mut details = vec![
         "first".to_string(),
         "critical security fix".to_string(),
         "third".to_string(),
         "performance improvement".to_string(),
         "fifth".to_string(),
      ];
      // Budget for ~3 items
      cap_details(&mut details, 50);
      // Should preserve relative order of kept items
      let security_idx = details.iter().position(|d| d.contains("security"));
      let perf_idx = details.iter().position(|d| d.contains("performance"));
      assert!(security_idx.unwrap() < perf_idx.unwrap());
   }

   #[test]
   fn test_cap_details_empty_list() {
      let mut details: Vec<String> = vec![];
      cap_details(&mut details, 100);
      assert_eq!(details.len(), 0);
   }

   #[test]
   fn test_cap_details_breaking_priority() {
      let mut details = vec![
         "normal change".to_string(),
         "breaking change introduced".to_string(),
         "another change".to_string(),
         "third change".to_string(),
         "fourth change".to_string(),
      ];
      // Budget for ~3 items
      cap_details(&mut details, 50);
      assert!(details.iter().any(|d| d.contains("breaking")));
   }

   #[test]
   fn test_cap_details_budget_prefers_short_high_priority() {
      // 6 short high-priority items should fit, but 2 long low-priority shouldn't
      let mut details = vec![
         "security fix".to_string(),     // ~12 chars, ~3 tokens, score 100
         "bug fix".to_string(),          // ~7 chars, ~2 tokens, score 70
         "API change".to_string(),       // ~10 chars, ~3 tokens, score 50
         "performance gain".to_string(), // ~16 chars, ~4 tokens, score 80
         "breaking change".to_string(),  // ~15 chars, ~4 tokens, score 90
         "user feature".to_string(),     // ~12 chars, ~3 tokens, score 40
         "This is a very long internal refactoring detail that adds no user value".to_string(), /* ~73 chars, ~19 tokens, score 0 */
         "Another extremely long low priority change description here".to_string(), /* ~61 chars, ~16 tokens, score 0 */
      ];
      // Budget: 30 tokens (enough for all 6 short items, not enough for long ones)
      cap_details(&mut details, 30);
      // Should keep short high-priority items
      assert!(details.iter().any(|d| d.contains("security")));
      assert!(details.iter().any(|d| d.contains("breaking")));
      // Should drop long low-priority items
      assert!(!details.iter().any(|d| d.contains("very long internal")));
   }

   #[test]
   fn test_cap_details_budget_allows_variable_count() {
      // With same budget, should fit more short items or fewer long items
      let short_details = vec![
         "fix A".to_string(),
         "fix B".to_string(),
         "fix C".to_string(),
         "fix D".to_string(),
         "fix E".to_string(),
         "fix F".to_string(),
      ];
      let long_details = vec![
         "Fixed a critical security vulnerability in authentication".to_string(),
         "Implemented comprehensive performance optimization".to_string(),
         "Added extensive API documentation and examples".to_string(),
      ];

      let mut short = short_details;
      let mut long = long_details;

      cap_details(&mut short, 50); // Should fit all 6 short items (~2 tokens each)
      cap_details(&mut long, 50); // Should fit only 2-3 long items (~13-15 tokens each)

      assert!(short.len() >= 5); // Most short items fit
      assert!(long.len() <= 3); // Fewer long items fit
   }

   // format_commit_message tests
   #[test]
   fn test_format_commit_message_type_summary_only() {
      let commit = ConventionalCommit {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       None,
         summary:     CommitSummary::new_unchecked("added new feature", 128).unwrap(),
         body:        vec![],
         footers:     vec![],
      };
      assert_eq!(format_commit_message(&commit), "feat: added new feature");
   }

   #[test]
   fn test_format_commit_message_with_scope() {
      let commit = ConventionalCommit {
         commit_type: CommitType::new("fix").unwrap(),
         scope:       Some(Scope::new("api").unwrap()),
         summary:     CommitSummary::new_unchecked("fixed bug", 128).unwrap(),
         body:        vec![],
         footers:     vec![],
      };
      assert_eq!(format_commit_message(&commit), "fix(api): fixed bug");
   }

   #[test]
   fn test_format_commit_message_with_body() {
      let commit = ConventionalCommit {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       None,
         summary:     CommitSummary::new_unchecked("added feature", 128).unwrap(),
         body:        vec!["First detail.".to_string(), "Second detail.".to_string()],
         footers:     vec![],
      };
      let expected = "feat: added feature\n\n- First detail.\n- Second detail.";
      assert_eq!(format_commit_message(&commit), expected);
   }

   #[test]
   fn test_format_commit_message_with_footers() {
      let commit = ConventionalCommit {
         commit_type: CommitType::new("fix").unwrap(),
         scope:       None,
         summary:     CommitSummary::new_unchecked("fixed bug", 128).unwrap(),
         body:        vec![],
         footers:     vec!["Closes: #123".to_string(), "Fixes: #456".to_string()],
      };
      let expected = "fix: fixed bug\n\nCloses: #123\nFixes: #456";
      assert_eq!(format_commit_message(&commit), expected);
   }

   #[test]
   fn test_format_commit_message_full() {
      let commit = ConventionalCommit {
         commit_type: CommitType::new("feat").unwrap(),
         scope:       Some(Scope::new("auth").unwrap()),
         summary:     CommitSummary::new_unchecked("added oauth support", 128).unwrap(),
         body:        vec![
            "Implemented OAuth2 flow.".to_string(),
            "Added token refresh.".to_string(),
         ],
         footers:     vec!["Closes: #789".to_string()],
      };
      let expected = "feat(auth): added oauth support\n\n- Implemented OAuth2 flow.\n- Added \
                      token refresh.\n\nCloses: #789";
      assert_eq!(format_commit_message(&commit), expected);
   }

   #[test]
   fn test_format_commit_message_nested_scope() {
      let commit = ConventionalCommit {
         commit_type: CommitType::new("refactor").unwrap(),
         scope:       Some(Scope::new("api/client").unwrap()),
         summary:     CommitSummary::new_unchecked("restructured code", 128).unwrap(),
         body:        vec![],
         footers:     vec![],
      };
      assert_eq!(format_commit_message(&commit), "refactor(api/client): restructured code");
   }
}
