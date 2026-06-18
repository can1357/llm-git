use std::{
   collections::HashMap,
   fs,
   io::Write,
   path::{Path, PathBuf},
   process::{Command, Stdio},
   sync::OnceLock,
   time::{SystemTime, UNIX_EPOCH},
};

pub use self::git_push as push;
use crate::{
   config::CommitConfig,
   error::{CommitGenError, Result},
   style,
   types::{CommitMetadata, Mode},
};

#[derive(Debug, Clone, Copy)]
struct GitCommandSettings {
   disable_git_background_features: bool,
}

impl Default for GitCommandSettings {
   fn default() -> Self {
      Self { disable_git_background_features: true }
   }
}

static GIT_COMMAND_SETTINGS: OnceLock<GitCommandSettings> = OnceLock::new();

pub fn init_git_command_settings(config: &CommitConfig) {
   let _ = GIT_COMMAND_SETTINGS.set(GitCommandSettings {
      disable_git_background_features: config.disable_git_background_features,
   });
}

fn current_git_command_settings() -> GitCommandSettings {
   GIT_COMMAND_SETTINGS.get().copied().unwrap_or_default()
}

fn apply_git_command_overrides(cmd: &mut Command, settings: GitCommandSettings) {
   if settings.disable_git_background_features {
      cmd.args(["-c", "core.fsmonitor=false", "-c", "core.untrackedCache=false"]);
   }
}

pub fn git_command() -> Command {
   git_command_with_settings(current_git_command_settings())
}

/// A temporary Git index file under `.git/llm-git/`.
///
/// The file is removed on drop, along with Git's sibling lock file if one was
/// left behind by an interrupted command.
pub struct TempGitIndex {
   path: PathBuf,
}

impl TempGitIndex {
   pub fn new(dir: &str) -> Result<Self> {
      let temp_dir = get_git_dir(dir)?.join("llm-git");
      fs::create_dir_all(&temp_dir).map_err(|e| {
         CommitGenError::git(format!("Failed to create temporary git index directory: {e}"))
      })?;

      let pid = std::process::id();
      let nanos = SystemTime::now()
         .duration_since(UNIX_EPOCH)
         .map_or(0, |duration| duration.as_nanos());

      for attempt in 0..100_u32 {
         let path = temp_dir.join(format!("index-{pid}-{nanos}-{attempt}"));
         match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
         {
            Ok(_) => {
               let _ = fs::remove_file(&path);
               return Ok(Self { path });
            },
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {},
            Err(err) => {
               return Err(CommitGenError::git(format!(
                  "Failed to create temporary git index: {err}"
               )));
            },
         }
      }

      Err(CommitGenError::git("Failed to allocate unique temporary git index path".to_string()))
   }

   pub fn path(&self) -> &Path {
      &self.path
   }
}

impl Drop for TempGitIndex {
   fn drop(&mut self) {
      let _ = fs::remove_file(&self.path);
      let lock_path = self.path.with_extension("lock");
      let _ = fs::remove_file(lock_path);
   }
}

pub fn git_command_with_index(index_file: &Path) -> Command {
   let mut cmd = git_command();
   cmd.env("GIT_INDEX_FILE", index_file);
   cmd
}

fn git_command_with_settings(settings: GitCommandSettings) -> Command {
   let mut cmd = Command::new("git");
   apply_git_command_overrides(&mut cmd, settings);
   cmd
}

fn diff_lines_preserve_cr(input: &str) -> impl Iterator<Item = &str> {
   input
      .split_inclusive('\n')
      .map(|line| line.strip_suffix('\n').unwrap_or(line))
}

