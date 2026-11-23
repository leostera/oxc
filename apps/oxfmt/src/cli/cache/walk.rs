//! What oxfmt's walk reads besides the directories it lists, as the `ignore` crate and config
//! discovery look for files: `.gitignore`, `.editorconfig`, configs, and Git's exclude files.

use std::{
    fs,
    io::{self, BufRead},
    path::{Path, PathBuf},
};

use oxc_cache::{Inputs, Recorder, Timestamp};

/// Records a walk, from any of its threads.
#[derive(Debug)]
pub struct WalkRecorder {
    /// Where what the walk reads is recorded.
    recorder: Recorder,
    /// Config file names discovery looks for.
    config_names: Vec<&'static str>,
    /// Whether nested configs are looked for in listed directories.
    nested: bool,
}

impl WalkRecorder {
    /// A recorder for a walk started at `now`, whose config discovery looks for `config_names`,
    /// in listed directories too if `nested`.
    pub fn new(now: Timestamp, config_names: Vec<&'static str>, nested: bool) -> Self {
        Self { recorder: Recorder::new(now), config_names, nested }
    }

    /// Records a file read, or looked for.
    pub fn file(&self, path: &Path) {
        self.recorder.file(path);
    }

    /// Records a path argument: whether it's a file, a directory or missing.
    pub fn target(&self, path: &Path) {
        self.recorder.kind(path);
    }

    /// Marks the walk unkeepable: it reported a diagnostic about `path`, which a kept walk
    /// wouldn't report again.
    pub fn reported(&self, path: &Path) {
        self.recorder.mark_unrecordable(path);
    }

    /// Records a directory the walk listed, with what it reads there besides the listing:
    /// its `.gitignore`, nested configs, and Git's files for a repository rooted there.
    /// Files appearing here show in the directory's `stat`.
    pub fn listed_dir(&self, dir: &Path) {
        self.recorder.dir(dir);
        self.recorder.file_if_present(&dir.join(".gitignore"));
        if self.nested {
            for name in &self.config_names {
                self.recorder.file_if_present(&dir.join(name));
            }
        }
        if fs::symlink_metadata(dir.join(".git")).is_ok() {
            self.git(dir);
        }
    }

    /// Records a directory above the walk, where config discovery and the `ignore` crate look for files
    /// by name: each present or absent. Its listing isn't read, so its `stat` isn't an input.
    pub fn ancestor(&self, dir: &Path) {
        for name in self.config_names.iter().chain(&[".gitignore", ".editorconfig"]) {
            self.recorder.file(&dir.join(name));
        }
        if fs::symlink_metadata(dir.join(".git")).is_ok() {
            self.git(dir);
        } else {
            self.recorder.file(&dir.join(".git"));
        }
    }

    /// Git's files for a repository rooted at `dir`: `.git` (a directory, or a worktree's file
    /// and the `commondir` it points to) and `info/exclude`, as the `ignore` crate finds them.
    fn git(&self, dir: &Path) {
        let dot_git = dir.join(".git");
        self.recorder.file(&dot_git);
        if fs::symlink_metadata(&dot_git).is_ok_and(|meta| meta.is_file()) {
            if let Some(common) = self.git_commondir(&dot_git) {
                self.recorder.file(&common.join("info/exclude"));
            }
        } else {
            self.recorder.file(&dot_git.join("info/exclude"));
        }
    }

    /// The common Git directory a `.git` file points to, as the `ignore` crate resolves it
    /// (relative paths against the process's working directory, as it does).
    fn git_commondir(&self, dot_git: &Path) -> Option<PathBuf> {
        let first_line = |path: &Path| -> Option<String> {
            io::BufReader::new(fs::File::open(path).ok()?).lines().next()?.ok()
        };
        let line = first_line(dot_git)?;
        let git_dir = std::path::absolute(line.strip_prefix("gitdir: ")?).ok()?;
        let commondir_file = git_dir.join("commondir");
        self.recorder.file(&commondir_file);
        let commondir = first_line(&commondir_file)?;
        Some(if commondir.starts_with('.') { git_dir.join(commondir) } else { commondir.into() })
    }

    /// Returns what the walk read, or `None` if something couldn't be recorded.
    pub fn finish(self) -> Option<Inputs> {
        self.recorder.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn later() -> Timestamp {
        Timestamp::now() + std::time::Duration::from_secs(20)
    }

    fn record(f: impl FnOnce(&WalkRecorder)) -> Inputs {
        let recorder = WalkRecorder::new(later(), vec![], false);
        f(&recorder);
        recorder.finish().unwrap()
    }

    #[test]
    fn a_worktree_records_its_common_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let common = tmp.path().join("repo/.git");
        let git_dir = common.join("worktrees/wt");
        fs::create_dir_all(&git_dir).unwrap();
        fs::create_dir_all(common.join("info")).unwrap();
        fs::write(common.join("info/exclude"), "x\n").unwrap();
        fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        let wt = tmp.path().join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", git_dir.display())).unwrap();

        let inputs = record(|r| r.listed_dir(&wt));
        let exclude = git_dir.join("../../info/exclude");
        assert!(format!("{inputs:?}").contains(&format!("{exclude:?}")), "{inputs:#?}");
        fs::write(common.join("info/exclude"), "y\n").unwrap();
        assert!(inputs.recheck(later()).is_none());
    }
}
