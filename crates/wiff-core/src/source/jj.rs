//! jj source-control backend: diff capture, base-ruleset resolution, and forge
//! operations for repositories managed by [jj](https://jj-vcs.github.io/jj/).

use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use async_trait::async_trait;
use tracing::trace;

use crate::base_resolve::{RevisionResolver, resolve_base};
use crate::base_ruleset::{BaseRuleset, parse_ruleset};
use crate::error::{Error, Result};
use crate::identity::ScmType;
use crate::record::{ChangeId, RevisionId, ScmSource, SourceKind, TipRule};
use crate::session_id::SessionId;
use crate::source::git::GitRepo;
use crate::source::{
    CapturedDiff, DiffSource, FetchSource, HeadBranch, Remote, ScmRepo, TrackingBranch,
};

/// Lines of unchanged context jj includes around each diff hunk, matching the
/// `GIT_CONTEXT_LINES` of the git backend. The window is large enough that a
/// hunk usually contains its whole file, regardless of which backend produced
/// the diff.
const JJ_CONTEXT_LINES: u32 = 3000;

/// jj's virtual root commit ID (all zeros). In jj 0.42+, `trunk()` resolves to
/// this when no trunk is configured rather than returning empty. See
/// <https://jj-vcs.github.io/jj/latest/revsets/#built-in-functions> for the
/// `trunk()` semantics. Any revset result equal to this should be treated as
/// "not meaningfully resolved".
const JJ_ROOT_COMMIT: &str = "0000000000000000000000000000000000000000";

/// Revision resolution, diff production, and forge operations against a jj
/// repository. Contains the subprocess plumbing that both the base-ruleset
/// resolver and [`JjSource::capture`](crate::source::DiffSource::capture) run
/// through.
#[derive(Debug, Clone)]
pub struct JjRepo {
    repo_root: PathBuf,
}

impl JjRepo {
    /// Creates a handle to the jj repository rooted at `repo_root`.
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    /// Returns a `GitRepo` pointed at the repo root, for forge operations
    /// (fetch, pin, publish) that must speak git's ref and network protocol.
    /// Only valid for colocated jj+git workspaces where `.git` exists alongside
    /// `.jj`. Returns an error for non-colocated workspaces.
    fn git_repo_for_forge(&self) -> Result<GitRepo> {
        if self.repo_root.join(".git").exists() {
            Ok(GitRepo::new(self.repo_root.clone()))
        } else {
            Err(Error::Repo(
                "forge operations require a colocated jj+git workspace \
                 (.git must exist at the workspace root)"
                    .to_string(),
            ))
        }
    }