fn list_untracked_files(dir: &str) -> Result<Vec<String>> {
   let output = git_command()
      .args(["ls-files", "--others", "--exclude-standard"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to list untracked files: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git ls-files failed: {stderr}")));
   }

   Ok(String::from_utf8_lossy(&output.stdout)
      .lines()
      .filter(|path| !path.is_empty())
      .map(str::to_string)
      .collect())
}

fn append_untracked_diff(
   mut base_diff: String,
   dir: &str,
   untracked_files: &[String],
) -> Result<String> {
   for file in untracked_files {
      let file_diff_output = git_command()
         .args([
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "/dev/null",
            file,
         ])
         .current_dir(dir)
         .output()
         .map_err(|e| CommitGenError::git(format!("Failed to diff untracked file {file}: {e}")))?;

      // `git diff --no-index` exits with 1 when files differ, which is expected.
      if file_diff_output.status.success() || file_diff_output.status.code() == Some(1) {
         let file_diff = String::from_utf8_lossy(&file_diff_output.stdout);
         let lines: Vec<&str> = diff_lines_preserve_cr(&file_diff).collect();
         if lines.len() >= 2 {
            let mode = lines
               .iter()
               .find_map(|line| line.strip_prefix("new file mode "))
               .unwrap_or("100644");
            use std::fmt::Write;
            if !base_diff.is_empty() {
               base_diff.push('\n');
            }
            writeln!(base_diff, "diff --git a/{file} b/{file}").unwrap();
            writeln!(base_diff, "new file mode {mode}").unwrap();
            base_diff.push_str("index 0000000..0000000\n");
            base_diff.push_str("--- /dev/null\n");
            writeln!(base_diff, "+++ b/{file}").unwrap();
            for line in lines
               .iter()
               .skip_while(|line| !line.starts_with("@@") && !line.starts_with("Binary files "))
            {
               base_diff.push_str(line);
               base_diff.push('\n');
            }
         }
      }
   }

   Ok(base_diff)
}

fn append_untracked_stat(mut stat: String, dir: &str, untracked_files: &[String]) -> String {
   use std::fmt::Write;

   for file in untracked_files {
      use std::fs;

      if let Ok(metadata) = fs::metadata(format!("{dir}/{file}")) {
         let lines = if metadata.is_file() {
            fs::read_to_string(format!("{dir}/{file}")).map_or(0, |content| content.lines().count())
         } else {
            0
         };

         if !stat.is_empty() && !stat.ends_with('\n') {
            stat.push('\n');
         }
         writeln!(stat, " {file} | {lines} {}", "+".repeat(lines.min(50))).unwrap();
      }
   }

   stat
}

fn append_untracked_numstat(mut numstat: String, dir: &str, untracked_files: &[String]) -> String {
   use std::fmt::Write;

   for file in untracked_files {
      use std::fs;

      let path = format!("{dir}/{file}");
      if let Ok(metadata) = fs::metadata(&path) {
         let (added, deleted) = if metadata.is_file() {
            match fs::read_to_string(&path) {
               Ok(content) => (content.lines().count().to_string(), "0".to_string()),
               Err(_) => ("-".to_string(), "-".to_string()),
            }
         } else {
            ("0".to_string(), "0".to_string())
         };

         if !numstat.is_empty() && !numstat.ends_with('\n') {
            numstat.push('\n');
         }
         writeln!(numstat, "{added}\t{deleted}\t{file}").unwrap();
      }
   }

   numstat
}

/// Detect a stale `index.lock` from git stderr and return a
/// [`CommitGenError::GitIndexLocked`] with the resolved path if found.
fn check_index_lock(stderr: &str, dir: &str) -> Option<CommitGenError> {
   if !stderr.contains("index.lock") {
      return None;
   }

   // Try to extract the exact lock path from the error message.
   // Git says: "Unable to create '/path/to/.git/index.lock': File exists."
   let lock_path = stderr
      .lines()
      .find_map(|line| {
         let start = line.find('\'')?;
         let end = line[start + 1..].find('\'')?;
         let path = &line[start + 1..start + 1 + end];
         if path.ends_with("index.lock") {
            Some(PathBuf::from(path))
         } else {
            None
         }
      })
      .unwrap_or_else(|| PathBuf::from(dir).join(".git/index.lock"));

   Some(CommitGenError::GitIndexLocked { lock_path })
}

/// Ensure the provided directory is inside a git work tree.
///
/// # Errors
/// Returns an error when the directory is not part of a git repository.
#[tracing::instrument(target = "lgit", name = "git.ensure_repo", skip_all, fields(dir))]
pub fn ensure_git_repo(dir: &str) -> Result<()> {
   let output = git_command()
      .args(["rev-parse", "--show-toplevel"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git rev-parse: {e}")))?;

   if output.status.success() {
      return Ok(());
   }

   let stderr = String::from_utf8_lossy(&output.stderr);
   if stderr.contains("not a git repository") {
      return Err(CommitGenError::git(
         "Not a git repository (or any of the parent directories): .git".to_string(),
      ));
   }

   Err(CommitGenError::git(format!("Failed to detect git repository: {stderr}")))
}

#[tracing::instrument(target = "lgit", name = "git.get_git_dir", skip_all, fields(dir))]
pub fn get_git_dir(dir: &str) -> Result<PathBuf> {
   let output = git_command()
      .args(["rev-parse", "--absolute-git-dir"])
      .current_dir(dir)
      .output()
      .map_err(|e| {
         CommitGenError::git(format!("Failed to run git rev-parse --absolute-git-dir: {e}"))
      })?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("Failed to resolve git dir: {stderr}")));
   }

   Ok(PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}

/// Get git diff based on the specified mode
#[tracing::instrument(target = "lgit", name = "git.diff", skip_all, fields(mode = ?mode, target = ?target, dir))]
pub fn get_git_diff(
   mode: &Mode,
   target: Option<&str>,
   dir: &str,
   config: &CommitConfig,
) -> Result<String> {
   let output = match mode {
      Mode::Staged => git_command()
         .args(["diff", "--cached"])
         .current_dir(dir)
         .output()
         .map_err(|e| CommitGenError::git(format!("Failed to run git diff --cached: {e}")))?,
      Mode::Commit => {
         let target = target.ok_or_else(|| {
            CommitGenError::ValidationError("--target required for commit mode".to_string())
         })?;
         let mut cmd = git_command();
         cmd.arg("show");
         if config.exclude_old_message {
            cmd.arg("--format=");
         }
         cmd.arg(target)
            .current_dir(dir)
            .output()
            .map_err(|e| CommitGenError::git(format!("Failed to run git show: {e}")))?
      },
      Mode::Unstaged => {
         // Get diff for tracked files
         let tracked_output = git_command()
            .args(["diff"])
            .current_dir(dir)
            .output()
            .map_err(|e| CommitGenError::git(format!("Failed to run git diff: {e}")))?;

         if !tracked_output.status.success() {
            let stderr = String::from_utf8_lossy(&tracked_output.stderr);
            return Err(CommitGenError::git(format!("git diff failed: {stderr}")));
         }

         let tracked_diff = String::from_utf8_lossy(&tracked_output.stdout).to_string();
         let untracked_files = list_untracked_files(dir)?;
         return append_untracked_diff(tracked_diff, dir, &untracked_files);
      },
      Mode::Compose => unreachable!("compose mode handled separately"),
   };

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("Git command failed: {stderr}")));
   }

   let diff = String::from_utf8_lossy(&output.stdout).to_string();

   if diff.trim().is_empty() {
      let mode_str = match mode {
         Mode::Staged => "staged",
         Mode::Commit => "commit",
         Mode::Unstaged => "unstaged",
         Mode::Compose => "compose",
      };
      return Err(CommitGenError::NoChanges { mode: mode_str.to_string() });
   }

   Ok(diff)
}

