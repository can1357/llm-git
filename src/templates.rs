use std::{
   path::{Path, PathBuf},
   sync::LazyLock,
};

use parking_lot::Mutex;
use rust_embed::RustEmbed;
use tera::{Context, Tera};

use crate::error::{CommitGenError, Result};

/// Rendered prompt split into system and user parts.
pub struct PromptParts {
   pub system: String,
   pub user:   String,
}

/// Split a rendered prompt into system and user parts based on separator line.
fn split_prompt_parts(rendered: &str) -> PromptParts {
   const SEPARATOR: &str = "\n======USER=======\n";
   if let Some(pos) = rendered.find(SEPARATOR) {
      PromptParts {
         system: rendered[..pos].trim().to_string(),
         user:   rendered[pos + SEPARATOR.len()..].trim().to_string(),
      }
   } else {
      // No separator - treat entire content as user message
      PromptParts { system: String::new(), user: rendered.trim().to_string() }
   }
}

/// Parameters for rendering the analysis prompt template.
#[derive(Default)]
pub struct AnalysisParams<'a> {
   pub variant:           &'a str,
   pub stat:              &'a str,
   pub diff:              &'a str,
   pub scope_candidates:  &'a str,
   pub recent_commits:    Option<&'a str>,
   pub common_scopes:     Option<&'a str>,
   pub types_description: Option<&'a str>,
   pub project_context:   Option<&'a str>,
}

/// Embedded prompts folder (compiled into binary)
#[derive(RustEmbed)]
#[folder = "prompts/"]
struct Prompts;

/// Global Tera instance for template rendering (wrapped in Mutex for mutable
/// access)
static TERA: LazyLock<Mutex<Tera>> = LazyLock::new(|| {
   // Ensure prompts are initialized
   if let Err(e) = ensure_prompts_dir() {
      eprintln!("Warning: Failed to initialize prompts directory: {e}");
   }

   let mut tera = Tera::default();

   // Load templates from user prompts directory first so they take precedence.
   if let Some(prompts_dir) = get_user_prompts_dir() {
      if let Err(e) =
         register_directory_templates(&mut tera, &prompts_dir.join("analysis"), "analysis")
      {
         eprintln!("Warning: {e}");
      }
      if let Err(e) =
         register_directory_templates(&mut tera, &prompts_dir.join("summary"), "summary")
      {
         eprintln!("Warning: {e}");
      }
      if let Err(e) =
         register_directory_templates(&mut tera, &prompts_dir.join("changelog"), "changelog")
      {
         eprintln!("Warning: {e}");
      }
      if let Err(e) = register_directory_templates(&mut tera, &prompts_dir.join("map"), "map") {
         eprintln!("Warning: {e}");
      }
      if let Err(e) = register_directory_templates(&mut tera, &prompts_dir.join("reduce"), "reduce")
      {
         eprintln!("Warning: {e}");
      }
   }

   // Register embedded templates that aren't overridden by user-provided files.
   for file in Prompts::iter() {
      if tera.get_template_names().any(|name| name == file.as_ref()) {
         continue;
      }

      if let Some(embedded_file) = Prompts::get(file.as_ref()) {
         match std::str::from_utf8(embedded_file.data.as_ref()) {
            Ok(content) => {
               if let Err(e) = tera.add_raw_template(file.as_ref(), content) {
                  eprintln!(
                     "Warning: Failed to register embedded template {}: {}",
                     file.as_ref(),
                     e
                  );
               }
            },
            Err(e) => {
               eprintln!("Warning: Embedded template {} is not valid UTF-8: {}", file.as_ref(), e);
            },
         }
      }
   }

   // Disable auto-escaping for markdown files
   tera.autoescape_on(vec![]);

   Mutex::new(tera)
});

/// Determine user prompts directory (~/.llm-git/prompts/) if a home dir exists.
fn get_user_prompts_dir() -> Option<PathBuf> {
   std::env::var("HOME")
      .or_else(|_| std::env::var("USERPROFILE"))
      .ok()
      .map(|home| PathBuf::from(home).join(".llm-git").join("prompts"))
}