    /// Run a jj subcommand under the repo root and return its output on
    /// success.
    async fn jj<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        let output = self.spawn(&args).await?;
        if !output.status.success() {
            let subcommand = args
                .first()
                .map(|arg| arg.to_string_lossy().into_owned())
                .unwrap_or_default();
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Repo(format!(
                "jj {subcommand} failed ({}): {}",
                output.status,
                stderr.trim()
            )));
        }
        Ok(output)
    }

    /// Run a jj subcommand and return its raw output whatever the exit status,
    /// failing only when the process could not be started.
    async fn spawn(&self, args: &[OsString]) -> Result<Output> {
        let mut command = Command::new("jj");
        command
            .arg("-R")
            .arg(&self.repo_root)
            .arg("--no-pager")
            .args(args);
        command.stdin(Stdio::null());
        unsafe {
            // A new session starts without a controlling terminal. jj's
            // credential prompt opens /dev/tty directly rather than using
            // stdin/stdout, and that open fails immediately in a session
            // lacking one, instead of the prompt waiting forever for input.
            command.pre_exec(|| {
                if nix::libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let rendered: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        trace!(repo = %self.repo_root.display(), args = ?rendered, "running jj");
        let output = tokio::process::Command::from(command)
            .output()
            .await
            .map_err(|source| Error::Repo(format!("could not run jj: {source}")))?;
        trace!(
            args = ?rendered,
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "jj finished"
        );
        Ok(output)
    }

    /// Run a jj query that prints one commit hash per line, returning the
    /// first. Exit code 1 means "nothing matched" and yields `Ok(None)`. Any
    /// other nonzero exit is a jj failure and returns `Err`.
    ///
    /// Caveat: jj exits 1 both when a revset matches nothing and when a revset
    /// fails to parse, and this function has no other signal to tell the two
    /// apart. A malformed revset yields `Ok(None)` here rather than an `Err`.
    /// Give this only revsets you know are syntactically valid (e.g.
    /// `bookmarks(exact:"...")`).
    async fn rev_query(&self, args: &[OsString]) -> Result<Option<RevisionId>> {
        let output = self.spawn(args).await?;
        match output.status.code() {
            Some(0) => {}
            Some(1) => return Ok(None),
            _ => {
                let subcommand = args
                    .first()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(Error::Repo(format!(
                    "jj {subcommand} failed ({}): {}",
                    output.status,
                    stderr.trim()
                )));
            }
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|source| Error::Repo(format!("jj printed a non-UTF-8 revision: {source}")))?;
        // Take only the first non-empty line: a revset that matches several
        // commits (e.g. `parents()` on a merge) yields one hash per line.
        match text.lines().find(|l| !l.trim().is_empty()) {
            Some(line) => Ok(Some(RevisionId(line.trim().to_string()))),
            None => Ok(None),
        }
    }

    /// Resolve `revset` to a commit hash via `jj log`.
    async fn resolve_revset(&self, revset: &str) -> Result<Option<RevisionId>> {
        self.rev_query(&[
            "log".into(),
            "--ignore-working-copy".into(),
            "--limit".into(),
            "1".into(),
            "--no-graph".into(),
            "-r".into(),
            revset.into(),
            "-T".into(),
            "commit_id ++ \"\\n\"".into(),
        ])
        .await
    }

    /// Returns the first local (non-tracking) bookmark on `@`.
    async fn current_bookmark(&self) -> Result<Option<String>> {
        let output = self
            .jj([
                "log",
                "--ignore-working-copy",
                "--no-graph",
                "-r",
                "@",
                "-T",
                "separate(\"\\n\", local_bookmarks)",
            ])
            .await?;
        let text = String::from_utf8(output.stdout)
            .map_err(|source| Error::Repo(format!("jj printed a non-UTF-8 bookmark: {source}")))?;
        Ok(text
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|s| s.trim().to_string()))
    }

    /// Get the diff of `@` against `base` in git unified-diff format.
    async fn diff_working_copy(&self, base: &RevisionId) -> Result<String> {
        let context = format!("--context={JJ_CONTEXT_LINES}");
        let output = self
            .jj([
                "diff",
                &context,
                "--from",
                base.as_str(),
                "--git",
                "--to",
                "@",
            ])
            .await?;
        String::from_utf8(output.stdout)
            .map_err(|source| Error::Source(format!("jj diff was not valid UTF-8: {source}")))
    }

    /// Get the diff from `base` to `tip` in git unified-diff format.
    async fn diff_range(&self, base: &RevisionId, tip: &RevisionId) -> Result<String> {
        let context = format!("--context={JJ_CONTEXT_LINES}");
        let output = self
            .jj([
                "diff",
                &context,
                "--from",
                base.as_str(),
                "--git",
                "--to",
                tip.as_str(),
            ])
            .await?;
        String::from_utf8(output.stdout)
            .map_err(|source| Error::Source(format!("jj diff was not valid UTF-8: {source}")))
    }
}

/// Reports the branch state of the jj workspace at `repo_root`: `On` with a
/// pseudo-ref naming the local bookmark on `@`, or `Detached`/`Unknown` when
/// there is none to report or the state could not be read.
pub fn head_branch(repo_root: &Path) -> HeadBranch {
    let Ok(output) = std::process::Command::new("jj")
        .arg("-R")
        .arg(repo_root)
        .arg("--no-pager")
        .args([
            "log",
            "--ignore-working-copy",
            "--no-graph",
            "-r",
            "@",
            "-T",
            "separate(\"\\n\", local_bookmarks)",
        ])
        .stdin(Stdio::null())
        .output()
    else {
        return HeadBranch::Unknown;
    };
    if !output.status.success() {
        return HeadBranch::Unknown;
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return HeadBranch::Unknown;
    };
    match text.lines().find(|l| !l.trim().is_empty()) {
        Some(bookmark) => HeadBranch::On(format!("refs/heads/{}", bookmark.trim())),
        None => HeadBranch::Detached,
    }
}

#[async_trait]
impl RevisionResolver for JjRepo {
    fn scm(&self) -> ScmType {
        ScmType::Jujutsu
    }

    async fn resolve_ref(&self, name: &str) -> Result<Option<RevisionId>> {
        self.resolve_revset(name).await
    }

    async fn trunk(&self) -> Result<Option<RevisionId>> {
        // jj's `trunk()` revset resolves the configured trunk branch. See
        // https://jj-vcs.github.io/jj/latest/revsets/#built-in-functions. In jj
        // 0.42+, it always resolves, defaulting to the all-zeros root commit
        // (`JJ_ROOT_COMMIT`) rather than returning empty when no trunk is
        // configured.
        if let Some(rev) = self.resolve_revset("trunk()").await?
            && rev.as_str() != JJ_ROOT_COMMIT
        {
            return Ok(Some(rev));
        }
        // The root commit above means jj has not configured a trunk. Fall back
        // to the two most common default branch names.
        for name in ["main", "master"] {
            if let Some(rev) = self.resolve_revset(name).await? {
                return Ok(Some(rev));
            }
        }
        Ok(None)
    }

    async fn upstream(&self) -> Result<Option<RevisionId>> {
        // Resolve the remote-tracking bookmark for the current change.
        let Some(bookmark) = self.current_bookmark().await? else {
            // A remote-tracking query needs the local bookmark's name. `@` has
            // none here.
            return Ok(None);
        };
        for remote in ["origin", "upstream"] {
            // jj expresses a remote-tracking bookmark as `<bookmark>@<remote>`.
            if let Some(rev) = self.resolve_revset(&format!("{bookmark}@{remote}")).await? {
                return Ok(Some(rev));
            }
        }
        Ok(None)
    }

    async fn parent(&self, rev: &RevisionId) -> Result<Option<RevisionId>> {
        match self
            .resolve_revset(&format!("parents({})", rev.as_str()))
            .await?
        {
            Some(parent) => Ok(Some(parent)),
            // `parents()` finds none for a root commit. Fall back to jj's
            // virtual root, the commit every revision in the repo descends
            // from.
            None => self.empty().await,
        }
    }

    async fn merge_base(&self, rev: &RevisionId, tip: &RevisionId) -> Result<Option<RevisionId>> {
        self.rev_query(&[
            "log".into(),
            "-r".into(),
            format!("ancestors({}) & ancestors({})", rev.as_str(), tip.as_str()).into(),
            "--no-graph".into(),
            "--ignore-working-copy".into(),
            "--limit".into(),
            "1".into(),
            "-T".into(),
            "commit_id ++ \"\\n\"".into(),
        ])
        .await
    }

    async fn native(&self, expr: &str) -> Result<Option<RevisionId>> {
        self.resolve_revset(expr).await
    }

    async fn empty(&self) -> Result<Option<RevisionId>> {
        // jj's all-zeros root commit is the correct "before everything" base:
        // `jj diff --from 0000...0000 --to @` works, while the git empty-tree
        // SHA1 (a tree object, not a commit) does not resolve in jj revsets.
        Ok(Some(RevisionId(JJ_ROOT_COMMIT.to_string())))
    }
}

#[async_trait]
impl ScmRepo for JjRepo {
    async fn fetch_pinned(&self, source: &FetchSource, session: SessionId) -> Result<RevisionId> {
        self.git_repo_for_forge()?
            .fetch_pinned(source, session)
            .await
    }

    async fn fetch_base(&self, source: &FetchSource, session: SessionId) -> Result<RevisionId> {
        self.git_repo_for_forge()?.fetch_base(source, session).await
    }

    async fn remotes(&self) -> Result<Vec<Remote>> {
        let output = self.jj(["git", "remote", "list"]).await?;
        let text = String::from_utf8(output.stdout)
            .map_err(|source| Error::Repo(format!("jj printed a non-UTF-8 remote: {source}")))?;
        let mut remotes = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((name, url)) = line.split_once(": ") else {
                return Err(Error::Repo(format!(
                    "jj git remote list printed an unreadable line: {line:?}"
                )));
            };
            remotes.push(Remote {
                name: name.trim().to_string(),
                url: url.trim().to_string(),
            });
        }
        Ok(remotes)
    }

    async fn working_tree_is_clean(&self) -> Result<bool> {
        // jj always commits the working copy into `@`. "Clean" means `@` has no
        // changes relative to its parent, which `jj diff` shows as empty.
        let output = self.jj(["diff", "--git"]).await?;
        let text = String::from_utf8(output.stdout)
            .map_err(|source| Error::Repo(format!("jj printed a non-UTF-8 diff: {source}")))?;
        Ok(text.trim().is_empty())
    }

    async fn remote_branch(&self, remote: &str, branch: &str) -> Result<Option<RevisionId>> {
        // jj represents remote tracking bookmarks as `<branch>@<remote>`.
        self.resolve_revset(&format!("{branch}@{remote}")).await
    }

    async fn current_upstream(&self) -> Result<Option<TrackingBranch>> {
        let Some(bookmark) = self.current_bookmark().await? else {
            return Ok(None);
        };
        for remote in ["origin", "upstream"] {
            if self
                .resolve_revset(&format!("{bookmark}@{remote}"))
                .await?
                .is_some()
            {
                return Ok(Some(TrackingBranch {
                    remote: remote.to_string(),
                    branch: bookmark,
                }));
            }
        }
        Ok(None)
    }

    async fn publish_branch(&self, remote: &str, branch: &str, commit: &RevisionId) -> Result<()> {
        // git_repo_for_forge errors when .git is absent (i.e. the repo is not
        // colocated).  Propagating that error with `?` here reports the
        // problem with a clear message before any jj subprocess runs, rather
        // than letting the user see a cryptic "jj git push failed".  The
        // GitRepo handle itself is not needed here.  Only the check matters.
        let _ = self.git_repo_for_forge()?;
        self.jj(["bookmark", "set", branch, "-r", commit.as_str()])
            .await?;
        self.jj(["git", "push", "-b", branch, "--remote", remote])
            .await?;
        Ok(())
    }

    async fn remove_pins(&self, session: SessionId) -> Result<()> {
        self.git_repo_for_forge()?.remove_pins(session).await
    }
}