/// Get git diff --stat to show file-level changes summary
#[tracing::instrument(target = "lgit", name = "git.stat", skip_all, fields(mode = ?mode, target = ?target, dir))]
pub fn get_git_stat(
   mode: &Mode,
   target: Option<&str>,
   dir: &str,
   config: &CommitConfig,
) -> Result<String> {
   let output = match mode {
      Mode::Staged => git_command()
         .args(["diff", "--cached", "--stat"])
         .current_dir(dir)
         .output()
         .map_err(|e| {
            CommitGenError::git(format!("Failed to run git diff --cached --stat: {e}"))
         })?,
      Mode::Commit => {
         let target = target.ok_or_else(|| {
            CommitGenError::ValidationError("--target required for commit mode".to_string())
         })?;
         let mut cmd = git_command();
         cmd.arg("show");
         if config.exclude_old_message {
            cmd.arg("--format=");
         }
         cmd.arg("--stat")
            .arg(target)
            .current_dir(dir)
            .output()
            .map_err(|e| CommitGenError::git(format!("Failed to run git show --stat: {e}")))?
      },
      Mode::Unstaged => {
         // Get stat for tracked files
         let tracked_output = git_command()
            .args(["diff", "--stat"])
            .current_dir(dir)
            .output()
            .map_err(|e| CommitGenError::git(format!("Failed to run git diff --stat: {e}")))?;

         if !tracked_output.status.success() {
            let stderr = String::from_utf8_lossy(&tracked_output.stderr);
            return Err(CommitGenError::git(format!("git diff --stat failed: {stderr}")));
         }

         let stat = String::from_utf8_lossy(&tracked_output.stdout).to_string();
         let untracked_files = list_untracked_files(dir)?;
         return Ok(append_untracked_stat(stat, dir, &untracked_files));
      },
      Mode::Compose => unreachable!("compose mode handled separately"),
   };

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("Git stat command failed: {stderr}")));
   }

   Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[tracing::instrument(target = "lgit", name = "git.numstat", skip_all, fields(mode = ?mode, target = ?target, dir))]
pub fn get_git_numstat(
   mode: &Mode,
   target: Option<&str>,
   dir: &str,
   config: &CommitConfig,
) -> Result<String> {
   let output = match mode {
      Mode::Staged => git_command()
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
         let mut cmd = git_command();
         cmd.arg("show");
         if config.exclude_old_message {
            cmd.arg("--format=");
         }
         cmd.arg("--numstat")
            .arg(target)
            .current_dir(dir)
            .output()
            .map_err(|e| CommitGenError::git(format!("Failed to run git show --numstat: {e}")))?
      },
      Mode::Unstaged => {
         let tracked_output = git_command()
            .args(["diff", "--numstat"])
            .current_dir(dir)
            .output()
            .map_err(|e| CommitGenError::git(format!("Failed to run git diff --numstat: {e}")))?;

         if !tracked_output.status.success() {
            let stderr = String::from_utf8_lossy(&tracked_output.stderr);
            return Err(CommitGenError::git(format!("git diff --numstat failed: {stderr}")));
         }

         let numstat = String::from_utf8_lossy(&tracked_output.stdout).to_string();
         let untracked_files = list_untracked_files(dir)?;
         return Ok(append_untracked_numstat(numstat, dir, &untracked_files));
      },
      Mode::Compose => unreachable!("compose mode handled separately"),
   };

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("Git numstat command failed: {stderr}")));
   }

   Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[tracing::instrument(target = "lgit", name = "git.compose_diff", skip_all, fields(dir))]
pub fn get_compose_diff(dir: &str) -> Result<String> {
   let output = git_command()
      .args([
         "diff",
         "--no-ext-diff",
         "--no-textconv",
         "--no-color",
         "--src-prefix=a/",
         "--dst-prefix=b/",
         "HEAD",
      ])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git diff HEAD: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git diff HEAD failed: {stderr}")));
   }

   let diff = String::from_utf8_lossy(&output.stdout).to_string();
   let untracked_files = list_untracked_files(dir)?;
   let diff = append_untracked_diff(diff, dir, &untracked_files)?;

   if diff.trim().is_empty() {
      return Err(CommitGenError::NoChanges { mode: "compose".to_string() });
   }

   Ok(diff)
}

#[tracing::instrument(target = "lgit", name = "git.compose_stat", skip_all, fields(dir))]
pub fn get_compose_stat(dir: &str) -> Result<String> {
   let output = git_command()
      .args(["diff", "--no-ext-diff", "--no-textconv", "--no-color", "HEAD", "--stat"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git diff HEAD --stat: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git diff HEAD --stat failed: {stderr}")));
   }

   let stat = String::from_utf8_lossy(&output.stdout).to_string();
   let untracked_files = list_untracked_files(dir)?;
   let stat = append_untracked_stat(stat, dir, &untracked_files);

   if stat.trim().is_empty() {
      return Err(CommitGenError::NoChanges { mode: "compose".to_string() });
   }

   Ok(stat)
}

/// Execute git commit with the given message
#[allow(clippy::fn_params_excessive_bools, reason = "commit flags are naturally boolean")]
#[tracing::instrument(
   target = "lgit",
   name = "git.commit",
   skip_all,
   fields(dir, dry_run, sign, signoff, skip_hooks, amend)
)]
pub fn git_commit(
   message: &str,
   dry_run: bool,
   dir: &str,
   sign: bool,
   signoff: bool,
   skip_hooks: bool,
   amend: bool,
) -> Result<()> {
   if dry_run {
      let sign_flag = if sign { " -S" } else { "" };
      let signoff_flag = if signoff { " -s" } else { "" };
      let hooks_flag = if skip_hooks { " --no-verify" } else { "" };
      let amend_flag = if amend { " --amend" } else { "" };
      let command = format!(
         "git commit{sign_flag}{signoff_flag}{hooks_flag}{amend_flag} -m \"{}\"",
         message.replace('\n', "\\n")
      );
      if style::pipe_mode() {
         eprintln!("\n{}", style::boxed_message("DRY RUN", &command, 60));
      } else {
         println!("\n{}", style::boxed_message("DRY RUN", &command, 60));
      }
      return Ok(());
   }

   let mut args = vec!["commit"];
   if sign {
      args.push("-S");
   }
   if signoff {
      args.push("-s");
   }
   if skip_hooks {
      args.push("--no-verify");
   }
   if amend {
      args.push("--amend");
   }
   args.push("-m");
   args.push(message);

   let output = git_command()
      .args(&args)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git commit: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      let stdout = String::from_utf8_lossy(&output.stdout);
      if let Some(err) = check_index_lock(&stderr, dir) {
         return Err(err);
      }
      return Err(CommitGenError::git(format!("git commit failed: {stderr}{stdout}")));
   }

   let stdout = String::from_utf8_lossy(&output.stdout);
   if style::pipe_mode() {
      eprintln!("\n{stdout}");
      eprintln!(
         "{} {}",
         style::success(style::icons::SUCCESS),
         style::success("Successfully committed!")
      );
   } else {
      println!("\n{stdout}");
      println!(
         "{} {}",
         style::success(style::icons::SUCCESS),
         style::success("Successfully committed!")
      );
   }

   Ok(())
}

