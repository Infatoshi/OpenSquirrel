use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) struct WorktreeInfo {
    pub(crate) repo_root: PathBuf,
    pub(crate) worktree_path: PathBuf,
    pub(crate) branch: String,
    pub(crate) base_branch: String,
}

impl WorktreeInfo {
    /// Create a new git worktree. Returns WorktreeInfo or error string.
    /// Runs `git worktree add ~/.osq/worktrees/{branch} -b {branch}` from repo_root.
    pub(crate) fn create(repo_root: &Path, branch: &str) -> Result<Self, String> {
        let base_branch = current_branch(repo_root).unwrap_or_else(|| "main".to_string());

        let worktrees_dir = worktrees_base_dir();
        std::fs::create_dir_all(&worktrees_dir)
            .map_err(|e| format!("Failed to create worktrees directory: {e}"))?;

        let worktree_path = worktrees_dir.join(branch);

        if worktree_path.exists() {
            return Err(format!(
                "Worktree path already exists: {}",
                worktree_path.display()
            ));
        }

        let output = Command::new("git")
            .args([
                "worktree",
                "add",
                &worktree_path.to_string_lossy(),
                "-b",
                branch,
            ])
            .current_dir(repo_root)
            .output()
            .map_err(|e| format!("Failed to run git worktree add: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git worktree add failed: {stderr}"));
        }

        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            worktree_path,
            branch: branch.to_string(),
            base_branch,
        })
    }

    /// Remove this worktree. Runs teardown script if present, then `git worktree remove --force`.
    pub(crate) fn remove(&self) -> Result<(), String> {
        // Run teardown script before removal
        run_teardown_script(&self.repo_root, &self.worktree_path, &self.branch);

        let output = Command::new("git")
            .args([
                "worktree",
                "remove",
                "--force",
                &self.worktree_path.to_string_lossy(),
            ])
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| format!("Failed to run git worktree remove: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git worktree remove failed: {stderr}"));
        }

        Ok(())
    }
}

/// Return the base directory for all worktrees: ~/.osq/worktrees/
fn worktrees_base_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME environment variable not set");
    PathBuf::from(home).join(".osq").join("worktrees")
}

/// Get the current branch name of the repo at the given path.
fn current_branch(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()?;

    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if branch.is_empty() || branch == "HEAD" {
            None
        } else {
            Some(branch)
        }
    } else {
        None
    }
}

/// Check if a path is inside a git repository.
pub(crate) fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// List all worktrees for a given repo root.
pub(crate) fn list_worktrees(repo_root: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("Failed to run git worktree list: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree list failed: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let paths: Vec<String> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|s| s.to_string())
        .collect();

    Ok(paths)
}

/// Run a hook script (.osq/setup.sh or teardown.sh) with standard env vars.
/// Returns captured output lines for display in agent transcript.
fn run_hook_script(
    script_name: &str,
    repo_root: &Path,
    worktree_path: &Path,
    workspace_name: &str,
) -> Vec<String> {
    let script_path = repo_root.join(".osq").join(script_name);
    if !script_path.is_file() {
        return Vec::new();
    }

    let result = Command::new("bash")
        .arg(&script_path)
        .current_dir(worktree_path)
        .env("OPENSQUIRREL_WORKSPACE_NAME", workspace_name)
        .env("OPENSQUIRREL_ROOT_PATH", repo_root)
        .env("OPENSQUIRREL_WORKTREE_PATH", worktree_path)
        .output();

    match result {
        Ok(output) => {
            let mut lines = Vec::new();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            for line in stdout.lines() {
                lines.push(line.to_string());
            }
            for line in stderr.lines() {
                lines.push(format!("[stderr] {line}"));
            }
            if !output.status.success() {
                lines.push(format!(
                    "[{}] exited with status {}",
                    script_name, output.status
                ));
            }
            lines
        }
        Err(e) => {
            vec![format!("Failed to run {script_name}: {e}")]
        }
    }
}

/// Run .osq/setup.sh if it exists in the repo root.
/// Environment: OPENSQUIRREL_WORKSPACE_NAME, OPENSQUIRREL_ROOT_PATH, OPENSQUIRREL_WORKTREE_PATH
pub(crate) fn run_setup_script(
    repo_root: &Path,
    worktree_path: &Path,
    workspace_name: &str,
) -> Vec<String> {
    run_hook_script("setup.sh", repo_root, worktree_path, workspace_name)
}

/// Run .osq/teardown.sh if it exists in the repo root.
pub(crate) fn run_teardown_script(
    repo_root: &Path,
    worktree_path: &Path,
    workspace_name: &str,
) -> Vec<String> {
    run_hook_script("teardown.sh", repo_root, worktree_path, workspace_name)
}