/// A [`DiffSource`] over a jj repository. Capture re-resolves the base and tip
/// from scratch each time rather than fixing them at construction. A session
/// reviewing a branch or change picks up the commit it currently points to,
/// even after a rebase or amend moves it.
#[derive(Debug, Clone)]
pub struct JjSource {
    repo: JjRepo,
    /// How the base of the reviewed range is resolved, re-evaluated on every
    /// capture.
    base: BaseRuleset,
    /// How the tip of the reviewed range is resolved, re-evaluated on every
    /// capture.
    tip: TipRule,
}

impl JjSource {
    /// Creates a source reviewing the working copy (`@`) against `base`.
    pub fn working_copy(repo_root: impl Into<PathBuf>, base: BaseRuleset) -> Self {
        Self {
            repo: JjRepo::new(repo_root),
            base,
            tip: TipRule::WorkingCopy,
        }
    }

    /// Creates a source reviewing `base` against whatever commit `tip`
    /// resolves to.
    pub fn revision(repo_root: impl Into<PathBuf>, base: BaseRuleset, tip: TipRule) -> Self {
        Self {
            repo: JjRepo::new(repo_root),
            base,
            tip,
        }
    }

    /// Build a source reviewing `change` against `base`, classifying it as a
    /// [`Ref`](TipRule::Ref), [`ChangeId`](TipRule::ChangeId), or
    /// [`Pinned`](TipRule::Pinned) tip. Bookmark lookup takes priority over
    /// the change-ID heuristic: an all-lowercase bookmark name like `master`
    /// still resolves to `Ref`. Errors if `change` resolves to nothing.
    pub async fn change(
        repo_root: impl Into<PathBuf>,
        base: BaseRuleset,
        change: String,
    ) -> Result<Self> {
        let repo = JjRepo::new(repo_root);
        let tip = if repo
            .resolve_revset(&format!(
                "bookmarks(exact:\"{}\")",
                // jj bookmark names cannot contain `"` or `\`. Escaping only
                // `"` is complete: a bookmark name cannot contain a literal `\`
                // for this replace to collide with.
                change.replace('"', "\\\"")
            ))
            .await?
            .is_some()
        {
            // Storing the name as TipRule::Ref lets it re-resolve to the
            // current commit of the bookmark on every wiff recapture.
            TipRule::Ref { name: change }
        } else if looks_like_change_id(&change) {
            // A jj change ID tracks the logical change across rewrites.
            TipRule::ChangeId {
                id: ChangeId(change),
            }
        } else {
            // A bare commit hash or other expression is resolved and pinned.
            let revision = repo
                .resolve_revset(&change)
                .await?
                .ok_or_else(|| Error::Source(format!("'{change}' did not resolve to a commit")))?;
            TipRule::Pinned { revision }
        };
        Ok(Self { repo, base, tip })
    }