/// Execute git push
#[tracing::instrument(target = "lgit", name = "git.push", skip_all, fields(dir))]
pub fn git_push(dir: &str) -> Result<()> {
   if style::pipe_mode() {
      eprintln!("\n{}", style::info("Pushing changes..."));
   } else {
      println!("\n{}", style::info("Pushing changes..."));
   }

   let output = git_command()
      .args(["push"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git push: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      let stdout = String::from_utf8_lossy(&output.stdout);
      return Err(CommitGenError::git(format!(
         "Git push failed:\nstderr: {stderr}\nstdout: {stdout}"
      )));
   }

   let stdout = String::from_utf8_lossy(&output.stdout);
   let stderr = String::from_utf8_lossy(&output.stderr);
   if style::pipe_mode() {
      if !stdout.is_empty() {
         eprintln!("{stdout}");
      }
      if !stderr.is_empty() {
         eprintln!("{stderr}");
      }
      eprintln!(
         "{} {}",
         style::success(style::icons::SUCCESS),
         style::success("Successfully pushed!")
      );
   } else {
      if !stdout.is_empty() {
         println!("{stdout}");
      }
      if !stderr.is_empty() {
         println!("{stderr}");
      }
      println!(
         "{} {}",
         style::success(style::icons::SUCCESS),
         style::success("Successfully pushed!")
      );
   }

   Ok(())
}

/// Get the current HEAD commit hash
#[tracing::instrument(target = "lgit", name = "git.head_hash", skip_all, fields(dir))]
pub fn get_head_hash(dir: &str) -> Result<String> {
   let output = git_command()
      .args(["rev-parse", "HEAD"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to get HEAD hash: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git rev-parse HEAD failed: {stderr}")));
   }

   Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[tracing::instrument(target = "lgit", name = "git.current_head_ref", skip_all, fields(dir))]
pub fn current_head_ref(dir: &str) -> Result<String> {
   let output = git_command()
      .args(["symbolic-ref", "-q", "HEAD"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to resolve HEAD ref: {e}")))?;

   if output.status.success() {
      let refname = String::from_utf8_lossy(&output.stdout).trim().to_string();
      if !refname.is_empty() {
         return Ok(refname);
      }
   }

   Ok("HEAD".to_string())
}

#[tracing::instrument(target = "lgit", name = "git.write_real_index_tree", skip_all, fields(dir))]
pub fn write_real_index_tree(dir: &str) -> Result<String> {
   let output = git_command()
      .arg("write-tree")
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to write real index tree: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git write-tree failed: {stderr}")));
   }

   Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Commit `tree` directly, bypassing the live index.
///
/// Used when the index drifted while a message was being generated: the
/// analyzed snapshot is committed as-is, the branch (or detached HEAD)
/// advances, and the index and worktree are left untouched — anything staged
/// mid-run stays staged for a later commit. Commit hooks do not run.
///
/// Returns `Ok(None)` when `tree` is already HEAD's tree (the same content
/// was committed mid-run), `Ok(Some(hash))` otherwise.
#[tracing::instrument(
   target = "lgit",
   name = "git.commit_snapshot_tree",
   skip_all,
   fields(dir, tree, sign, signoff, amend)
)]
pub fn commit_snapshot_tree(
   message: &str,
   tree: &str,
   dir: &str,
   sign: bool,
   signoff: bool,
   amend: bool,
) -> Result<Option<String>> {
   let message = if signoff {
      append_signoff_trailer(message, dir)?
   } else {
      message.to_string()
   };

   // Unborn branch (no commits yet) has no head and no parents.
   let head = get_head_hash(dir).ok();
   let head_ref = current_head_ref(dir)?;

   let mut parents: Vec<String> = Vec::new();
   if let Some(head) = &head {
      if amend {
         parents = rev_parse_parents(head, dir)?;
      } else {
         if rev_parse_tree_of(head, dir)? == tree {
            return Ok(None);
         }
         parents.push(head.clone());
      }
   }

   let parent_refs: Vec<&str> = parents.iter().map(String::as_str).collect();
   let hash = commit_tree(tree, &parent_refs, &message, dir, sign)?;
   update_ref_checked(&head_ref, &hash, head.as_deref().unwrap_or(""), dir)?;
   Ok(Some(hash))
}

/// Tree oid of a commit-ish.
fn rev_parse_tree_of(commitish: &str, dir: &str) -> Result<String> {
   let output = git_command()
      .args(["rev-parse", &format!("{commitish}^{{tree}}")])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to resolve tree of {commitish}: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!(
         "git rev-parse {commitish}^{{tree}} failed: {stderr}"
      )));
   }

   Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Parent hashes of a commit-ish (empty for a root commit).
fn rev_parse_parents(commitish: &str, dir: &str) -> Result<Vec<String>> {
   let output = git_command()
      .args(["rev-parse", &format!("{commitish}^@")])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to resolve parents of {commitish}: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git rev-parse {commitish}^@ failed: {stderr}")));
   }

   Ok(String::from_utf8_lossy(&output.stdout)
      .lines()
      .map(str::to_string)
      .collect())
}

