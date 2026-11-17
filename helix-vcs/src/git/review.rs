//! The file at a review's base revision (fork addition, see bb-bridge/).
//!
//! While the bb bridge has a review active, [`super::get_diff_base`] compares
//! the files of that repository with the review's revision instead of `HEAD`.
//! Every place Helix computes a diff base (opening a file, `:reload`, the
//! bridge's reloads) therefore follows the review, and so do the gutter,
//! `]g`/`[g` and the bridge's inline diff, which all read Helix's own diff.
//! A base is read where Helix reads `HEAD`'s, at the same cost.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use gix::filter::plumbing::driver::apply::Delay;
use gix::ObjectId;
use helix_core::Rope;
use helix_event::runtime_local;
use imara_diff::{Algorithm, Diff, InternedInput};
use parking_lot::RwLock;

pub use imara_diff::Hunk;

/// What the active review compares against.
#[derive(Debug, Clone)]
pub struct Review {
    pub repo: Repo,
    /// `None` compares with `HEAD` as it is whenever a base is read.
    pub rev: Option<ObjectId>,
    /// Paths relative to the work tree: name now -> name at `rev`.
    pub renamed: HashMap<PathBuf, PathBuf>,
    /// New in the review: compared with an empty file when `rev` does not
    /// have them, so every line shows as added.
    pub added: HashSet<PathBuf>,
}

/// A repository, as discovered from a path inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub git_dir: PathBuf,
    pub workdir: PathBuf,
}

/// Why a review cannot be set up.
#[derive(Debug)]
pub enum OpenError {
    /// The path is not inside a git work tree.
    NoRepository(String),
    /// The repository does not have the revision.
    UnknownRevision(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NoRepository(msg) | OpenError::UnknownRevision(msg) => f.write_str(msg),
        }
    }
}

runtime_local! {
    static ACTIVE: RwLock<Option<Arc<Review>>> = RwLock::new(None);
}

/// Compare with `review` from now on (replacing any other review).
pub fn set(review: Review) {
    *ACTIVE.write() = Some(Arc::new(review));
}

/// Back to `HEAD`.
pub fn clear() {
    *ACTIVE.write() = None;
}

pub fn active() -> Option<Arc<Review>> {
    ACTIVE.read().clone()
}

/// The repository holding `path` (a file or directory, which need not
/// exist), and `rev` resolved to a commit in it (`None` for `HEAD`).
pub fn open(
    path: &Path,
    rev: Option<&str>,
    trust_full: bool,
) -> Result<(Repo, Option<ObjectId>), OpenError> {
    let dir = existing_dir(path)
        .ok_or_else(|| OpenError::NoRepository(format!("{} does not exist", path.display())))?;
    let (repo, local) = discover(&dir, trust_full).map_err(|err| {
        OpenError::NoRepository(format!(
            "{} is not in a git work tree: {err:#}",
            path.display()
        ))
    })?;
    let Some(rev) = rev else {
        return Ok((repo, None));
    };
    // a full object id; resolving names would need gix's `revision` feature
    let id = ObjectId::from_hex(rev.as_bytes())
        .ok()
        .and_then(|id| local.find_object(id).ok())
        .and_then(|object| object.peel_to_commit().ok())
        .map(|commit| commit.id)
        .ok_or_else(|| {
            OpenError::UnknownRevision(format!(
                "the repository at {} has no commit {rev}",
                repo.workdir.display()
            ))
        })?;
    Ok((repo, Some(id)))
}

/// The nearest directory at or above `path` that exists.
fn existing_dir(path: &Path) -> Option<PathBuf> {
    let mut dir = Some(path);
    while let Some(d) = dir {
        if d.is_dir() {
            return gix::path::realpath(d).ok();
        }
        dir = d.parent();
    }
    None
}

fn discover(dir: &Path, trust_full: bool) -> Result<(Repo, gix::Repository)> {
    let repo = super::open_repo(dir, trust_full)?.to_thread_local();
    let workdir = repo.workdir().context("repository has no work tree")?;
    let workdir = gix::path::realpath(workdir).unwrap_or_else(|_| workdir.to_path_buf());
    let git_dir = gix::path::realpath(repo.path()).unwrap_or_else(|_| repo.path().to_path_buf());
    Ok((Repo { git_dir, workdir }, repo))
}