    /// Builds the base ruleset that pins the review at the parent of the
    /// working copy (`@-`), the jj equivalent of `git diff HEAD`. Falls back
    /// to the empty tree for a repo whose first commit has no parent.
    pub async fn pinned_base_at_head(repo_root: impl Into<PathBuf>) -> Result<BaseRuleset> {
        let repo = JjRepo::new(repo_root);
        Ok(match repo.resolve_revset("@-").await? {
            Some(parent) if parent.as_str() != JJ_ROOT_COMMIT => BaseRuleset::pinned(&parent),
            _ => BaseRuleset::empty(),
        })
    }

    /// Resolve the tip rule to the commit the diff runs up to.
    async fn resolve_tip(&self) -> Result<RevisionId> {
        match &self.tip {
            TipRule::Index => Err(Error::Source(
                "jj has no staging area; use `wiff new` without `--cached`".to_string(),
            )),
            TipRule::WorkingCopy => self
                .repo
                .resolve_revset("@")
                .await?
                .ok_or_else(|| Error::Source("jj could not resolve @".to_string())),
            // Detection above matched this name with `bookmarks(exact:"...")`;
            // here it is instead handed to the revset resolver as a bare
            // name. The two are equivalent for a real bookmark name, which is
            // the only name this arm is reached with.
            TipRule::Ref { name } => self
                .repo
                .resolve_revset(name)
                .await?
                .ok_or_else(|| Error::Source(format!("'{name}' did not resolve to a commit"))),
            TipRule::ChangeId { id } => {
                self.repo.resolve_revset(id.as_str()).await?.ok_or_else(|| {
                    Error::Source(format!("change '{id}' did not resolve to a commit"))
                })
            }
            TipRule::Pinned { revision } => self
                .repo
                .resolve_revset(revision.as_str())
                .await?
                .ok_or_else(|| {
                    Error::Source(format!("revision '{revision}' did not resolve to a commit"))
                }),
        }
    }
}