#[tracing::instrument(target = "lgit", name = "git.read_tree_into_index", skip_all, fields(dir, treeish, index = %index_file.display()))]
pub fn read_tree_into_index(index_file: &Path, treeish: &str, dir: &str) -> Result<()> {
   let output = git_command_with_index(index_file)
      .arg("read-tree")
      .arg(treeish)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to read tree into temporary index: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git read-tree {treeish} failed: {stderr}")));
   }

   Ok(())
}

#[tracing::instrument(target = "lgit", name = "git.write_index_tree", skip_all, fields(dir, index = %index_file.display()))]
pub fn write_index_tree(index_file: &Path, dir: &str) -> Result<String> {
   let output = git_command_with_index(index_file)
      .arg("write-tree")
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to write temporary index tree: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!(
         "git write-tree failed for temporary index: {stderr}"
      )));
   }

   Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[tracing::instrument(
   target = "lgit",
   name = "git.commit_tree",
   skip_all,
   fields(dir, parents = parents.len(), tree, sign)
)]
pub fn commit_tree(
   tree: &str,
   parents: &[&str],
   message: &str,
   dir: &str,
   sign: bool,
) -> Result<String> {
   let mut cmd = git_command();
   cmd.arg("commit-tree");
   if sign {
      cmd.arg("-S");
   }
   cmd.arg(tree);
   for parent in parents {
      cmd.arg("-p").arg(parent);
   }
   cmd.arg("-F").arg("-");

   let mut child = cmd
      .current_dir(dir)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .map_err(|e| CommitGenError::git(format!("Failed to spawn git commit-tree: {e}")))?;

   {
      let Some(mut stdin) = child.stdin.take() else {
         return Err(CommitGenError::git("Failed to open git commit-tree stdin".to_string()));
      };
      stdin
         .write_all(message.as_bytes())
         .map_err(|e| CommitGenError::git(format!("Failed to write commit message: {e}")))?;
   }

   let output = child
      .wait_with_output()
      .map_err(|e| CommitGenError::git(format!("Failed to wait for git commit-tree: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git commit-tree failed: {stderr}")));
   }

   let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
   if hash.is_empty() {
      return Err(CommitGenError::git("git commit-tree returned an empty hash".to_string()));
   }

   Ok(hash)
}

#[tracing::instrument(
   target = "lgit",
   name = "git.update_ref_checked",
   skip_all,
   fields(dir, refname, new, old)
)]
pub fn update_ref_checked(refname: &str, new: &str, old: &str, dir: &str) -> Result<()> {
   let output = git_command()
      .args(["update-ref", refname, new, old])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to update {refname}: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git update-ref failed for {refname}: {stderr}")));
   }

   Ok(())
}

#[tracing::instrument(target = "lgit", name = "git.reset_mixed", skip_all, fields(dir, treeish))]
pub fn reset_mixed_to(treeish: &str, dir: &str) -> Result<()> {
   let output = git_command()
      .args(["reset", "--mixed", "-q", treeish])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to reset index to {treeish}: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git reset --mixed failed: {stderr}")));
   }

   Ok(())
}

/// Reset the index entries for `paths` to their state in `treeish`, leaving
/// every other index entry and the worktree untouched.
///
/// Used after compose when the real index drifted mid-run: the committed
/// snapshot paths are refreshed while anything staged during the run stays
/// staged.
#[tracing::instrument(target = "lgit", name = "git.reset_paths", skip_all, fields(dir, treeish, path_count = paths.len()))]
pub fn reset_paths_to(treeish: &str, paths: &[String], dir: &str) -> Result<()> {
   if paths.is_empty() {
      return Ok(());
   }

   let output = git_command()
      .args(["reset", "-q", treeish, "--"])
      .args(paths)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to reset paths to {treeish}: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git reset {treeish} -- <paths> failed: {stderr}")));
   }

   Ok(())
}

#[tracing::instrument(target = "lgit", name = "git.append_signoff", skip_all, fields(dir))]
pub fn append_signoff_trailer(message: &str, dir: &str) -> Result<String> {
   let output = git_command()
      .args(["var", "GIT_COMMITTER_IDENT"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to read committer identity: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git var GIT_COMMITTER_IDENT failed: {stderr}")));
   }

   let ident = String::from_utf8_lossy(&output.stdout);
   let Some(end) = ident.find('>') else {
      return Err(CommitGenError::git(format!(
         "Could not parse committer identity: {}",
         ident.trim()
      )));
   };
   let signer = ident[..=end].trim();
   let trailer = format!("Signed-off-by: {signer}");
   let trimmed = message.trim_end();
   let mut signed = String::with_capacity(trimmed.len() + trailer.len() + 3);
   signed.push_str(trimmed);
   signed.push_str("\n\n");
   signed.push_str(&trailer);
   Ok(signed)
}

// === History Rewrite Operations ===

/// Get list of commit hashes to rewrite (in chronological order)
#[tracing::instrument(target = "lgit", name = "git.commit_list", skip_all, fields(dir, start_ref = ?start_ref))]
pub fn get_commit_list(start_ref: Option<&str>, dir: &str) -> Result<Vec<String>> {
   let mut args = vec!["rev-list", "--reverse"];
   let range;
   if let Some(start) = start_ref {
      range = format!("{start}..HEAD");
      args.push(&range);
   } else {
      args.push("HEAD");
   }

   let output = git_command()
      .args(&args)
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git rev-list: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git rev-list failed: {stderr}")));
   }

   let stdout = String::from_utf8_lossy(&output.stdout);
   Ok(stdout.lines().map(|s| s.to_string()).collect())
}