/// `path` with symlinks resolved, for a path whose last components may not
/// exist (a deleted file).
pub fn resolve(path: &Path) -> Option<PathBuf> {
    let mut rest = Vec::new();
    let mut dir = path;
    loop {
        if let Ok(real) = gix::path::realpath(dir) {
            return Some(rest.iter().rev().fold(real, |p, c| p.join(c)));
        }
        rest.push(dir.file_name()?.to_owned());
        dir = dir.parent()?;
    }
}

/// Called by `get_diff_base` with the resolved path: the base of `file` in
/// the active review, or `None` when no review covers it (then `HEAD`).
pub(super) fn diff_base(file: &Path, trust_full: bool) -> Option<Result<Vec<u8>>> {
    let review = active()?;
    // the common case for files outside the review's repository: no I/O
    if !file.starts_with(&review.repo.workdir) {
        return None;
    }
    let (found, repo) = discover(file.parent()?, trust_full).ok()?;
    // a nested repository or submodule is not the review's
    if found != review.repo {
        return None;
    }
    Some(base_of(&review, &repo, file, true))
}

/// The text of `file` (an absolute path in the review's work tree, which
/// need not exist) at the review's base, as git would check it out. A file
/// the base does not have is an error, or empty when `empty_if_added` and
/// the review lists it as added.
fn base_of(
    review: &Review,
    repo: &gix::Repository,
    file: &Path,
    empty_if_added: bool,
) -> Result<Vec<u8>> {
    let rel = file.strip_prefix(&review.repo.workdir)?;
    let at_base = review.renamed.get(rel).map_or(rel, PathBuf::as_path);
    let commit = match review.rev {
        Some(id) => repo.find_object(id)?.peel_to_commit()?,
        None => match repo.head_commit() {
            Ok(commit) => commit,
            // a repository with no commits: everything listed is new
            Err(_) if empty_if_added && review.added.contains(rel) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        },
    };
    let oid = match super::find_file_in_commit(repo, &commit, &review.repo.workdir.join(at_base)) {
        Ok(oid) => oid,
        Err(_) if empty_if_added && review.added.contains(rel) => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let data = repo.find_object(oid)?.detach().data;
    to_worktree(repo, at_base, data)
}

/// The text of `path` (absolute, in the active review's work tree) at the
/// review's base, for a file the review deleted.
pub fn file_at_base(path: &Path, trust_full: bool) -> Result<Vec<u8>> {
    let review = active().context("no review is active")?;
    let file = resolve(path).context("cannot resolve the path")?;
    let repo = super::open_repo(&review.repo.workdir, trust_full)?.to_thread_local();
    base_of(&review, &repo, &file, false)
}

/// What git makes of a blob when it checks out `rel_path` (line endings,
/// attributes), as `get_diff_base` does for `HEAD`.
fn to_worktree(repo: &gix::Repository, rel_path: &Path, data: Vec<u8>) -> Result<Vec<u8>> {
    let rel_path = gix::path::try_into_bstr(rel_path)?;
    let (mut pipeline, _) = repo.filter_pipeline(None)?;
    let mut outcome = pipeline.convert_to_worktree(&data, rel_path.as_ref(), Delay::Forbid)?;
    let mut buf = Vec::with_capacity(data.len());
    outcome.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Line hunks between two texts, as Helix's diff gutter computes them.
pub fn hunks(before: &Rope, after: &Rope) -> Vec<Hunk> {
    let before = before.to_string();
    let after = after.to_string();
    let input = InternedInput::new(before.as_str(), after.as_str());
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    diff.hunks().collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::*;
    use crate::git::get_diff_base;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env("GIT_AUTHOR_NAME", "a")
            .env("GIT_AUTHOR_EMAIL", "a@example.com")
            .env("GIT_COMMITTER_NAME", "c")
            .env("GIT_COMMITTER_EMAIL", "c@example.com")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
            .env("GIT_CONFIG_VALUE_0", "false")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn write(dir: &Path, rel: &str, text: &str) -> PathBuf {
        let path = dir.join(rel);
        std::fs::write(&path, text).unwrap();
        gix::path::realpath(&path).unwrap()
    }

    fn review(dir: &Path, rev: Option<&str>, renamed: &[(&str, &str)], added: &[&str]) -> Review {
        let (repo, rev) = open(dir, rev, true).unwrap();
        Review {
            repo,
            rev,
            renamed: renamed
                .iter()
                .map(|(a, b)| (PathBuf::from(a), PathBuf::from(b)))
                .collect(),
            added: added.iter().map(PathBuf::from).collect(),
        }
    }

    // One test: the active review is one per process outside integration
    // tests, and other tests only touch files outside these repositories.
    #[test]
    fn bases_follow_the_review() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q"]);
        let a = write(dir, "a.txt", "one\n");
        write(dir, "old.txt", "moved\n");
        write(dir, "gone.txt", "deleted text\n");
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-qm", "first"]);
        let first = git(dir, &["rev-parse", "HEAD"]);
        write(dir, "a.txt", "two\n");
        git(dir, &["commit", "-qam", "second"]);
        write(dir, "a.txt", "three\n");
        git(dir, &["mv", "old.txt", "new.txt"]);
        let new = gix::path::realpath(dir.join("new.txt")).unwrap();
        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        let gone = resolve(&dir.join("gone.txt")).unwrap();
        let added = write(dir, "added.txt", "fresh\n");
        let untracked = write(dir, "untracked.txt", "loose\n");
        let other = tempfile::tempdir().unwrap();
        let outside = write(other.path(), "x.txt", "outside\n");

        // no review: HEAD
        clear();
        assert_eq!(get_diff_base(&a, true).unwrap(), b"two\n");
        assert!(get_diff_base(&new, true).is_err());

        // a commit as the base
        set(review(
            dir,
            Some(&first),
            &[("new.txt", "old.txt")],
            &["added.txt"],
        ));
        assert_eq!(get_diff_base(&a, true).unwrap(), b"one\n");
        assert_eq!(get_diff_base(&new, true).unwrap(), b"moved\n");
        assert_eq!(get_diff_base(&added, true).unwrap(), b"");
        assert!(get_diff_base(&untracked, true).is_err());
        assert_eq!(file_at_base(&gone, true).unwrap(), b"deleted text\n");
        // another repository, or none: as before
        assert!(get_diff_base(&outside, true).is_err());

        // uncommitted: HEAD, with the rename and the new file
        set(review(dir, None, &[("new.txt", "old.txt")], &["added.txt"]));
        assert_eq!(get_diff_base(&a, true).unwrap(), b"two\n");
        assert_eq!(get_diff_base(&new, true).unwrap(), b"moved\n");
        assert_eq!(get_diff_base(&added, true).unwrap(), b"");

        clear();
        assert_eq!(get_diff_base(&a, true).unwrap(), b"two\n");

        // revisions the repository does not have, and places that are not one
        let missing = "0123456789abcdef0123456789abcdef01234567";
        assert!(matches!(
            open(dir, Some(missing), true),
            Err(OpenError::UnknownRevision(_))
        ));
        assert!(matches!(
            open(dir, Some("not-a-sha"), true),
            Err(OpenError::UnknownRevision(_))
        ));
        assert!(matches!(
            open(other.path(), None, true),
            Err(OpenError::NoRepository(_))
        ));

        // a repository with no commits: what is listed as added is all new
        let empty = tempfile::tempdir().unwrap();
        git(empty.path(), &["init", "-q"]);
        let fresh = write(empty.path(), "f.txt", "new\n");
        set(review(empty.path(), None, &[], &["f.txt"]));
        assert_eq!(get_diff_base(&fresh, true).unwrap(), b"");
        assert!(matches!(
            open(empty.path(), Some(&first), true),
            Err(OpenError::UnknownRevision(_))
        ));

        // CRLF: the base is what git would check out
        let crlf = tempfile::tempdir().unwrap();
        git(crlf.path(), &["init", "-q"]);
        write(crlf.path(), ".gitattributes", "*.txt text eol=crlf\n");
        let c = write(crlf.path(), "c.txt", "a\r\nb\r\n");
        git(crlf.path(), &["add", "-A"]);
        git(crlf.path(), &["commit", "-qm", "crlf"]);
        let rev = git(crlf.path(), &["rev-parse", "HEAD"]);
        set(review(crlf.path(), Some(&rev), &[], &[]));
        assert_eq!(get_diff_base(&c, true).unwrap(), b"a\r\nb\r\n");
        clear();
    }

    #[test]
    fn hunks_match_the_gutter() {
        let hunks = hunks(&Rope::from("a\nb\nc\n"), &Rope::from("a\nB\nc\nd\n"));
        assert_eq!(hunks.len(), 2);
        assert_eq!(
            (hunks[0].before.clone(), hunks[0].after.clone()),
            (1..2, 1..2)
        );
        assert_eq!(
            (hunks[1].before.clone(), hunks[1].after.clone()),
            (3..3, 3..4)
        );
    }
}