/// Initialize prompts directory by unpacking embedded prompts if needed
pub fn ensure_prompts_dir() -> Result<()> {
   let Some(user_prompts_dir) = get_user_prompts_dir() else {
      // No HOME/USERPROFILE, so we can't materialize templates on disk.
      // We'll fall back to the embedded prompts in-memory.
      return Ok(());
   };

   // Safety: prompts dir always has a parent (…/.llm-git/prompts)
   let user_llm_git_dir = user_prompts_dir
      .parent()
      .ok_or_else(|| CommitGenError::Other("Invalid prompts directory path".to_string()))?;

   // Create ~/.llm-git directory if it doesn't exist
   if !user_llm_git_dir.exists() {
      std::fs::create_dir_all(user_llm_git_dir).map_err(|e| {
         CommitGenError::Other(format!(
            "Failed to create directory {}: {}",
            user_llm_git_dir.display(),
            e
         ))
      })?;
   }

   // Create prompts subdirectory if it doesn't exist
   if !user_prompts_dir.exists() {
      std::fs::create_dir_all(&user_prompts_dir).map_err(|e| {
         CommitGenError::Other(format!(
            "Failed to create directory {}: {}",
            user_prompts_dir.display(),
            e
         ))
      })?;
   }

   // Unpack embedded prompts, updating if content differs
   for file in Prompts::iter() {
      let file_path = user_prompts_dir.join(file.as_ref());

      // Create parent directories if needed
      if let Some(parent) = file_path.parent() {
         std::fs::create_dir_all(parent).map_err(|e| {
            CommitGenError::Other(format!("Failed to create directory {}: {}", parent.display(), e))
         })?;
      }

      if let Some(embedded_file) = Prompts::get(file.as_ref()) {
         let embedded_content = embedded_file.data;

         // Check if we need to write: file doesn't exist OR content differs
         let should_write = if file_path.exists() {
            match std::fs::read(&file_path) {
               Ok(existing_content) => existing_content != embedded_content.as_ref(),
               Err(_) => true, // Can't read, assume we should write
            }
         } else {
            true // File doesn't exist
         };

         if should_write {
            std::fs::write(&file_path, embedded_content.as_ref()).map_err(|e| {
               CommitGenError::Other(format!("Failed to write file {}: {}", file_path.display(), e))
            })?;
         }
      }
   }

   Ok(())
}

fn register_directory_templates(tera: &mut Tera, directory: &Path, category: &str) -> Result<()> {
   if !directory.exists() {
      return Ok(());
   }

   for entry in std::fs::read_dir(directory).map_err(|e| {
      CommitGenError::Other(format!(
         "Failed to read {} templates directory {}: {}",
         category,
         directory.display(),
         e
      ))
   })? {
      let entry = match entry {
         Ok(entry) => entry,
         Err(e) => {
            eprintln!(
               "Warning: Failed to iterate template entry in {}: {}",
               directory.display(),
               e
            );
            continue;
         },
      };

      let path = entry.path();
      if path.extension().and_then(|s| s.to_str()) != Some("md") {
         continue;
      }

      let template_name = format!(
         "{}/{}",
         category,
         path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
      );

      // Add template (overwrites if exists, allowing user files to override embedded
      // defaults)
      if let Err(e) = tera.add_template_file(&path, Some(&template_name)) {
         eprintln!("Warning: Failed to load template file {}: {}", path.display(), e);
      }
   }

   Ok(())
}

/// Load template content from file (for dynamic user templates)
fn load_template_file(category: &str, variant: &str) -> Result<String> {
   // Prefer user-provided template if available.
   if let Some(prompts_dir) = get_user_prompts_dir() {
      let template_path = prompts_dir.join(category).join(format!("{variant}.md"));
      if template_path.exists() {
         return std::fs::read_to_string(&template_path).map_err(|e| {
            CommitGenError::Other(format!(
               "Failed to read template file {}: {}",
               template_path.display(),
               e
            ))
         });
      }
   }

   // Fallback to embedded template bundled with the binary.
   let embedded_key = format!("{category}/{variant}.md");
   if let Some(bytes) = Prompts::get(&embedded_key) {
      return std::str::from_utf8(bytes.data.as_ref())
         .map(|s| s.to_string())
         .map_err(|e| {
            CommitGenError::Other(format!(
               "Embedded template {embedded_key} is not valid UTF-8: {e}"
            ))
         });
   }

   Err(CommitGenError::Other(format!(
      "Template variant '{variant}' in category '{category}' not found as user override or \
       embedded default"
   )))
}

