use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Untracked,
}

#[derive(Clone, Debug)]
pub(crate) struct FileChange {
    pub(crate) path: String,
    pub(crate) status: FileStatus,
}

#[derive(Clone, Debug)]
pub(crate) struct ChangesState {
    pub(crate) workdir: PathBuf,
    pub(crate) staged: Vec<FileChange>,
    pub(crate) unstaged: Vec<FileChange>,
    pub(crate) untracked: Vec<String>,
    pub(crate) selected_index: usize,
    pub(crate) diff_content: Vec<String>,
    pub(crate) diff_scroll: usize,
}

fn parse_status_char(c: char) -> Option<FileStatus> {
    match c {
        'M' => Some(FileStatus::Modified),
        'A' => Some(FileStatus::Added),
        'D' => Some(FileStatus::Deleted),
        'R' => Some(FileStatus::Renamed),
        'C' => Some(FileStatus::Copied),
        _ => None,
    }
}

impl ChangesState {
    /// Create a new ChangesState for the given working directory.
    pub(crate) fn new(workdir: &Path) -> Self {
        let mut state = Self {
            workdir: workdir.to_path_buf(),
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked: Vec::new(),
            selected_index: 0,
            diff_content: Vec::new(),
            diff_scroll: 0,
        };
        state.refresh();
        state
    }

    /// Refresh git status by running `git status --porcelain=v1`.
    pub(crate) fn refresh(&mut self) {
        self.staged.clear();
        self.unstaged.clear();
        self.untracked.clear();

        let output = Command::new("git")
            .arg("status")
            .arg("--porcelain=v1")
            .current_dir(&self.workdir)
            .output();

        let output = match output {
            Ok(o) => o,
            Err(_) => return,
        };

        if !output.status.success() {
            return;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        for line in stdout.lines() {
            if line.len() < 3 {
                continue;
            }

            let bytes = line.as_bytes();
            let x = bytes[0] as char; // staged status
            let y = bytes[1] as char; // unstaged status
            let path_str = &line[3..];

            // For renames, the path looks like "old -> new"; take the new path.
            let path = if let Some(arrow_pos) = path_str.find(" -> ") {
                path_str[arrow_pos + 4..].to_string()
            } else {
                path_str.to_string()
            };

            // Untracked files
            if x == '?' && y == '?' {
                self.untracked.push(path);
                continue;
            }

            // Staged changes (column 1)
            if x != ' ' && x != '?' {
                if let Some(status) = parse_status_char(x) {
                    self.staged.push(FileChange {
                        path: path.clone(),
                        status,
                    });
                }
            }

            // Unstaged changes (column 2)
            if y != ' ' && y != '?' {
                if let Some(status) = parse_status_char(y) {
                    self.unstaged.push(FileChange {
                        path: path.clone(),
                        status,
                    });
                }
            }
        }
    }

    /// Get the total number of items (staged + unstaged + untracked) for navigation.
    pub(crate) fn total_items(&self) -> usize {
        self.staged.len() + self.unstaged.len() + self.untracked.len()
    }

    /// Get the file path at the given flat index across all sections.
    /// Returns (path, is_staged).
    pub(crate) fn file_at(&self, index: usize) -> Option<(&str, bool)> {
        let staged_len = self.staged.len();
        let unstaged_len = self.unstaged.len();

        if index < staged_len {
            Some((&self.staged[index].path, true))
        } else if index < staged_len + unstaged_len {
            Some((&self.unstaged[index - staged_len].path, false))
        } else if index < staged_len + unstaged_len + self.untracked.len() {
            Some((&self.untracked[index - staged_len - unstaged_len], false))
        } else {
            None
        }
    }

    /// Load the diff for the currently selected file.
    pub(crate) fn load_diff(&mut self) {
        self.diff_content.clear();
        self.diff_scroll = 0;

        let staged_len = self.staged.len();
        let unstaged_len = self.unstaged.len();
        let index = self.selected_index;

        if index < staged_len {
            // Staged file: git diff --cached
            let path = self.staged[index].path.clone();
            let output = Command::new("git")
                .args(["diff", "--cached", &path])
                .current_dir(&self.workdir)
                .output();

            if let Ok(o) = output {
                let text = String::from_utf8_lossy(&o.stdout);
                self.diff_content = text.lines().map(|l| l.to_string()).collect();
            }
        } else if index < staged_len + unstaged_len {
            // Unstaged file: git diff
            let path = self.unstaged[index - staged_len].path.clone();
            let output = Command::new("git")
                .args(["diff", &path])
                .current_dir(&self.workdir)
                .output();

            if let Ok(o) = output {
                let text = String::from_utf8_lossy(&o.stdout);
                self.diff_content = text.lines().map(|l| l.to_string()).collect();
            }
        } else if index < staged_len + unstaged_len + self.untracked.len() {
            // Untracked file: read contents directly
            let path = &self.untracked[index - staged_len - unstaged_len];
            let full_path = self.workdir.join(path);
            if let Ok(contents) = std::fs::read_to_string(&full_path) {
                self.diff_content = contents.lines().map(|l| format!("+{}", l)).collect();
            }
        }
    }

    /// Stage a file. Runs `git add {path}`.
    pub(crate) fn stage_file(&mut self, path: &str) {
        let _ = Command::new("git")
            .args(["add", path])
            .current_dir(&self.workdir)
            .output();
        self.refresh();
    }

    /// Unstage a file. Runs `git restore --staged {path}`.
    pub(crate) fn unstage_file(&mut self, path: &str) {
        let _ = Command::new("git")
            .args(["restore", "--staged", path])
            .current_dir(&self.workdir)
            .output();
        self.refresh();
    }

    /// Create a commit with the given message.
    pub(crate) fn commit(&self, message: &str) -> Result<String, String> {
        let output = Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(&self.workdir)
            .output()
            .map_err(|e| format!("Failed to run git commit: {}", e))?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).to_string())
        }
    }

    /// Push to remote.
    pub(crate) fn push(&self) -> Result<String, String> {
        let output = Command::new("git")
            .arg("push")
            .current_dir(&self.workdir)
            .output()
            .map_err(|e| format!("Failed to run git push: {}", e))?;

        if output.status.success() {
            // git push often writes progress to stderr even on success
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            Ok(format!("{}{}", stdout, stderr))
        } else {
            Err(String::from_utf8_lossy(&output.stderr).to_string())
        }
    }

    /// Navigate selection up.
    pub(crate) fn select_prev(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
            self.load_diff();
        }
    }

    /// Navigate selection down.
    pub(crate) fn select_next(&mut self) {
        let total = self.total_items();
        if total > 0 && self.selected_index < total - 1 {
            self.selected_index += 1;
            self.load_diff();
        }
    }
}
