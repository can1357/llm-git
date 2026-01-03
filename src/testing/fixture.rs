//! Fixture types and I/O operations

use std::{collections::HashMap, fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
   error::{CommitGenError, Result},
   types::ConventionalAnalysis,
};

/// Manifest listing all fixtures
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
   /// Map of fixture name to metadata
   #[serde(default)]
   pub fixtures: HashMap<String, FixtureEntry>,
}

/// Entry in the manifest for a single fixture
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureEntry {
   /// Brief description of what this fixture tests
   pub description: String,
   /// Tags for filtering (e.g., "large", "map-reduce", "edge-case")
   #[serde(default)]
   pub tags: Vec<String>,
}

impl Manifest {
   /// Load manifest from fixtures directory
   pub fn load(fixtures_dir: &Path) -> Result<Self> {
      let path = fixtures_dir.join("manifest.toml");
      if !path.exists() {
         return Ok(Self { fixtures: HashMap::new() });
      }
      let content = fs::read_to_string(&path)?;
      toml::from_str(&content).map_err(|e| {
         CommitGenError::Other(format!("Failed to parse manifest.toml: {e}"))
      })
   }

   /// Save manifest to fixtures directory
   pub fn save(&self, fixtures_dir: &Path) -> Result<()> {
      let path = fixtures_dir.join("manifest.toml");
      let content = toml::to_string_pretty(self).map_err(|e| {
         CommitGenError::Other(format!("Failed to serialize manifest: {e}"))
      })?;
      fs::write(&path, content)?;
      Ok(())
   }

   /// Add a new fixture entry
   pub fn add(&mut self, name: String, entry: FixtureEntry) {
      self.fixtures.insert(name, entry);
   }
}

/// Metadata for a fixture
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureMeta {
   /// Source repository (e.g., "tetra")
   pub source_repo: String,
   /// Original commit hash
   pub source_commit: String,
   /// Why this fixture is interesting
   pub description: String,
   /// When this fixture was captured
   pub captured_at: String,
   /// Tags for categorization
   #[serde(default)]
   pub tags: Vec<String>,
}

/// Context captured for analysis (replaces live git queries)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FixtureContext {
   /// Style patterns from recent commits
   #[serde(default)]
   pub recent_commits: Option<String>,
   /// Common scopes in repository
   #[serde(default)]
   pub common_scopes: Option<String>,
   /// Project metadata
   #[serde(default)]
   pub project_context: Option<String>,
   /// User-provided context
   #[serde(default)]
   pub user_context: Option<String>,
}

/// Input data for a fixture
#[derive(Debug, Clone)]
pub struct FixtureInput {
   /// The diff content
   pub diff: String,
   /// The stat content
   pub stat: String,
   /// Pre-computed scope candidates
   pub scope_candidates: String,
   /// Analysis context
   pub context: FixtureContext,
}

/// Golden (expected) output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Golden {
   /// Expected analysis result
   pub analysis: ConventionalAnalysis,
   /// Expected final commit message
   pub final_message: String,
}

/// A complete fixture with all data
#[derive(Debug, Clone)]
pub struct Fixture {
   /// Fixture name (directory name)
   pub name: String,
   /// Fixture metadata
   pub meta: FixtureMeta,
   /// Input data
   pub input: FixtureInput,
   /// Golden output (None if not yet generated)
   pub golden: Option<Golden>,
}