/// Extract complete metadata for a commit (for rewriting)
#[tracing::instrument(target = "lgit", name = "git.commit_metadata", skip_all, fields(dir, hash))]
pub fn get_commit_metadata(hash: &str, dir: &str) -> Result<CommitMetadata> {
   // Format: author_name\0author_email\0author_date\0committer_name\
   // 0committer_email\0committer_date\0message
   let format_str = "%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B";

   let info_output = git_command()
      .args(["show", "-s", &format!("--format={format_str}"), hash])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git show: {e}")))?;

   if !info_output.status.success() {
      let stderr = String::from_utf8_lossy(&info_output.stderr);
      return Err(CommitGenError::git(format!("git show failed for {hash}: {stderr}")));
   }

   let info = String::from_utf8_lossy(&info_output.stdout);
   let parts: Vec<&str> = info.splitn(7, '\0').collect();

   if parts.len() < 7 {
      return Err(CommitGenError::git(format!("Failed to parse commit metadata for {hash}")));
   }

   // Get tree hash
   let tree_output = git_command()
      .args(["rev-parse", &format!("{hash}^{{tree}}")])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to get tree hash: {e}")))?;
   let tree_hash = String::from_utf8_lossy(&tree_output.stdout)
      .trim()
      .to_string();

   // Get parent hashes
   let parents_output = git_command()
      .args(["rev-list", "--parents", "-n", "1", hash])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to get parent hashes: {e}")))?;
   let parents_line = String::from_utf8_lossy(&parents_output.stdout);
   let parent_hashes: Vec<String> = parents_line
      .split_whitespace()
      .skip(1) // First is the commit itself
      .map(|s| s.to_string())
      .collect();

   Ok(CommitMetadata {
      hash: hash.to_string(),
      author_name: parts[0].to_string(),
      author_email: parts[1].to_string(),
      author_date: parts[2].to_string(),
      committer_name: parts[3].to_string(),
      committer_email: parts[4].to_string(),
      committer_date: parts[5].to_string(),
      message: parts[6].trim().to_string(),
      parent_hashes,
      tree_hash,
   })
}