/// Heuristic: a jj change ID uses only lowercase letters (no digits, no
/// uppercase).  Commit hashes are hex (digits + a-f), bookmark names typically
/// contain slashes or digits, and revsets contain operators.
fn looks_like_change_id(s: &str) -> bool {
    // jj change IDs are random lowercase-letter strings. Common branch names
    // (main, dev, feat) are short. Require at least 5 chars to avoid treating
    // short bookmark names as change IDs.
    s.len() >= 5 && s.chars().all(|c| c.is_ascii_lowercase())
}

#[async_trait]
impl DiffSource for JjSource {
    async fn capture(&self) -> Result<CapturedDiff> {
        let ruleset = parse_ruleset(self.base.as_str())?;
        let tip = self.resolve_tip().await?;
        let resolved = resolve_base(&ruleset, &tip, &self.repo)
            .await?
            .ok_or_else(|| {
                Error::Source(format!(
                    "base ruleset '{}' did not resolve to any commit",
                    self.base
                ))
            })?;
        let base = resolved.revision;
        let (text, head_revision) = match &self.tip {
            TipRule::WorkingCopy => (self.repo.diff_working_copy(&base).await?, None),
            TipRule::Index => unreachable!("Index is rejected in resolve_tip"),
            _ => (self.repo.diff_range(&base, &tip).await?, Some(tip.clone())),
        };
        let branch_hint = match &self.tip {
            TipRule::WorkingCopy => self
                .repo
                .current_bookmark()
                .await?
                .map(|b| format!("refs/heads/{b}")),
            _ => None,
        };
        Ok(CapturedDiff {
            text,
            source: SourceKind::Scm(ScmSource {
                scm: ScmType::Jujutsu,
                base: self.base.clone(),
                tip: self.tip.clone(),
                branch_hint,
            }),
            base_revision: Some(base),
            base_tip_relative: resolved.tip_relative,
            head_revision,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base_ruleset::BaseRuleset;
    use crate::record::{RevisionId, SourceKind, TipRule};
    use crate::source::DiffSource;

    /// Run `jj` with `args` in `repo`, asserting success.  HOME is set to the
    /// parent of `repo`.  jj writes its user config under HOME, keeping that
    /// config outside the git repo and out of its commits.
    fn jj(repo: &Path, args: &[&str]) -> std::process::Output {
        let home = repo.parent().unwrap_or(repo);
        let output = std::process::Command::new("jj")
            .env_clear()
            .env("HOME", home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("JJ_USER", "wez")
            .env("JJ_EMAIL", "wez@example.com")
            .args(["-R", &repo.display().to_string()])
            .arg("--no-pager")
            .args(args)
            .output()
            .expect("run jj");
        assert!(
            output.status.success(),
            "jj {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// Run `jj` in `repo` and return its trimmed stdout.
    fn jj_out(repo: &Path, args: &[&str]) -> String {
        String::from_utf8(jj(repo, args).stdout)
            .expect("utf-8")
            .trim()
            .to_string()
    }

    /// Initialize a new jj repo in `dir` with git backend.  Does NOT pass `-R`
    /// since the repo doesn't exist yet.  HOME is set to the parent of `dir`.
    /// jj writes its user config under HOME, keeping that config outside the
    /// git repo.
    fn jj_init(dir: &Path) {
        let home = dir.parent().unwrap_or(dir);
        let output = std::process::Command::new("jj")
            .env_clear()
            .env("HOME", home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("JJ_USER", "wez")
            .env("JJ_EMAIL", "wez@example.com")
            .current_dir(dir)
            .args(["--no-pager", "git", "init", "--colocate"])
            .output()
            .expect("run jj");
        assert!(
            output.status.success(),
            "jj git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Blank the variable `index <old>..<new>` blob hashes in `text`, the only
    /// part of a captured patch that differs between runs.  The result can be
    /// asserted whole.
    fn stable(text: &str) -> String {
        text.lines()
            .map(|line| {
                if line.starts_with("index ") {
                    "index HASHES".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn looks_like_change_id_accepts_lowercase_letter_strings() {
        assert!(looks_like_change_id("kkmpptxz"));
        assert!(looks_like_change_id("qouvsmrv"));
        assert!(!looks_like_change_id("abc123")); // digits disqualify it
        // Below the 5-char minimum that keeps short bookmark names like this
        // from being misread as change IDs.
        assert!(!looks_like_change_id("main"));
        assert!(!looks_like_change_id("")); // shorter still
        // The slash is not an ASCII lowercase letter, failing the
        // all-lowercase-letters check.
        assert!(!looks_like_change_id("refs/heads/main"));
    }

    #[tokio::test]
    async fn a_working_copy_source_captures_the_diff() {
        let repo = tempfile::tempdir().expect("tempdir");
        jj_init(repo.path());
        std::fs::write(repo.path().join("f.txt"), "alpha\nbeta\n").expect("write");
        jj(repo.path(), &["new", "-m", "add f"]);

        // Record @- (the parent commit = "add f") for the base_revision assertion.
        let parent_sha = RevisionId(jj_out(
            repo.path(),
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
        ));

        // Pin the base at @- (parent of the working copy).
        let base = JjSource::pinned_base_at_head(repo.path())
            .await
            .expect("base");
        std::fs::write(repo.path().join("g.txt"), "gamma\n").expect("write g");

        let captured = JjSource::working_copy(repo.path(), base)
            .capture()
            .await
            .expect("capture");
        let expected = "\
diff --git a/g.txt b/g.txt
new file mode 100644
index HASHES
--- /dev/null
+++ b/g.txt
@@ -0,0 +1,1 @@
+gamma";
        assert_eq!(stable(&captured.text), expected.to_string());
        // The base is pinned at @- (parent). base_revision reports that commit
        // directly, with no further resolution.
        assert_eq!(captured.base_revision, Some(parent_sha));
        assert!(!captured.base_tip_relative);
        assert!(matches!(captured.source, SourceKind::Scm(_)));
        if let SourceKind::Scm(src) = &captured.source {
            assert_eq!(src.scm, ScmType::Jujutsu);
            assert!(matches!(src.tip, TipRule::WorkingCopy));
            assert!(src.branch_hint.is_none());
        }
        assert!(captured.head_revision.is_none());
    }

    #[tokio::test]
    async fn a_revision_source_captures_the_commit_patch() {
        let repo = tempfile::tempdir().expect("tempdir");
        jj_init(repo.path());
        std::fs::write(repo.path().join("f.txt"), "alpha\nbeta\n").expect("write");
        jj(repo.path(), &["new", "-m", "add f"]);
        jj(repo.path(), &["bookmark", "set", "main", "-r", "@-"]);

        // Diff the first commit against the empty tree.
        let head = RevisionId(jj_out(
            repo.path(),
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
        ));
        let captured = JjSource::revision(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            TipRule::Pinned {
                revision: head.clone(),
            },
        )
        .capture()
        .await
        .expect("capture");

        let expected = "\
diff --git a/f.txt b/f.txt
new file mode 100644
index HASHES
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,2 @@
+alpha
+beta";
        assert_eq!(stable(&captured.text), expected.to_string());
        assert_eq!(captured.head_revision, Some(head));
        // parent(@) on the first commit in jj resolves to the virtual root
        // commit (all zeros) described on `JJ_ROOT_COMMIT`.
        assert_eq!(
            captured.base_revision,
            Some(RevisionId(JJ_ROOT_COMMIT.to_string()))
        );
        // parent(@) is tip-relative: it tracks whichever commit is reviewed.
        assert!(captured.base_tip_relative);
    }

    #[tokio::test]
    async fn a_staged_tip_is_rejected_with_a_clear_error() {
        let repo = tempfile::tempdir().expect("tempdir");
        jj_init(repo.path());
        let error = JjSource::revision(repo.path(), BaseRuleset::empty(), TipRule::Index)
            .capture()
            .await
            .expect_err("Index must be rejected");
        assert!(
            error.to_string().contains("jj has no staging area"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn trunk_resolves_via_bookmark_fallback() {
        let repo = tempfile::tempdir().expect("tempdir");
        jj_init(repo.path());
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        jj(repo.path(), &["new", "-m", "first"]);
        jj(repo.path(), &["bookmark", "set", "main", "-r", "@-"]);

        let main_sha = RevisionId(jj_out(
            repo.path(),
            &["log", "-r", "main", "--no-graph", "-T", "commit_id"],
        ));

        let jj_repo = JjRepo::new(repo.path());
        // This repo has not configured a trunk. jj 0.42+ resolves trunk() to
        // the root commit (`JJ_ROOT_COMMIT`) rather than returning empty in
        // that case, which this assertion confirms.
        let trunk_revset = jj_repo.resolve_revset("trunk()").await.expect("revset");
        assert_eq!(
            trunk_revset,
            Some(RevisionId(JJ_ROOT_COMMIT.to_string())),
            "trunk() should resolve to root commit (all zeros) when unconfigured"
        );
        // With trunk() confirmed unconfigured above, this exercises the
        // bookmark-name fallback in JjRepo::trunk rather than its trunk()
        // path.
        let trunk = jj_repo.trunk().await.expect("trunk");
        assert_eq!(
            trunk,
            Some(main_sha),
            "trunk should resolve via 'main' bookmark"
        );
    }

    #[tokio::test]
    async fn empty_returns_the_jj_root_commit() {
        let repo = tempfile::tempdir().expect("tempdir");
        jj_init(repo.path());
        let jj_repo = JjRepo::new(repo.path());
        let empty = jj_repo.empty().await.expect("empty");
        assert_eq!(empty, Some(RevisionId(JJ_ROOT_COMMIT.to_string())));
    }

    #[tokio::test]
    async fn working_copy_in_a_root_only_repo_captures_the_diff() {
        // The very first change in a fresh repo: @- is the virtual root commit,
        // which pinned_base_at_head treats as having no real parent. It falls
        // back to BaseRuleset::empty(), which resolves to JJ_ROOT_COMMIT
        // rather than git's empty-tree SHA1. jj diff can diff from
        // JJ_ROOT_COMMIT, a real commit in its history, but not from git's
        // empty-tree SHA1, a tree object that jj's revsets do not resolve.
        let repo = tempfile::tempdir().expect("tempdir");
        jj_init(repo.path());
        std::fs::write(repo.path().join("f.txt"), "hello\n").expect("write");
        let base = JjSource::pinned_base_at_head(repo.path())
            .await
            .expect("base");
        let captured = JjSource::working_copy(repo.path(), base)
            .capture()
            .await
            .expect("capture");
        assert!(captured.text.contains("+hello"), "expected diff output");
        assert_eq!(
            captured.base_revision,
            Some(RevisionId(JJ_ROOT_COMMIT.to_string()))
        );
    }

    #[tokio::test]
    async fn change_with_a_bookmark_name_records_a_ref_tip() {
        // Bookmark names that pass the change-id heuristic (all lowercase, >=5
        // chars) must not be misclassified. Bookmark existence takes priority.
        let repo = tempfile::tempdir().expect("tempdir");
        jj_init(repo.path());
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        jj(repo.path(), &["new", "-m", "first"]);
        // "master" passes looks_like_change_id but is a bookmark.
        jj(repo.path(), &["bookmark", "set", "master", "-r", "@-"]);
        let head = RevisionId(jj_out(
            repo.path(),
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
        ));
        // `resolve_base` substitutes the commit "master" resolves to for `@`
        // in the "parent(@)" ruleset below, not jj's own working-copy commit.
        let source = JjSource::change(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            "master".to_string(),
        )
        .await
        .expect("change");
        assert!(
            matches!(source.tip, TipRule::Ref { ref name } if name == "master"),
            "expected TipRule::Ref, got {:?}",
            source.tip
        );
        // Capture through the Ref tip exercises resolve_tip's Ref arm, which
        // was previously dead code (the old exact:"..." detection always
        // returned None).
        let captured = source.capture().await.expect("capture through Ref tip");
        assert!(captured.text.contains("+alpha"), "expected diff output");
        assert_eq!(captured.head_revision, Some(head));
        // "master" is the only real commit in this repo. jj created it directly
        // on top of the virtual root commit described on `JJ_ROOT_COMMIT`. That
        // root is its parent and only ancestor.
        assert_eq!(
            captured.base_revision,
            Some(RevisionId(JJ_ROOT_COMMIT.to_string()))
        );
        assert!(captured.base_tip_relative);
    }

    #[tokio::test]
    async fn change_with_a_non_heuristic_bookmark_name_records_a_ref_tip() {
        // Bookmarks whose names do NOT pass looks_like_change_id (e.g. because
        // they contain a hyphen or digit) still resolve as TipRule::Ref.
        let repo = tempfile::tempdir().expect("tempdir");
        jj_init(repo.path());
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        jj(repo.path(), &["new", "-m", "first"]);
        jj(repo.path(), &["bookmark", "set", "feature-1", "-r", "@-"]);
        let head = RevisionId(jj_out(
            repo.path(),
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
        ));
        // `resolve_base` substitutes the commit "feature-1" resolves to for
        // `@` in the "parent(@)" ruleset below, not jj's own working-copy
        // commit.
        let source = JjSource::change(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            "feature-1".to_string(),
        )
        .await
        .expect("change");
        assert!(
            matches!(source.tip, TipRule::Ref { ref name } if name == "feature-1"),
            "expected TipRule::Ref, got {:?}",
            source.tip
        );
        let captured = source.capture().await.expect("capture");
        assert!(captured.text.contains("+alpha"), "expected diff output");
        assert_eq!(captured.head_revision, Some(head));
        // "feature-1" is the only real commit in this repo. jj created it
        // directly on top of the virtual root commit described on
        // `JJ_ROOT_COMMIT`. That root is its parent and only ancestor.
        assert_eq!(
            captured.base_revision,
            Some(RevisionId(JJ_ROOT_COMMIT.to_string()))
        );
        assert!(captured.base_tip_relative);
    }

    #[tokio::test]
    async fn change_with_a_change_id_records_a_change_id_tip() {
        let repo = tempfile::tempdir().expect("tempdir");
        jj_init(repo.path());
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        jj(repo.path(), &["new", "-m", "first"]);
        // Record the change ID and commit ID of @- (the "first" commit).
        let change_id = jj_out(
            repo.path(),
            &["log", "-r", "@-", "--no-graph", "-T", "change_id"],
        );
        let head = RevisionId(jj_out(
            repo.path(),
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
        ));
        // Change IDs are all-lowercase letters, which is what
        // looks_like_change_id checks for.
        assert!(
            looks_like_change_id(&change_id),
            "jj change id should pass heuristic"
        );
        // `resolve_base` substitutes the commit `change_id` resolves to for
        // `@` in the "parent(@)" ruleset below, not jj's own working-copy
        // commit.
        let source = JjSource::change(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            change_id.clone(),
        )
        .await
        .expect("change");
        assert!(
            matches!(source.tip, TipRule::ChangeId { ref id } if id.as_str() == change_id),
            "expected TipRule::ChangeId, got {:?}",
            source.tip
        );
        // Capture through the ChangeId tip to exercise resolve_tip's ChangeId arm.
        let captured = source
            .capture()
            .await
            .expect("capture through ChangeId tip");
        assert!(captured.text.contains("+alpha"), "expected diff output");
        assert_eq!(captured.head_revision, Some(head));
        // `change_id` is the only real commit in this repo. jj created it
        // directly on top of the virtual root commit described on
        // `JJ_ROOT_COMMIT`. That root is its parent and only ancestor.
        assert_eq!(
            captured.base_revision,
            Some(RevisionId(JJ_ROOT_COMMIT.to_string()))
        );
        assert!(captured.base_tip_relative);
    }

    #[tokio::test]
    async fn change_with_a_commit_hash_records_a_pinned_tip() {
        let repo = tempfile::tempdir().expect("tempdir");
        jj_init(repo.path());
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        jj(repo.path(), &["new", "-m", "first"]);
        let commit_sha = jj_out(
            repo.path(),
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
        );
        // `resolve_base` substitutes the commit `commit_sha` names for `@` in
        // the "parent(@)" ruleset below, not jj's own working-copy commit.
        let source = JjSource::change(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            commit_sha.clone(),
        )
        .await
        .expect("change");
        assert!(
            matches!(source.tip, TipRule::Pinned { ref revision } if revision.as_str() == commit_sha),
            "expected TipRule::Pinned, got {:?}",
            source.tip
        );
        let captured = source.capture().await.expect("capture through Pinned tip");
        assert!(captured.text.contains("+alpha"), "expected diff output");
        assert_eq!(captured.head_revision, Some(RevisionId(commit_sha.clone())));
        // `commit_sha` is the only real commit in this repo. jj created it
        // directly on top of the virtual root commit described on
        // `JJ_ROOT_COMMIT`. That root is its parent and only ancestor.
        assert_eq!(
            captured.base_revision,
            Some(RevisionId(JJ_ROOT_COMMIT.to_string()))
        );
        assert!(captured.base_tip_relative);
    }

    #[tokio::test]
    async fn forge_operations_fail_with_a_clear_error_on_non_colocated_repos() {
        let repo = tempfile::tempdir().expect("tempdir");
        jj_init(repo.path());
        // Simulate a non-colocated workspace by removing the .git directory
        // that jj git init --colocate creates.
        std::fs::remove_dir_all(repo.path().join(".git")).expect("remove .git");
        let jj_repo = JjRepo::new(repo.path());
        // A placeholder revision: `publish_branch` rejects the missing `.git`
        // in `git_repo_for_forge` before it would resolve or use the revision.
        let err = jj_repo
            .publish_branch("origin", "main", &RevisionId("0".repeat(40)))
            .await
            .expect_err("should fail without .git");
        assert!(
            err.to_string().contains("colocated"),
            "expected colocation error, got: {err}"
        );
    }
}