impl Fixture {
   /// Load a fixture from disk
   pub fn load(fixtures_dir: &Path, name: &str) -> Result<Self> {
      let fixture_dir = fixtures_dir.join(name);
      if !fixture_dir.exists() {
         return Err(CommitGenError::Other(format!(
            "Fixture '{}' not found at {}",
            name,
            fixture_dir.display()
         )));
      }

      // Load metadata
      let meta_path = fixture_dir.join("meta.toml");
      let meta: FixtureMeta = if meta_path.exists() {
         let content = fs::read_to_string(&meta_path)?;
         toml::from_str(&content).map_err(|e| {
            CommitGenError::Other(format!("Failed to parse {}: {e}", meta_path.display()))
         })?
      } else {
         return Err(CommitGenError::Other(format!(
            "Fixture '{}' missing meta.toml",
            name
         )));
      };

      // Load input files
      let input_dir = fixture_dir.join("input");
      let diff = fs::read_to_string(input_dir.join("diff.patch"))
         .map_err(|e| CommitGenError::Other(format!("Failed to read diff.patch: {e}")))?;
      let stat = fs::read_to_string(input_dir.join("stat.txt"))
         .map_err(|e| CommitGenError::Other(format!("Failed to read stat.txt: {e}")))?;
      let scope_candidates =
         fs::read_to_string(input_dir.join("scope_candidates.txt")).unwrap_or_default();

      // Load context
      let context_path = input_dir.join("context.toml");
      let context: FixtureContext = if context_path.exists() {
         let content = fs::read_to_string(&context_path)?;
         toml::from_str(&content).map_err(|e| {
            CommitGenError::Other(format!("Failed to parse context.toml: {e}"))
         })?
      } else {
         FixtureContext::default()
      };

      // Load golden output if it exists
      let golden_dir = fixture_dir.join("golden");
      let golden = if golden_dir.exists() {
         let analysis_path = golden_dir.join("analysis.json");
         let final_path = golden_dir.join("final.txt");

         if analysis_path.exists() && final_path.exists() {
            let analysis_content = fs::read_to_string(&analysis_path)?;
            let analysis: ConventionalAnalysis = serde_json::from_str(&analysis_content)
               .map_err(|e| {
                  CommitGenError::Other(format!("Failed to parse analysis.json: {e}"))
               })?;
            let final_message = fs::read_to_string(&final_path)?;
            Some(Golden { analysis, final_message })
         } else {
            None
         }
      } else {
         None
      };

      Ok(Self {
         name: name.to_string(),
         meta,
         input: FixtureInput { diff, stat, scope_candidates, context },
         golden,
      })
   }

   /// Save a fixture to disk
   pub fn save(&self, fixtures_dir: &Path) -> Result<()> {
      let fixture_dir = fixtures_dir.join(&self.name);
      let input_dir = fixture_dir.join("input");
      let golden_dir = fixture_dir.join("golden");

      // Create directories
      fs::create_dir_all(&input_dir)?;
      fs::create_dir_all(&golden_dir)?;

      // Save metadata
      let meta_content = toml::to_string_pretty(&self.meta).map_err(|e| {
         CommitGenError::Other(format!("Failed to serialize meta: {e}"))
      })?;
      fs::write(fixture_dir.join("meta.toml"), meta_content)?;

      // Save input files
      fs::write(input_dir.join("diff.patch"), &self.input.diff)?;
      fs::write(input_dir.join("stat.txt"), &self.input.stat)?;
      fs::write(input_dir.join("scope_candidates.txt"), &self.input.scope_candidates)?;

      let context_content = toml::to_string_pretty(&self.input.context).map_err(|e| {
         CommitGenError::Other(format!("Failed to serialize context: {e}"))
      })?;
      fs::write(input_dir.join("context.toml"), context_content)?;

      // Save golden output if present
      if let Some(ref golden) = self.golden {
         let analysis_json = serde_json::to_string_pretty(&golden.analysis)?;
         fs::write(golden_dir.join("analysis.json"), analysis_json)?;
         fs::write(golden_dir.join("final.txt"), &golden.final_message)?;
      }

      Ok(())
   }

   /// Update golden output
   pub fn update_golden(&mut self, analysis: ConventionalAnalysis, final_message: String) {
      self.golden = Some(Golden { analysis, final_message });
   }
}

/// Discover all fixtures in a directory
pub fn discover_fixtures(fixtures_dir: &Path) -> Result<Vec<String>> {
   let mut fixtures = Vec::new();

   if !fixtures_dir.exists() {
      return Ok(fixtures);
   }

   for entry in fs::read_dir(fixtures_dir)? {
      let entry = entry?;
      let path = entry.path();

      // Skip manifest.toml and non-directories
      if !path.is_dir() {
         continue;
      }

      // Check if it has meta.toml (valid fixture)
      if path.join("meta.toml").exists() {
         if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            fixtures.push(name.to_string());
         }
      }
   }

   fixtures.sort();
   Ok(fixtures)
}
