//! Repository metadata detection
//!
//! Detects project type, language, and framework from manifest files
//! to provide context for commit message generation.

use std::path::Path;

/// Detected repository metadata
#[derive(Debug, Clone, Default)]
pub struct RepoMetadata {
   /// Primary programming language
   pub language: Option<String>,
   /// Web/backend framework if detected
   pub framework: Option<String>,
   /// Package manager used
   pub package_manager: Option<String>,
   /// Whether this is a monorepo/workspace
   pub is_monorepo: bool,
   /// Number of packages/crates (for monorepos)
   pub package_count: Option<usize>,
}

impl RepoMetadata {
   /// Detect repository metadata from the given directory
   pub fn detect(dir: &Path) -> Self {
      let mut meta = Self::default();

      // Check for Rust project
      if let Some(rust_meta) = detect_rust(dir) {
         meta = rust_meta;
      }
      // Check for Node.js/TypeScript project
      else if let Some(node_meta) = detect_node(dir) {
         meta = node_meta;
      }
      // Check for Python project
      else if let Some(python_meta) = detect_python(dir) {
         meta = python_meta;
      }
      // Check for Go project
      else if detect_go(dir) {
         meta.language = Some("Go".to_string());
         meta.package_manager = Some("go mod".to_string());
      }

      meta
   }

   /// Format metadata for prompt injection
   pub fn format_for_prompt(&self) -> Option<String> {
      self.language.as_ref()?;

      let mut lines = Vec::new();

      // Language line
      if let Some(ref lang) = self.language {
         let mut lang_str = lang.clone();
         if self.is_monorepo {
            if let Some(count) = self.package_count {
               lang_str = format!("{lang} (workspace, {count} packages)");
            } else {
               lang_str = format!("{lang} (workspace)");
            }
         }
         lines.push(format!("Language: {lang_str}"));
      }

      // Framework line
      if let Some(ref framework) = self.framework {
         lines.push(format!("Framework: {framework}"));
      }

      if lines.is_empty() {
         None
      } else {
         Some(lines.join("\n"))
      }
   }
}

/// Detect Rust project metadata
fn detect_rust(dir: &Path) -> Option<RepoMetadata> {
   let cargo_toml = dir.join("Cargo.toml");
   if !cargo_toml.exists() {
      return None;
   }

   let content = std::fs::read_to_string(&cargo_toml).ok()?;
   let mut meta = RepoMetadata {
      language:        Some("Rust".to_string()),
      package_manager: Some("cargo".to_string()),
      ..Default::default()
   };

   // Check for workspace
   if content.contains("[workspace]") {
      meta.is_monorepo = true;

      // Count workspace members
      if let Some(members_start) = content.find("members")
         && let Some(bracket_start) = content[members_start..].find('[') {
            let rest = &content[members_start + bracket_start..];
            if let Some(bracket_end) = rest.find(']') {
               let members_str = &rest[1..bracket_end];
               meta.package_count = Some(members_str.matches('"').count() / 2);
            }
         }
   }

   // Detect framework from dependencies
   let framework = detect_rust_framework(&content);
   if framework.is_some() {
      meta.framework = framework;
   }

   Some(meta)
}

/// Detect Rust framework from Cargo.toml dependencies
fn detect_rust_framework(content: &str) -> Option<String> {
   // Check for common web frameworks (order matters - first match wins)
   let frameworks = [
      ("axum", "Axum"),
      ("actix-web", "Actix Web"),
      ("rocket", "Rocket"),
      ("warp", "Warp"),
      ("tide", "Tide"),
      ("poem", "Poem"),
      ("tower-http", "Tower HTTP"),
      ("hyper", "Hyper"),
      ("tokio", "Tokio async runtime"),
      ("bevy", "Bevy game engine"),
      ("iced", "Iced GUI"),
      ("egui", "egui GUI"),
      ("tauri", "Tauri"),
      ("leptos", "Leptos"),
      ("yew", "Yew"),
      ("dioxus", "Dioxus"),
   ];

   for (dep, name) in frameworks {
      // Match "dep_name" or "dep-name" in dependencies
      if content.contains(&format!("\"{dep}\"")) || content.contains(&format!("{dep} =")) {
         return Some(name.to_string());
      }
   }

   None
}