/// Render analysis prompt template
pub fn render_analysis_prompt(p: &AnalysisParams<'_>) -> Result<PromptParts> {
   // Try to load template dynamically (supports user-added templates)
   let template_content = load_template_file("analysis", p.variant)?;

   // Create context with all the data
   let mut context = Context::new();
   context.insert("stat", p.stat);
   context.insert("diff", p.diff);
   context.insert("scope_candidates", p.scope_candidates);
   if let Some(commits) = p.recent_commits {
      context.insert("recent_commits", commits);
   }
   if let Some(scopes) = p.common_scopes {
      context.insert("common_scopes", scopes);
   }
   if let Some(types) = p.types_description {
      context.insert("types_description", types);
   }
   if let Some(ctx) = p.project_context {
      context.insert("project_context", ctx);
   }

   // Render using render_str for dynamic templates
   let mut tera = TERA.lock();

   let rendered = tera.render_str(&template_content, &context).map_err(|e| {
      CommitGenError::Other(format!(
         "Failed to render analysis prompt template '{}': {e}",
         p.variant
      ))
   })?;
   Ok(split_prompt_parts(&rendered))
}

/// Render summary prompt template
pub fn render_summary_prompt(
   variant: &str,
   commit_type: &str,
   scope: &str,
   chars: &str,
   details: &str,
   stat: &str,
   user_context: Option<&str>,
) -> Result<PromptParts> {
   // Try to load template dynamically (supports user-added templates)
   let template_content = load_template_file("summary", variant)?;

   // Create context with all the data
   let mut context = Context::new();
   context.insert("commit_type", commit_type);
   context.insert("scope", scope);
   context.insert("chars", chars);
   context.insert("details", details);
   context.insert("stat", stat);
   if let Some(ctx) = user_context {
      context.insert("user_context", ctx);
   }

   // Render using render_str for dynamic templates
   let mut tera = TERA.lock();
   let rendered = tera.render_str(&template_content, &context).map_err(|e| {
      CommitGenError::Other(format!("Failed to render summary prompt template '{variant}': {e}"))
   })?;
   Ok(split_prompt_parts(&rendered))
}

/// Render changelog prompt template
pub fn render_changelog_prompt(
   variant: &str,
   changelog_path: &str,
   is_package_changelog: bool,
   stat: &str,
   diff: &str,
   existing_entries: Option<&str>,
) -> Result<PromptParts> {
   // Try to load template dynamically (supports user-added templates)
   let template_content = load_template_file("changelog", variant)?;

   // Create context with all the data
   let mut context = Context::new();
   context.insert("changelog_path", changelog_path);
   context.insert("is_package_changelog", &is_package_changelog);
   context.insert("stat", stat);
   context.insert("diff", diff);
   if let Some(entries) = existing_entries {
      context.insert("existing_entries", entries);
   }

   // Render using render_str for dynamic templates
   let mut tera = TERA.lock();
   let rendered = tera.render_str(&template_content, &context).map_err(|e| {
      CommitGenError::Other(format!("Failed to render changelog prompt template '{variant}': {e}"))
   })?;
   Ok(split_prompt_parts(&rendered))
}

/// Render map prompt template (per-file observation extraction)
pub fn render_map_prompt(
   variant: &str,
   filename: &str,
   diff: &str,
   context_header: &str,
) -> Result<PromptParts> {
   let template_content = load_template_file("map", variant)?;

   let mut context = Context::new();
   context.insert("filename", filename);
   context.insert("diff", diff);
   if !context_header.is_empty() {
      context.insert("context_header", context_header);
   }

   let mut tera = TERA.lock();
   let rendered = tera.render_str(&template_content, &context).map_err(|e| {
      CommitGenError::Other(format!("Failed to render map prompt template '{variant}': {e}"))
   })?;
   Ok(split_prompt_parts(&rendered))
}

/// Render reduce prompt template (synthesis from observations)
pub fn render_reduce_prompt(
   variant: &str,
   observations: &str,
   stat: &str,
   scope_candidates: &str,
   types_description: Option<&str>,
) -> Result<PromptParts> {
   let template_content = load_template_file("reduce", variant)?;

   let mut context = Context::new();
   context.insert("observations", observations);
   context.insert("stat", stat);
   context.insert("scope_candidates", scope_candidates);
   if let Some(types_desc) = types_description {
      context.insert("types_description", types_desc);
   }

   let mut tera = TERA.lock();
   let rendered = tera.render_str(&template_content, &context).map_err(|e| {
      CommitGenError::Other(format!("Failed to render reduce prompt template '{variant}': {e}"))
   })?;
   Ok(split_prompt_parts(&rendered))
}