/// Check if working directory is clean
#[tracing::instrument(target = "lgit", name = "git.check_worktree_clean", skip_all, fields(dir))]
pub fn check_working_tree_clean(dir: &str) -> Result<bool> {
   let output = git_command()
      .args(["status", "--porcelain"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to check working tree: {e}")))?;

   Ok(output.stdout.is_empty())
}

/// Create timestamped backup branch
#[tracing::instrument(target = "lgit", name = "git.create_backup_branch", skip_all, fields(dir))]
pub fn create_backup_branch(dir: &str) -> Result<String> {
   use chrono::Local;

   let timestamp = Local::now().format("%Y%m%d-%H%M%S");
   let backup_name = format!("backup-rewrite-{timestamp}");

   let output = git_command()
      .args(["branch", &backup_name])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to create backup branch: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git branch failed: {stderr}")));
   }

   Ok(backup_name)
}

/// Get recent commit messages for style consistency (last N commits)
#[tracing::instrument(target = "lgit", name = "git.recent_commits", skip_all, fields(dir, count))]
pub fn get_recent_commits(dir: &str, count: usize) -> Result<Vec<String>> {
   let output = git_command()
      .args(["log", &format!("-{count}"), "--pretty=format:%s"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git log: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git log failed: {stderr}")));
   }

   let stdout = String::from_utf8_lossy(&output.stdout);
   Ok(stdout.lines().map(|s| s.to_string()).collect())
}

/// Extract common scopes from git history by parsing commit messages
#[tracing::instrument(target = "lgit", name = "git.common_scopes", skip_all, fields(dir, limit))]
pub fn get_common_scopes(dir: &str, limit: usize) -> Result<Vec<(String, usize)>> {
   let output = git_command()
      .args(["log", &format!("-{limit}"), "--pretty=format:%s"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to run git log: {e}")))?;

   if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(CommitGenError::git(format!("git log failed: {stderr}")));
   }

   let stdout = String::from_utf8_lossy(&output.stdout);
   let mut scope_counts: HashMap<String, usize> = HashMap::new();

   // Parse conventional commit format: type(scope): message
   for line in stdout.lines() {
      if let Some(scope) = extract_scope_from_commit(line) {
         *scope_counts.entry(scope).or_insert(0) += 1;
      }
   }

   // Sort by frequency (descending)
   let mut scopes: Vec<(String, usize)> = scope_counts.into_iter().collect();
   scopes.sort_by_key(|scope| std::cmp::Reverse(scope.1));

   Ok(scopes)
}

/// Extract scope from a conventional commit message
fn extract_scope_from_commit(commit_msg: &str) -> Option<String> {
   // Match pattern: type(scope): message
   let parts: Vec<&str> = commit_msg.splitn(2, ':').collect();
   if parts.len() < 2 {
      return None;
   }

   let prefix = parts[0];
   if let Some(scope_start) = prefix.find('(')
      && let Some(scope_end) = prefix.find(')')
      && scope_start < scope_end
   {
      return Some(prefix[scope_start + 1..scope_end].to_string());
   }

   None
}

/// Quantified style patterns extracted from commit history
#[derive(Debug, Clone)]
pub struct StylePatterns {
   /// Percentage of commits using scopes (0.0-100.0)
   pub scope_usage_pct: f32,
   /// Common verbs with counts (sorted by count descending)
   pub common_verbs:    Vec<(String, usize)>,
   /// Average summary length in chars
   pub avg_length:      usize,
   /// Summary length range (min, max)
   pub length_range:    (usize, usize),
   /// Percentage of commits starting with lowercase (0.0-100.0)
   pub lowercase_pct:   f32,
   /// Top scopes with counts (sorted by count descending)
   pub top_scopes:      Vec<(String, usize)>,
}

impl StylePatterns {
   /// Format patterns for prompt injection
   pub fn format_for_prompt(&self) -> String {
      let mut lines = Vec::new();

      lines.push(format!("Scope usage: {:.0}% of commits use scopes", self.scope_usage_pct));

      if !self.common_verbs.is_empty() {
         let verbs: Vec<_> = self
            .common_verbs
            .iter()
            .take(5)
            .map(|(v, c)| format!("{v} ({c})"))
            .collect();
         lines.push(format!("Common verbs: {}", verbs.join(", ")));
      }

      lines.push(format!(
         "Average length: {} chars (range: {}-{})",
         self.avg_length, self.length_range.0, self.length_range.1
      ));

      lines.push(format!("Capitalization: {:.0}% start lowercase", self.lowercase_pct));

      if !self.top_scopes.is_empty() {
         let scopes: Vec<_> = self
            .top_scopes
            .iter()
            .take(5)
            .map(|(s, c)| format!("{s} ({c})"))
            .collect();
         lines.push(format!("Top scopes: {}", scopes.join(", ")));
      }

      lines.join("\n")
   }
}

/// Extract style patterns from commit history
pub fn extract_style_patterns(commits: &[String]) -> Option<StylePatterns> {
   if commits.is_empty() {
      return None;
   }

   let mut scope_count = 0;
   let mut lowercase_count = 0;
   let mut verb_counts: HashMap<String, usize> = HashMap::new();
   let mut scope_counts: HashMap<String, usize> = HashMap::new();
   let mut lengths = Vec::new();

   for commit in commits {
      // Parse: type(scope): summary
      if let Some(colon_pos) = commit.find(':') {
         let prefix = &commit[..colon_pos];
         let summary = commit[colon_pos + 1..].trim();

         // Check for scope
         if let Some(paren_start) = prefix.find('(')
            && let Some(paren_end) = prefix.find(')')
         {
            scope_count += 1;
            let scope = &prefix[paren_start + 1..paren_end];
            *scope_counts.entry(scope.to_string()).or_insert(0) += 1;
         }

         // Check capitalization of summary
         if let Some(first_char) = summary.chars().next() {
            if first_char.is_lowercase() {
               lowercase_count += 1;
            }

            // Extract first word as verb
            let first_word = summary.split_whitespace().next().unwrap_or("");
            if !first_word.is_empty() {
               *verb_counts.entry(first_word.to_lowercase()).or_insert(0) += 1;
            }
         }

         lengths.push(summary.len());
      }
   }

   let total = commits.len();
   let scope_usage_pct = (scope_count as f32 / total as f32) * 100.0;
   let lowercase_pct = (lowercase_count as f32 / total as f32) * 100.0;

   // Sort verbs by count
   let mut common_verbs: Vec<_> = verb_counts.into_iter().collect();
   common_verbs.sort_by_key(|verb| std::cmp::Reverse(verb.1));

   // Sort scopes by count
   let mut top_scopes: Vec<_> = scope_counts.into_iter().collect();
   top_scopes.sort_by_key(|scope| std::cmp::Reverse(scope.1));

   // Calculate length stats
   let avg_length = if lengths.is_empty() {
      0
   } else {
      lengths.iter().sum::<usize>() / lengths.len()
   };
   let length_range = if lengths.is_empty() {
      (0, 0)
   } else {
      (*lengths.iter().min().unwrap_or(&0), *lengths.iter().max().unwrap_or(&0))
   };

   Some(StylePatterns {
      scope_usage_pct,
      common_verbs,
      avg_length,
      length_range,
      lowercase_pct,
      top_scopes,
   })
}

/// Rewrite git history with new commit messages
#[tracing::instrument(target = "lgit", name = "git.rewrite_history", skip_all, fields(dir, commit_count = commits.len()))]
pub fn rewrite_history(
   commits: &[CommitMetadata],
   new_messages: &[String],
   dir: &str,
) -> Result<()> {
   if commits.len() != new_messages.len() {
      return Err(CommitGenError::Other("Commit count mismatch".to_string()));
   }

   // Get current branch
   let branch_output = git_command()
      .args(["rev-parse", "--abbrev-ref", "HEAD"])
      .current_dir(dir)
      .output()
      .map_err(|e| CommitGenError::git(format!("Failed to get current branch: {e}")))?;
   let current_branch = String::from_utf8_lossy(&branch_output.stdout)
      .trim()
      .to_string();

   // Map old commit hashes to new ones
   let mut parent_map: HashMap<String, String> = HashMap::new();
   let mut new_head: Option<String> = None;

   for (idx, (commit, new_msg)) in commits.iter().zip(new_messages.iter()).enumerate() {
      // Map old parents to new parents
      let new_parents: Vec<String> = commit
         .parent_hashes
         .iter()
         .map(|old_parent| {
            parent_map
               .get(old_parent)
               .cloned()
               .unwrap_or_else(|| old_parent.clone())
         })
         .collect();

      // Build commit-tree command
      let mut cmd = git_command();
      cmd.arg("commit-tree")
         .arg(&commit.tree_hash)
         .arg("-m")
         .arg(new_msg)
         .current_dir(dir);

      for parent in &new_parents {
         cmd.arg("-p").arg(parent);
      }

      // Preserve original author/committer metadata
      cmd.env("GIT_AUTHOR_NAME", &commit.author_name)
         .env("GIT_AUTHOR_EMAIL", &commit.author_email)
         .env("GIT_AUTHOR_DATE", &commit.author_date)
         .env("GIT_COMMITTER_NAME", &commit.committer_name)
         .env("GIT_COMMITTER_EMAIL", &commit.committer_email)
         .env("GIT_COMMITTER_DATE", &commit.committer_date);

      let output = cmd
         .output()
         .map_err(|e| CommitGenError::git(format!("Failed to run git commit-tree: {e}")))?;

      if !output.status.success() {
         let stderr = String::from_utf8_lossy(&output.stderr);
         return Err(CommitGenError::git(format!(
            "commit-tree failed for {}: {}",
            commit.hash, stderr
         )));
      }

      let new_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();

      parent_map.insert(commit.hash.clone(), new_hash.clone());
      new_head = Some(new_hash);

      // Progress reporting
      if (idx + 1) % 50 == 0 {
         eprintln!("  Rewrote {}/{} commits...", idx + 1, commits.len());
      }
   }

   // Update branch to new head
   if let Some(head) = new_head {
      let update_output = git_command()
         .args(["update-ref", &format!("refs/heads/{current_branch}"), &head])
         .current_dir(dir)
         .output()
         .map_err(|e| CommitGenError::git(format!("Failed to update ref: {e}")))?;

      if !update_output.status.success() {
         let stderr = String::from_utf8_lossy(&update_output.stderr);
         return Err(CommitGenError::git(format!("git update-ref failed: {stderr}")));
      }

      let reset_output = git_command()
         .args(["reset", "--hard", &head])
         .current_dir(dir)
         .output()
         .map_err(|e| CommitGenError::git(format!("Failed to reset: {e}")))?;

      if !reset_output.status.success() {
         let stderr = String::from_utf8_lossy(&reset_output.stderr);
         return Err(CommitGenError::git(format!("git reset failed: {stderr}")));
      }
   }

   Ok(())
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn test_git_command_applies_background_feature_overrides_when_enabled() {
      let cmd =
         git_command_with_settings(GitCommandSettings { disable_git_background_features: true });
      let args: Vec<String> = cmd
         .get_args()
         .map(|arg| arg.to_string_lossy().into_owned())
         .collect();

      assert_eq!(args, vec![
         "-c".to_string(),
         "core.fsmonitor=false".to_string(),
         "-c".to_string(),
         "core.untrackedCache=false".to_string(),
      ]);
   }

   fn run_test_git(dir: &tempfile::TempDir, args: &[&str]) -> String {
      let output = git_command()
         .args(args)
         .current_dir(dir.path())
         .output()
         .unwrap_or_else(|err| panic!("git {args:?} failed to spawn: {err}"));
      assert!(
         output.status.success(),
         "git {:?} failed: {}",
         args,
         String::from_utf8_lossy(&output.stderr)
      );
      String::from_utf8_lossy(&output.stdout).to_string()
   }

   #[test]
   fn test_commit_snapshot_tree_commits_snapshot_and_keeps_drifted_staging() {
      let dir = tempfile::TempDir::new().unwrap();
      let dir_str = dir.path().to_str().unwrap();
      run_test_git(&dir, &["init"]);
      run_test_git(&dir, &["config", "user.name", "Guard Test"]);
      run_test_git(&dir, &["config", "user.email", "guard@test.local"]);
      run_test_git(&dir, &["config", "commit.gpgsign", "false"]);
      std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
      run_test_git(&dir, &["add", "a.txt"]);
      run_test_git(&dir, &["commit", "-m", "base"]);

      // The analyzed snapshot: a.txt modified and staged.
      std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
      run_test_git(&dir, &["add", "a.txt"]);
      let snapshot_tree = write_real_index_tree(dir_str).unwrap();

      // Mid-run drift: another file gets staged.
      std::fs::write(dir.path().join("b.txt"), "drift\n").unwrap();
      run_test_git(&dir, &["add", "b.txt"]);

      let hash =
         commit_snapshot_tree("feat: snapshot", &snapshot_tree, dir_str, false, false, false)
            .unwrap()
            .expect("snapshot differs from HEAD");

      // HEAD advanced to exactly the snapshot tree.
      assert_eq!(run_test_git(&dir, &["rev-parse", "HEAD"]).trim(), hash);
      assert_eq!(run_test_git(&dir, &["rev-parse", "HEAD^{tree}"]).trim(), snapshot_tree);
      assert_eq!(run_test_git(&dir, &["show", "HEAD:a.txt"]), "two\n");
      assert!(
         !run_test_git(&dir, &["ls-tree", "--name-only", "HEAD"]).contains("b.txt"),
         "drifted staging must not enter the commit"
      );

      // The drifted staging survives, staged for the next commit.
      assert_eq!(run_test_git(&dir, &["diff", "--cached", "--name-only"]).trim(), "b.txt");
      assert_eq!(std::fs::read_to_string(dir.path().join("b.txt")).unwrap(), "drift\n");

      // Re-committing the same snapshot is a no-op.
      let again =
         commit_snapshot_tree("feat: again", &snapshot_tree, dir_str, false, false, false).unwrap();
      assert_eq!(again, None);
      assert_eq!(run_test_git(&dir, &["rev-parse", "HEAD"]).trim(), hash);
   }

   #[test]
   fn test_git_command_skips_background_feature_overrides_when_disabled() {
      let cmd =
         git_command_with_settings(GitCommandSettings { disable_git_background_features: false });
      assert!(cmd.get_args().next().is_none());
   }
}