/// Detect Node.js/TypeScript project metadata
fn detect_node(dir: &Path) -> Option<RepoMetadata> {
   let package_json = dir.join("package.json");
   if !package_json.exists() {
      return None;
   }

   let content = std::fs::read_to_string(&package_json).ok()?;

   // Determine if TypeScript
   let is_typescript =
      content.contains("\"typescript\"") || dir.join("tsconfig.json").exists();

   let language = if is_typescript { "TypeScript" } else { "JavaScript" };

   let mut meta = RepoMetadata {
      language: Some(language.to_string()),
      ..Default::default()
   };

   // Detect package manager
   if dir.join("pnpm-lock.yaml").exists() {
      meta.package_manager = Some("pnpm".to_string());
   } else if dir.join("yarn.lock").exists() {
      meta.package_manager = Some("yarn".to_string());
   } else if dir.join("bun.lockb").exists() {
      meta.package_manager = Some("bun".to_string());
   } else {
      meta.package_manager = Some("npm".to_string());
   }

   // Check for workspaces
   if content.contains("\"workspaces\"") {
      meta.is_monorepo = true;
   } else if dir.join("pnpm-workspace.yaml").exists() {
      meta.is_monorepo = true;
   }

   // Detect framework
   let framework = detect_node_framework(&content);
   if framework.is_some() {
      meta.framework = framework;
   }

   Some(meta)
}

/// Detect Node.js framework from package.json
fn detect_node_framework(content: &str) -> Option<String> {
   let frameworks = [
      ("next", "Next.js"),
      ("nuxt", "Nuxt"),
      ("@angular/core", "Angular"),
      ("vue", "Vue"),
      ("react", "React"),
      ("svelte", "Svelte"),
      ("solid-js", "SolidJS"),
      ("express", "Express"),
      ("fastify", "Fastify"),
      ("hono", "Hono"),
      ("nestjs", "NestJS"),
      ("@nestjs/core", "NestJS"),
      ("electron", "Electron"),
      ("expo", "Expo"),
      ("react-native", "React Native"),
   ];

   for (dep, name) in frameworks {
      if content.contains(&format!("\"{dep}\"")) {
         return Some(name.to_string());
      }
   }

   None
}

/// Detect Python project metadata
fn detect_python(dir: &Path) -> Option<RepoMetadata> {
   let pyproject = dir.join("pyproject.toml");
   let setup_py = dir.join("setup.py");
   let requirements = dir.join("requirements.txt");

   if !pyproject.exists() && !setup_py.exists() && !requirements.exists() {
      return None;
   }

   let mut meta = RepoMetadata {
      language: Some("Python".to_string()),
      ..Default::default()
   };

   // Detect package manager
   if pyproject.exists() {
      let content = std::fs::read_to_string(&pyproject).unwrap_or_default();
      if content.contains("[tool.poetry]") {
         meta.package_manager = Some("poetry".to_string());
      } else if content.contains("[tool.uv]") || dir.join("uv.lock").exists() {
         meta.package_manager = Some("uv".to_string());
      } else if content.contains("[tool.pdm]") {
         meta.package_manager = Some("pdm".to_string());
      } else {
         meta.package_manager = Some("pip".to_string());
      }

      // Detect framework
      meta.framework = detect_python_framework(&content);
   } else {
      meta.package_manager = Some("pip".to_string());
   }

   Some(meta)
}

/// Detect Python framework from pyproject.toml
fn detect_python_framework(content: &str) -> Option<String> {
   let frameworks = [
      ("fastapi", "FastAPI"),
      ("django", "Django"),
      ("flask", "Flask"),
      ("starlette", "Starlette"),
      ("litestar", "Litestar"),
      ("sanic", "Sanic"),
      ("tornado", "Tornado"),
      ("aiohttp", "aiohttp"),
      ("pytorch", "PyTorch"),
      ("torch", "PyTorch"),
      ("tensorflow", "TensorFlow"),
      ("jax", "JAX"),
      ("transformers", "Hugging Face"),
   ];

   for (dep, name) in frameworks {
      if content.to_lowercase().contains(dep) {
         return Some(name.to_string());
      }
   }

   None
}

/// Check if directory is a Go project
fn detect_go(dir: &Path) -> bool {
   dir.join("go.mod").exists()
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn test_format_for_prompt_empty() {
      let meta = RepoMetadata::default();
      assert!(meta.format_for_prompt().is_none());
   }

   #[test]
   fn test_format_for_prompt_rust() {
      let meta = RepoMetadata {
         language:        Some("Rust".to_string()),
         framework:       Some("Axum".to_string()),
         package_manager: Some("cargo".to_string()),
         is_monorepo:     true,
         package_count:   Some(5),
      };

      let formatted = meta.format_for_prompt().unwrap();
      assert!(formatted.contains("Rust (workspace, 5 packages)"));
      assert!(formatted.contains("Framework: Axum"));
   }
}
